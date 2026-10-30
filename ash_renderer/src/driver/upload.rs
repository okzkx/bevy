//! 上传编排:消费 [`InstanceLedger`] 的**待盘点行**(新增/换柄的增量,非全量快照),
//! 把未驻留的 `Assets<Mesh>` 去重转换、未驻留的贴图经材质五槽去重建图,合成
//! **一个批次**送进 GPU(网格进池、贴图进 dedicated memory)。零裸 Vulkan 调用——
//! 资源操作全部走 [`crate::render`] 的 pool/uploader/images 出口。
//!
//! 时序:PostUpdate 账本对账 → **Last:准备+上传提交** →
//! 同帧图形提交(图形侧等票据=draw 挂 ticket 信号量等待,见 render/frame/frame_draw.rs)。
//! 本系统住 `Last`,由 [`crate::driver::host`] 的 `draw_frame` 链成序(先上传
//! 后画)。失败按统一错误通道分流(判定线=冒泡与否只看是否提前 return):
//! 资产未到货/转换拒绝 = 循环内逐项降级,就地 warn 重试不进通道;提前 return
//! 的 Vulkan/账本失败 = Tier②(`.or_fatal`/`ok_or_fatal` 挂负载 `?` 冒泡,
//! 负载只写失败含义;提交失败不发布票据,池内 bump 游标随
//! 下次容量保证自然前移,不留指向未上传数据的账本行)。
//!
//! 增量纪律:发现侧只把"新增/换柄"的行放进脏集——盘点只过脏行,
//! 静态场景稳态脏集为空,**零盘点零上传**;盘点时已全驻留的行就地清标(空批
//! 不提交)。资产身份去重跨行跨帧:同一身份只有驻留缓存查无才转换上传。
//! 贴图身份从脏行的 material 柄出发去 `Assets<StandardMaterial>`
//! 解引用五槽,再按资产身份去重。资产未到货/被拒的行保持脏标下帧重试;
//! 未到货有显式账面
//! ([`AssetReadiness`]:pending 身份 → 等待帧数),加载失败/转换拒绝进
//! failed 集不再重试,失败腿且无其余 pending 腿的行盘点时出队。
//!
//! 贴图上传形态(显存机制篇判定线):staging 字节 → `vkCmdCopyBufferToImage`
//! (重排由 copy 引擎完成),前后置布局屏障与跨族 release 在 uploader 图像段;
//! `ImageSampler::Default` 按官方 ImagePlugin 全局默认(linear)解析,格式角色
//! 以 `texture_descriptor.format` 的 Srgb 后缀为准,不二次推断。

use std::collections::HashSet;

use bevy::{
    app::OnAppExitSystems,
    asset::AssetId,
    ecs::system::SystemParam,
    image::Image,
    pbr::StandardMaterial,
    prelude::*,
};

use ash_macros::system;

use crate::{
    common::syntax::{OnceLog, OptionTierExt, ResultTierExt},
    scene::{
        anchor_level, AssetInvalidations, AssetReadiness, AssetRetirements, InstanceLedger,
        LodDemand, LodGroups, ResidencySet,
    },
    render::{
        pack_object_record, sampler_key, FramePool, GpuImage, ImageCache, InstanceTables, MeshPool,
        StagingCopy, StagingImageCopy, UploadBatch, Uploader, MAX_LOD_LEVELS, OBJECT_RECORD_SIZE,
    },
};

/// StandardMaterial 的贴图槽清单(五槽)。
const TEXTURE_SLOTS: fn(&StandardMaterial) -> [&Option<Handle<Image>>; 5] = |m| {
    [
        &m.base_color_texture,
        &m.metallic_roughness_texture,
        &m.occlusion_texture,
        &m.normal_map_texture,
        &m.emissive_texture,
    ]
};

/// 上传编排插件:资源插入在 driver 的 Vulkan 初始化链完成(需要 Device),
/// 本插件只把 [`flush_uploads`] 系统挂进 `Last`:排在驻留 tick([`ResidencySet`])
/// 之后、draw_frame 之前("驻留动作进同帧上传账、先上传后画")。
pub struct AshUploadPlugin;

impl Plugin for AshUploadPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Last,
            flush_uploads
                .run_if(resource_exists::<MeshPool>)
                .run_if(resource_exists::<ImageCache>)
                .run_if(resource_exists::<crate::render::BindlessTables>)
                .run_if(resource_exists::<AssetInvalidations>)
                .run_if(resource_exists::<AssetReadiness>)
                .run_if(resource_exists::<AssetRetirements>)
                .after(ResidencySet)
                .before(crate::driver::host::draw_frame)
                .before(OnAppExitSystems),
        );
    }
}

/// 上传状态(跨帧 `Local`):warn 去重 + "增量链首次清账"一次性收账。
#[derive(Default)]
pub(crate) struct UploadState {
    /// 转换拒绝已 warn 过的 mesh 身份(一次一资产,不刷屏)。
    warned: HashSet<AssetId<Mesh>>,
    /// 规格映射拒绝已 warn 过的贴图身份(一次一资产,不刷屏)。
    warned_images: HashSet<AssetId<Image>>,
    /// "脏集首次清空且账本非空"报一次(稳态零盘点零上传的基准证据)。
    settled_logged: OnceLog,
}

/// mesh 腿三态：完成(驻留且不待换)/失败(身份在 failed 集,不再重试)/待到货
/// (保持脏标跨帧重试,身份进本批)。"不待换" = 无失效凭据——持凭据的已驻留资产
/// 走换条目重传(swap-on-commit,驻留不留空洞)。
#[derive(Clone, Copy, PartialEq, Eq)]
enum MeshLeg {
    Done,
    Failed,
    Pending,
}

impl MeshLeg {
    fn of(
        id: AssetId<Mesh>,
        pool: &MeshPool,
        invalidations: &AssetInvalidations,
        readiness: &AssetReadiness,
    ) -> Self {
        if pool.resident(id).is_some() && !invalidations.mesh_pending(id) {
            Self::Done
        } else if readiness.mesh_failed(id) {
            Self::Failed
        } else {
            Self::Pending
        }
    }

    /// 行级记账：失败腿/待到货腿置位对应旗标，返回"身份需进本批"（待到货）。
    fn tally(self, pending_leg: &mut bool, failed_leg: &mut bool) -> bool {
        match self {
            Self::Done => false,
            Self::Failed => {
                *failed_leg = true;
                false
            }
            Self::Pending => {
                *pending_leg = true;
                true
            }
        }
    }
}

/// flush_uploads 的只读参数束(SystemParam):成排的 Res 收进一个参数束,
/// 让系统签名保持精简。
#[derive(SystemParam)]
pub(crate) struct UploadData<'w> {
    meshes: Res<'w, Assets<Mesh>>,
    std_materials: Res<'w, Assets<StandardMaterial>>,
    image_assets: Res<'w, Assets<Image>>,
    /// 账本(独占:消费脏行、写上传计量)。
    ledger: ResMut<'w, InstanceLedger>,
    /// 资产失效凭据(读侧产物:换内容成功的身份在此销账,未成功的保留
    /// 供行级重试)。
    invalidations: ResMut<'w, AssetInvalidations>,
    /// 异步就绪账面:pending/failed 身份账,本系统独占写 pending 侧
    /// (mark/arrive/expel/reconcile),失败读侧只写 failed 侧。
    readiness: ResMut<'w, AssetReadiness>,
    /// 卸载回收账面:事件读侧记下的待回收身份,本系统独占消费
    /// (take + 驻留条目出账)。
    retirements: ResMut<'w, AssetRetirements>,
    /// GPU 常驻绘制数据(对象表/材质槽/停尸房):实例槽分配与记录换槽的
    /// 写点独占本系统(行盘点期间);环的写入在帧循环(draw_frame)。
    instance_tables: ResMut<'w, InstanceTables>,
    /// LOD 组登记处（只读）：档位盘点与到货反查——AshLedgerPlugin
    /// init_resource 恒在账。
    lod_groups: Res<'w, LodGroups>,
    /// 需求档位图（只读）：residency_tick 同帧发布（Last 排序先于本
    /// 系统）——位图点名才盘点，非需求档不重传（上传按需）。
    lod_demand: Res<'w, LodDemand>,
    /// overlay 图集账面:旧代停尸房并入本排空点;overlay 未装时
    /// 缺席 = None(no-op)。
    atlas: Option<ResMut<'w, crate::render::AtlasGpu>>,
    ctx: Res<'w, crate::render::Context>,
    /// 帧槽:池迁移销毁旧池前的"图形最后使用"等待经此接线。
    frames: Res<'w, FramePool>,
}

/// flush_uploads 本体:账本脏行 → 去重(mesh 直读 + 贴图经材质五槽)→ 转换/建图
/// → 容量保证(可能触发维护迁移)→ 合批提交 → 驻留登记。空批次(无新资产)不提交。
///
/// 失败不设壳:提前 return 的失败 `or_fatal`/`ok_or_fatal` 挂负载 `?` 直冒泡——
/// Err 上交统一错误通道(tier_handler 分流,fatal_gate 写 AppExit),负载只写
/// 失败含义,本系统只管控制流。
#[system]
pub(crate) fn flush_uploads(
    data: UploadData,
    mut pool: ResMut<MeshPool>,
    mut image_cache: ResMut<ImageCache>,
    mut tables: ResMut<crate::render::BindlessTables>,
    mut uploader: ResMut<Uploader>,
    mut state: Local<UploadState>,
) -> Result<(), BevyError> {
    let UploadData {
        ref meshes,
        ref std_materials,
        ref image_assets,
        mut ledger,
        mut invalidations,
        mut readiness,
        mut retirements,
        mut instance_tables,
        ref lod_groups,
        ref lod_demand,
        mut atlas,
        ref ctx,
        ref frames,
    } = data;
    // —— 0) 停尸房排空(内容重传/卸载/替换的统一排空点,含 overlay 图集旧代):
    // 上一轮换下的旧图/旧网格区间/待回收描述符槽等"双条件"(图形最后使用=
    // 全部帧槽 fence + 上传完成=停尸房票据)后销毁/归还——三条销毁路径各自
    // 空房判定,非空才等待。排空失败按 Tier② 致命退出——or_fatal 走统一
    // 错误通道由 fatal_gate 写 AppExit,停尸房不会重试。
    image_cache
        .drain_graveyard(&uploader, &mut || frames.wait_all_inflight())
        .or_fatal("贴图停尸房排空失败(帧侧等待),上传链无法继续")?;
    pool.drain_graveyard(&uploader, &mut || frames.wait_all_inflight())
        .or_fatal("网格回收停尸房排空失败(帧侧等待),上传链无法继续")?;
    let drained_slots = tables
        .drain_retired_slots(&mut || frames.wait_all_inflight())
        .or_fatal("描述符槽回收排空失败(帧侧等待),上传链无法继续")?;
    // —— 0.4) 对象表停尸房排空 + 槽位交接账消费:行删除/换绑换下的旧实例槽
    // (update_ledger 交接而来)进停尸房,等"图形最后使用"后排空归 free list
    // ——与网格/贴图停尸房同一节拍,共用同一次帧侧等待。排空先于本帧槽位
    // 分配:复用槽的旧记录读者已全部收口,新记录写入无竞态。
    for slot in ledger.take_freed_slots() {
        instance_tables.retire_object_slot(slot);
    }
    instance_tables
        .drain_graveyard(&mut || frames.wait_all_inflight())
        .or_fatal("对象表停尸房排空失败(帧侧等待),上传链无法继续")?;
    // —— 0.5) 卸载回收消费:事件读侧记下的待回收身份出账——驻留条目
    // 进停尸房(网格区间/旧图)、描述符槽进待回收账(下帧步骤 0 排空后才真正
    // 复用/销毁),失效凭据一并出清(资产已不在容器,凭据是僵尸账)。**先于脏行
    // 盘点**——同帧"卸载又重载同身份"的行要看到驻留已出账(查无 → 进本批),
    // 不能被旧条目的"驻留命中"误判完成。
    let mut reclaimed = 0u64;
    let retired_meshes = retirements.take_meshes();
    for id in &retired_meshes {
        if pool.retire(*id).is_some() {
            reclaimed += 1;
        }
        invalidations.consume_mesh(*id);
        // 组装联动：驻留出账 → 以该身份为锚的行可绘性可能翻暂缓
        //（基准锚直引 + 保底档锚经组反查），反查引用行标记组装重估。
        for entity in ledger.rows_with_mesh(*id) {
            ledger.mark_assembly_stale(entity);
        }
        if let Some(base) = lod_groups.base_of(*id) {
            for entity in ledger.rows_with_mesh(base) {
                ledger.mark_assembly_stale(entity);
            }
        }
    }
    for id in retirements.take_images() {
        if image_cache.retire(id, &mut tables) {
            reclaimed += 1;
        }
        invalidations.consume_image(id);
    }
    retirements.stats.resident_reclaimed_total += reclaimed;
    retirements.stats.slots_reclaimed_total += drained_slots as u64;
    if reclaimed > 0 || drained_slots > 0 {
        info!(
            "卸载回收出账:驻留条目 {reclaimed} 个(mesh+image,区间/旧图进停尸房下帧排空),\
             描述符槽归还 {drained_slots} 条"
        );
    }
    // —— 0.6) overlay 图集旧代排空:整传换下的旧 GpuImage 走同一
    // 双条件(帧侧等待 + 上传票据);overlay 未装时 atlas = None,空房 no-op。
    // 旧代槽位已在整传帧交回表,由下一轮步骤 0 的 drain_retired_slots 等 GPU 后归还。
    if let Some(atlas) = atlas.as_mut() {
        atlas
            .drain_graveyard(&uploader, &mut || frames.wait_all_inflight())
            .or_fatal("图集停尸房排空失败(双条件等待),上传链无法继续")?;
    }
    // —— 1) 脏行盘点(上传清单 = 账本脏行):每行查其资产身份的驻留
    // 状态——全驻留的行就地清标;有未驻留身份的行保持脏标(重试),身份进本批。
    // 判定为"查无**或失效**才传"——失效凭据放行
    // "已驻留但内容变了"的资产,走换条目重传(swap-on-commit,驻留不留空洞)。
    // mesh:脏行直读;贴图:material 柄 → 五槽解引用(材质未到货 = 该行保持脏,
    // 下帧重试,与 mesh 异步到货同口径)。
    let mut seen = HashSet::new();
    // (身份, 柄, 是否换条目重传)——第三位凭据决定 commit 走登记还是替换
    let mut fresh: Vec<(AssetId<Mesh>, Handle<Mesh>, bool)> = Vec::new();
    // LOD 组档位身份去重集（登记组的档位资产随引用行首次盘点一并
    // 进本批——档位不被实体直接引用，账本脏行扫描是它们唯一的上传触发点）
    let mut lod_seen = HashSet::new();
    let mut img_seen = HashSet::new();
    let mut fresh_images: Vec<(AssetId<Image>, Handle<Image>, bool)> = Vec::new();
    let mut materials_pending = 0usize;
    let mut settled_rows: Vec<Entity> = Vec::new();
    // 失败腿出队的行数(失败且无其余 pending 腿的行清脏标,防僵尸重试;
    // mesh 未驻留 = DrawList 本就暂缓该行,失败贴图 = fallback 白图照画)
    let mut failed_settled_rows = 0usize;
    let dirty_entities: Vec<Entity> = ledger.dirty_entities().collect();
    // 对象记录待写集（行扫描收集,排批段并入 staging）:(实例槽, 记录 48B)。
    let mut staged_records: Vec<(u32, [u8; OBJECT_RECORD_SIZE as usize])> = Vec::new();
    for entity in dirty_entities {
        // 组装联动：被盘点的行其可绘输入（槽位/驻留/材质在容器）本帧可能翻面，
        // 逐行标记组装重估（draw_frame 组装段消费，幂等零成本）
        ledger.mark_assembly_stale(entity);
        let Some(row) = ledger.row(entity).cloned() else {
            // 行已清(同帧先摘后挂等时序):脏标一并清,不留悬空脏标
            settled_rows.push(entity);
            continue;
        };
        // 行级裁决:有 pending 腿就保持脏(mesh 可能还在到货路上),无 pending
        // 腿且有失败腿才出队。各腿三态见 [`MeshLeg`]。
        let mut pending_leg = false;
        let mut failed_leg = false;
        let mesh_id = row.mesh.id();
        // —— 锚腿：floor_anchor 过滤激活且行在登记组内 → 可绘锚 = 保底档
        //（最粗变体）：锚腿决定实例槽/对象记录（记录引脚 = 锚槽位——GPU 端
        // b7 全哨兵时的 rec 回退即保底引脚，shader 零改动）；基准档降为按需
        // 高档，只由需求位图点名才进本批。过滤未激活/非组行 = 锚即基准。
        let demand_bits = lod_demand
            .active
            .then(|| lod_demand.levels.get(&mesh_id).copied())
            .flatten();
        // 锚档解析（anchor_level 纯函数，与 tick 需求集/DrawList 同源）：
        // floor_anchor 激活 ∧ 组在账 → 保底档（尾档缺席退回基准 level 0）
        let anchor: Option<(AssetId<Mesh>, Handle<Mesh>)> = if lod_demand.active {
            lod_groups.levels_of(mesh_id).and_then(|levels| {
                let level =
                    anchor_level(true, levels[(MAX_LOD_LEVELS - 1) as usize].is_some()) as usize;
                levels[level].clone().map(|handle| (handle.id(), handle))
            })
        } else {
            None
        };
        let (anchor_id, anchor_handle) = anchor.unwrap_or((mesh_id, row.mesh.clone()));
        let anchor_leg = MeshLeg::of(anchor_id, &pool, &invalidations, &readiness);
        // —— LOD 组档位盘点：行引用的 mesh 在登记组内 → 各档变体未驻留的进本批
        //（档位不被实体引用，本扫描是它们的唯一上传触发点；失败档保持哨兵 0，
        // 消费端向更细档回退——降级画面语义）。**level 0 = 基准 mesh 本身，必须
        // 跳过**——floor_anchor 下它由"基准高档腿"处理，关闭时由锚腿（锚 = 基准）
        // 处理，重复入 fresh 会让 commit 撞"同身份重复登记"。异步未到货的档位标
        // pending 进 readiness 账，重试由驻留管理器接管（见 lod_registry 模块注释）。
        // **需求位图过滤**：位图在账时只盘点点名的档位（保底档已由锚腿承担，跳过
        // 防重复入批）；位图缺席 = 不过滤（B 面/管理器关闭，全档盘点）。
        for (level, handle) in lod_groups
            .levels_of(mesh_id)
            .map(|levels| levels.iter().enumerate().skip(1))
            .into_iter()
            .flatten()
        {
            let Some(handle) = handle else { continue };
            let level_id = handle.id();
            if level_id == anchor_id {
                continue; // 锚腿承担（floor_anchor 下 = 保底档）
            }
            if let Some(bits) = demand_bits
                && (bits >> level) & 1 == 0
            {
                continue; // 非需求档不盘点（上传按需）
            }
            // 完成档跳过；失败档不再重试（哨兵回退兜底，行级判定不受影响）
            if MeshLeg::of(level_id, &pool, &invalidations, &readiness) == MeshLeg::Pending
                && lod_seen.insert(level_id)
            {
                fresh.push((level_id, handle.clone(), pool.resident(level_id).is_some()));
            }
        }
        // —— 锚腿（锚 = 保底档或基准）记账。
        if anchor_leg.tally(&mut pending_leg, &mut failed_leg) && seen.insert(anchor_id) {
            fresh.push((anchor_id, anchor_handle, pool.resident(anchor_id).is_some()));
        }
        // —— 基准高档腿（锚 ≠ 基准时，基准降为按需高档）：需求位图点名才进本批；
        // 未点名 = 跳过（不 pending——行可绘性已由锚腿承载，重试通道由期望档位
        // 变化重新置脏触发）。
        if anchor_id != mesh_id
            && let Some(bits) = demand_bits
            && bits & 1 != 0
        {
            let base_leg = MeshLeg::of(mesh_id, &pool, &invalidations, &readiness);
            if base_leg.tally(&mut pending_leg, &mut failed_leg) && seen.insert(mesh_id) {
                fresh.push((mesh_id, row.mesh.clone(), pool.resident(mesh_id).is_some()));
            }
        }
        match std_materials.get(&row.material) {
            Some(material) => {
                for handle in TEXTURE_SLOTS(material).into_iter().flatten() {
                    let id = handle.id();
                    let img_replace = invalidations.image_pending(id);
                    if image_cache.resident(id).is_some() && !img_replace {
                        continue; // 该腿完成
                    }
                    if readiness.image_failed(id) {
                        failed_leg = true;
                    } else {
                        pending_leg = true;
                        if img_seen.insert(id) {
                            let had_old = image_cache.resident(id).is_some();
                            fresh_images.push((id, handle.clone(), had_old));
                        }
                    }
                }
            }
            None => {
                // 材质未到货:行保持脏标下帧重试(材质已失败则该腿记失败)
                if readiness.material_failed(row.material.id()) {
                    failed_leg = true;
                } else {
                    pending_leg = true;
                    materials_pending += 1;
                }
            }
        }
        // —— 实例槽与对象记录:锚腿完成才动(保底锚下记录引脚取锚槽位 = 保底档
        // 引脚,GPU 端 rec 回退即保底——shader 零改动;未激活时锚 = 基准)。
        // 记录身份 (锚身份, 内容票据, 材质身份) 三者不变 = 内容不变,免重写;
        // 变化(首次驻留/换绑/换内容)走"新槽写 + 旧槽停尸房"(从不原地覆盖
        // ——在飞 draw 可能在读旧记录)。材质**内容**不进记录:材质环每帧
        // 全量重写,内容变化自动生效(材质槽索引跨帧稳定)。
        // 材质缺/贴图 pending 不拦记录(记录只持材质槽号,内容环里现读)。
        if anchor_leg == MeshLeg::Done {
            // 腿判定刚确认驻留,取不到是状态矛盾(与"容量保证后池缺席"同级)
            let anchor_slot = pool
                .resident(anchor_id)
                .ok_or_fatal("锚腿判定完成后驻留条目消失,状态矛盾")?;
            let bound = (anchor_id, anchor_slot.ticket, row.material.id());
            if row.instance_slot.is_none() || row.bound != Some(bound) {
                let mat_index = instance_tables
                    .ensure_material_slot(row.material.id())
                    .or_fatal("材质槽分配失败,上传链无法继续")?;
                let slot = instance_tables
                    .alloc_object_slot()
                    .or_fatal("对象表实例槽分配失败,上传链无法继续")?;
                let old_slot = ledger.set_instance_slot(entity, slot, bound);
                if let Some(old) = old_slot {
                    instance_tables.retire_object_slot(old);
                }
                let (aabb_min, aabb_max) = anchor_slot.local_aabb;
                staged_records.push((
                    slot,
                    pack_object_record(
                        anchor_slot.first_index(),
                        anchor_slot.index_count,
                        anchor_slot.vertex_base(),
                        mat_index,
                        aabb_min,
                        aabb_max,
                    ),
                ));
                // 新槽的模型要写进两个帧槽的环(本帧 + 下帧各一写)
                ledger.mark_model_dirty(entity);
                // 新槽的 LOD 表条目同款两帧奇偶纪律(换槽 = 槽号变了,
                // 表条目按槽直址必须重写;level 0 引脚取自本行基准 mesh 槽位)
                ledger.mark_lod_dirty(entity);
            }
        }
        if !pending_leg {
            settled_rows.push(entity);
            if failed_leg {
                failed_settled_rows += 1;
            }
        }
    }
    // 清标不依赖后续步骤成败:行"全驻留"是账本事实,与提交无关
    for entity in settled_rows {
        ledger.clear_dirty(entity);
    }
    // 重试队列规模在此定案(清标后即最终值:本系统此后不再增删脏标),唯一写点
    // 放在空批早退之前——放到提交尾段会让空批帧的该字段滞留上帧值
    ledger.stats.dirty_rows = ledger.dirty_count();
    readiness.note_failed_rows_cleared(failed_settled_rows);
    // —— 1.5) 收账:脏集清空且账本非空 = 全部资产驻留,报一次(稳态基准证据:
    // 之后每帧盘点 0 行、上传 0B,处理成本只剩 DrawList 读账本)。放空批
    // early-return 之前——清空脏集的那帧多半就是空批帧,放在后面永远打不出
    // (此后脏集不变,本处是唯一收账点)。
    if ledger.dirty_count() == 0 && !ledger.is_empty() {
        state.settled_logged.log(|| info!(
            "增量链首次清账:账本 {} 行全部资产驻留,脏集空——稳态帧零盘点零上传(增量判定线基准)",
            ledger.len()
        ));
    }
    // —— 2) 转换/规格映射(Tier①:未到货跳过重试,拒绝 warn 一次)——
    // 查无 → pending 记账(身份 → 等待帧数,跨帧累计);拒绝 → 进
    // failed 集不再重试(僵尸行由盘点段出队)。
    let mut converted: Vec<(AssetId<Mesh>, crate::render::ConvertedMesh, bool)> = Vec::new();
    let mut pending = 0usize;
    let mut marked_meshes: HashSet<AssetId<Mesh>> = HashSet::new();
    for (id, handle, replace) in fresh {
        match meshes.get(&handle) {
            None => {
                pending += 1; // 异步加载未到货,下帧脏行重试
                readiness.mark_pending_mesh(id);
                marked_meshes.insert(id);
            }
            Some(mesh) => match crate::render::convert_mesh(mesh) {
                Ok(c) => converted.push((id, c, replace)),
                Err(e) => {
                    // expel 必须在日志去重条件之外——拒绝必须每次记账,
                    // 否则"拒绝→Modified 复活→再拒绝"第二次不进 failed 集,脏行
                    // 无限重试。warned 集只管日志一次一资产
                    if state.warned.insert(id) {
                        warn!("mesh {id:?} 转换拒绝({e}),本步不绘制该资产,帧循环照常");
                    }
                    readiness.expel_mesh(id);
                }
            },
        }
    }
    let mut image_specs: Vec<(AssetId<Image>, Handle<Image>, crate::render::ImageSpec, bool)> =
        Vec::new();
    let mut pending_images = 0usize;
    let mut marked_images: HashSet<AssetId<Image>> = HashSet::new();
    for (id, handle, replace) in fresh_images {
        match image_assets.get(&handle) {
            None => {
                pending_images += 1; // 异步加载未到货,下帧脏行重试
                readiness.mark_pending_image(id);
                marked_images.insert(id);
            }
            Some(image) => match crate::render::image_spec(image) {
                Ok(spec) => image_specs.push((id, handle, spec, replace)),
                Err(e) => {
                    // 与 mesh 同口径:expel 在日志去重条件之外,拒绝每次记账
                    if state.warned_images.insert(id) {
                        warn!("贴图 {id:?} 规格拒绝({e}),本步不采样该资产,帧循环照常");
                    }
                    readiness.expel_image(id);
                }
            },
        }
    }
    if converted.is_empty() && image_specs.is_empty() && staged_records.is_empty() {
        // 空批不提交;pending 帧末对账照走——未到货的等待帧数照常累计
        readiness.reconcile(&marked_meshes, &marked_images);
        return Ok(());
    }
    // —— 3) 容量保证(初次懒建或维护迁移,等待/迁移在池内分账;迁移销毁旧池
    // 前经 frames.wait_all_inflight 等"图形最后使用")——
    let (vertex_bytes, index_bytes) = converted.iter().fold((0u64, 0u64), |(v, i), (_, c, _)| {
        (v + c.vertices.len() as u64, i + c.indices.len() as u64)
    });
    pool.ensure_capacity(
        &mut uploader,
        vertex_bytes,
        index_bytes,
        &mut || frames.wait_all_inflight(),
    )
    .or_fatal("池容量保证失败,上传链无法继续")?;
    let vertex_buffer = pool
        .vertex_buffer()
        .ok_or_fatal("容量保证后顶点池仍缺席,状态矛盾")?;
    let index_buffer = pool
        .index_buffer()
        .ok_or_fatal("容量保证后索引池仍缺席,状态矛盾")?;
    // —— 4a) 建图(Tier②):每张贴图 VkImage+dedicated memory+view+sampler——
    // 创建先于排批(拷贝目标必须已存在);提交失败走 Tier② 退出,局部变量随
    // 返回 Drop,不留半建对象。跨族设备:CONCURRENT 双族(与池同款)
    let image_sharing = ctx.sharing_families();
    let mut gpu_images: Vec<(AssetId<Image>, GpuImage)> = Vec::with_capacity(image_specs.len());
    for (id, _, spec, _) in &image_specs {
        let g = GpuImage::create(
            &ctx.device,
            &ctx.instance,
            ctx.physical_device,
            ctx.memory_contract(),
            spec,
            &image_sharing,
        )
        .or_fatal("贴图创建失败,上传链无法继续")?;
        gpu_images.push((*id, g));
    }
    // —— 4b) 排批:staging 连续排布(网格两段 + 每贴图一段);分配必在
    // ensure_capacity 之后——"不写越界"的顺序契约——
    // 贴图字节按 level 0 计（RGBA8 两档 4B/texel）：mip 链其余层级由提交内
    // blit 生成，不进 staging（数据量 = w*h*4，与 data.len() 在单级
    // 资产上恒等）
    let image_bytes: u64 = image_specs
        .iter()
        .map(|(_, _, spec, _)| u64::from(spec.width) * u64::from(spec.height) * 4)
        .sum();
    let mut staging = Vec::with_capacity(
        (vertex_bytes + index_bytes + image_bytes) as usize
            + staged_records.len() * OBJECT_RECORD_SIZE as usize,
    );
    let mut uploads = Vec::with_capacity(converted.len() * 2);
    let mut image_uploads = Vec::with_capacity(image_specs.len());
    let mut planned: Vec<(
        AssetId<Mesh>,
        bool,
        crate::render::ConvertedMesh,
        crate::render::PoolRange,
        crate::render::PoolRange,
    )> = Vec::with_capacity(converted.len());
    for (id, c, replace) in converted {
        let (vertex, index) = pool.alloc(c.vertices.len() as u64, c.indices.len() as u64);
        let v_src = staging.len() as u64;
        staging.extend_from_slice(&c.vertices);
        uploads.push(crate::render::StagingCopy {
            dst: vertex_buffer,
            src_offset: v_src,
            dst_offset: vertex.offset,
            size: vertex.size,
        });
        let i_src = staging.len() as u64;
        staging.extend_from_slice(&c.indices);
        uploads.push(crate::render::StagingCopy {
            dst: index_buffer,
            src_offset: i_src,
            dst_offset: index.offset,
            size: index.size,
        });
        planned.push((id, replace, c, vertex, index));
    }
    for ((id, handle, spec, _), (_, gpu)) in image_specs.iter().zip(&gpu_images) {
        // 贴图字节直取:上一阶段已验 Some;同一次系统调用内资产不变,状态矛盾
        // 按 Tier② 冒泡(与"容量保证后池缺席"同级,不 panic)
        let image = image_assets.get(handle).ok_or_fatal(format!(
            "贴图 {id:?} 在规格映射与排批之间消失,状态矛盾"
        ))?;
        let bytes = image
            .data
            .as_ref()
            .ok_or_fatal(format!("贴图 {id:?} 像素数据消失,状态矛盾"))?;
        // 只排 level 0（见 image_bytes 口径）；数据不足 = 宽高与数据不符的
        // 资产形状矛盾（能力降级后单级也至少要覆盖 level 0）
        let level0_bytes = u64::from(spec.width) * u64::from(spec.height) * 4;
        (bytes.len() as u64 >= level0_bytes)
            .then_some(())
            .ok_or_fatal(format!(
                "贴图 {id:?} 数据 {}B 不足 level 0 {level0_bytes}B,资产形状矛盾",
                bytes.len()
            ))?;
        let src_offset = staging.len() as u64;
        staging.extend_from_slice(&bytes[..level0_bytes as usize]);
        image_uploads.push(StagingImageCopy {
            image: gpu.image(),
            src_offset,
            width: spec.width,
            height: spec.height,
            // 实际建成级数（blit 能力降级后的值，与 view 层级同源）
            mip_levels: gpu.mip_levels(),
        });
    }
    // —— 对象记录入批:staging 排布 → 对象表拷贝段(48B/记录,按实例槽定址)。
    // 记录写经本批票据保护,图形提交在 ALL_COMMANDS 等票据——"上传完成才可
    // 使用"同一条执法点(读侧;写侧由"槽位只来自 bump 或排空点归还"保证)。
    let mut record_bytes = 0u64;
    for (slot, record) in &staged_records {
        let src_offset = staging.len() as u64;
        staging.extend_from_slice(record);
        uploads.push(StagingCopy {
            dst: instance_tables.object_buffer(),
            src_offset,
            dst_offset: u64::from(*slot) * OBJECT_RECORD_SIZE,
            size: OBJECT_RECORD_SIZE,
        });
        record_bytes += OBJECT_RECORD_SIZE;
    }
    // 跨族内存可见性不靠 release/acquire 成对:图像与池一样走 CONCURRENT 双族
    // 共享——上传批次的图像收尾是纯布局转换(IGNORED 族),
    // 跨队列内存可见性由图形提交等票据信号量收口(见 render/frame/frame_draw.rs 提交段注释)。
    // —— 5) 一次合批 = 一个 staging 范围 + 一次 transfer 提交。
    // staging 非空的批次必有提交、空批已在入口 return——查无提交是内部契约矛盾。
    // 提交失败不发布票据,池内 bump 游标随下次容量保证自然前移
    let batch = UploadBatch {
        staging,
        uploads,
        image_uploads,
        ..Default::default()
    };
    let ticket = uploader
        .submit_batch(batch)
        .or_fatal("transfer 提交失败,上传链无法继续(未发布票据)")?
        .ok_or_fatal("内部矛盾:staging 非空的批次未产生提交")?;
    // 驻留登记只在提交成功之后:失败路径不会留下指向未上传数据的账本行。
    // 持失效凭据的身份走 replace 换条目(旧区间进停尸房,排空后
    // 归还 free list),其余走 commit 严格登记(重复登记仍按编排 bug 报错)
    let mut replaced_meshes = 0usize;
    let mut committed_meshes: Vec<AssetId<Mesh>> = Vec::with_capacity(planned.len());
    let planned_count = planned.len();
    for (id, replace, c, vertex, index) in planned {
        if replace {
            // 旧槽位进停尸房(在飞安全,排空后归还 free list),返回值无需持有
            let _old_slot = pool.replace(
                id,
                vertex,
                index,
                c.vertex_count,
                c.index_count,
                c.local_aabb,
                ticket,
            );
            replaced_meshes += 1;
        } else {
            pool.commit(
                id,
                vertex,
                index,
                c.vertex_count,
                c.index_count,
                c.local_aabb,
                ticket,
            )
            .or_fatal("驻留登记失败,上传链无法继续")?;
        }
        committed_meshes.push(id);
    }
    // —— 槽位发布:提交成功 → 分槽(采样器按功能键去重,本体归表)
    // → 驻留登记带槽位。只写排空点归还过的新槽;换内容改走"新槽发布 + 旧槽
    // 延迟回收"(原槽重写对在飞 draw 不安全,UAB 不授权覆盖仍被未完成
    // 命令消费的描述符)。发布失败 = 槽容量耗尽,按 Tier② 冒泡,不静默截断
    // mip 链计量（实际建成级数 > 1 的张数；单级/降级不计）
    let mip_chained = gpu_images
        .iter()
        .filter(|(_, gpu)| gpu.mip_levels() > 1)
        .count();
    let mut sampler_slots_hit = HashSet::new();
    let mut replaced_images = 0usize;
    let mut committed_images: Vec<AssetId<Image>> = Vec::with_capacity(image_specs.len());
    for ((id, _, spec, replace), (gid, gpu)) in image_specs.iter().zip(gpu_images) {
        // 两表同序构建,错位即内部矛盾——Tier②(发布会把视图挂到错误身份上)
        (*id == gid)
            .then_some(())
            .ok_or_fatal(format!("贴图 {id:?} 与建图结果 {gid:?} 错位,状态矛盾"))?;
        let slots = tables
            .publish(gpu.view(), &sampler_key(&spec.sampler))
            .or_fatal(format!("贴图 {id:?} 槽位发布失败,上传链无法继续"))?;
        if *replace {
            replaced_images += 1;
        }
        sampler_slots_hit.insert(slots.sampler);
        // 换条目重传的旧条目随登记原子退场(旧图进停尸房、旧纹理槽进待回收账
        // ——在飞 draw 经旧槽读旧图直到排空;新 draw 经新槽读新图)
        image_cache.swap_commit(*id, gpu, ticket, slots, &mut tables);
        committed_images.push(*id);
    }
    // 换内容成功的身份销账(凭据消费);未成功(未到货/被拒)的保留,
    // 行保持脏标下帧重试。到货销账——pending 身份离开账面,等待帧数
    // 计入到货计量(非 pending 身份的稳态重传无账可销,静默无动作)。
    for id in &committed_meshes {
        invalidations.consume_mesh(*id);
        readiness.arrive_mesh(*id);
    }
    // LOD 组档位到货反查：档位资产提交成功 → 重挂引用行的 LOD
    // 脏标（draw_frame 下两帧把新引脚写进两个帧槽的表环；O(rows) 反查，
    // 触发频率 = 档位提交，稀疏事件）
    for id in committed_meshes {
        if let Some(base) = lod_groups.base_of(id) {
            for entity in ledger.rows_with_mesh(base) {
                ledger.mark_lod_dirty(entity);
            }
        }
    }
    for id in committed_images {
        invalidations.consume_image(id);
        readiness.arrive_image(id);
    }
    // 帧末对账:仍被引用的 pending 身份等待 +1,失去引用的出账计取消
    //(despawn/换柄的"到货时人已不在",到货后无行可服务=不错误复活)
    readiness.reconcile(&marked_meshes, &marked_images);
    let srgb_count = image_specs
        .iter()
        .filter(|(_, _, spec, _)| spec.srgb_role())
        .count();
    // 计量:上传侧累计进账本(判定线的字节/批次口径);对象记录字节并入
    // 批次字节、槽位读数单独报(对象表/材质槽分列)
    let batch_bytes = vertex_bytes + index_bytes + image_bytes + record_bytes;
    let stats = &mut ledger.stats;
    stats.upload_batches_total += 1;
    stats.upload_bytes_total += batch_bytes;
    stats.last_batch_bytes = batch_bytes;
    stats.object_slots = usize::try_from(instance_tables.used_object_slots())
        .unwrap_or(usize::MAX);
    stats.material_slots = usize::try_from(instance_tables.used_material_slots())
        .unwrap_or(usize::MAX);
    info!(
        "上传批次 #{ticket}:mesh {} 个(其中换内容 {replaced_meshes},顶点 {vertex_bytes}B / 索引 {index_bytes}B)\
         + 贴图 {} 张(其中换内容重传 {replaced_images},{image_bytes}B,sRGB 角色 {srgb_count} / 线性 {},mip 链 {mip_chained} 张)\
         + 对象记录 {} 条(换槽重写,{record_bytes}B);\
         槽位发布:纹理槽 {}/{}(含 fallback 0 号)、采样器槽 {} 种(本批涉及 {new_sampler});\
         实例槽 {}/{}(停尸房 {})、材质槽 {}/{};\
         未到货跳过 mesh {pending} / 贴图 {pending_images}(材质缺 {materials_pending}),失败出队行 {failed_settled_rows};\
         已驻留 mesh {} / 贴图 {}",
        planned_count,
        image_specs.len(),
        image_specs.len() - srgb_count,
        staged_records.len(),
        tables.used_texture_slots(),
        tables.capacity(),
        tables.used_sampler_slots(),
        instance_tables.used_object_slots(),
        instance_tables.capacity_objects(),
        instance_tables.graveyard_count(),
        instance_tables.used_material_slots(),
        instance_tables.capacity_materials(),
        pool.resident_count(),
        image_cache.resident_count(),
        new_sampler = sampler_slots_hit.len(),
    );
    Ok(())
}
