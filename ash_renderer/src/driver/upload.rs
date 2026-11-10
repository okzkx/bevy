//! 上传编排:消费 [`InstanceLedger`] 的**待盘点行**(新增/换柄的增量,非全量快照),
//! 把未驻留的 `Assets<Mesh>` 去重转换、未驻留的贴图经材质五槽去重建图,合成
//! **一个批次**送进 GPU(网格进池、贴图进 dedicated memory)。零裸 Vulkan 调用——
//! 资源操作全部走 [`crate::render`] 的 pool/uploader/images 出口。
//!
//! 时序:PostUpdate 账本对账 → Last:准备 + 上传提交 → 同帧图形提交(draw 挂 ticket
//! 信号量等待,见 render/frame/record.rs)。排序在 [`AshUploadPlugin`] 声明。
//!
//! 失败分流:资产未到货/转换拒绝 = 循环内逐项降级,就地 warn 下帧重试,不进错误通道;
//! 提前 return 的 Vulkan/账本失败 = `or_fatal`/`ok_or_fatal` 冒泡致命退出。提交失败不
//! 发布票据,池内 bump 游标随下次容量保证自然前移,不留指向未上传数据的账本行。
//!
//! 增量:发现侧只把"新增/换柄"的行放进脏集,静态场景脏集为空,零盘点零上传;已全驻留
//! 的行就地清标,空批不提交。资产身份跨行跨帧去重,只有驻留缓存查无才转换上传。未到货
//! 的行保持脏标下帧重试,记账在 [`AssetReadiness`](pending 身份 → 等待帧数);加载
//! 失败/转换拒绝进 failed 集不再重试,有失败腿且无 pending 腿的行出队。
//!
//! 贴图上传:staging 字节 → `vkCmdCopyBufferToImage`,前后置布局屏障在 uploader 图像段;
//! `ImageSampler::Default` 按 ImagePlugin 全局默认(linear)解析,格式角色以
//! `texture_descriptor.format` 的 Srgb 后缀为准,不二次推断。

use std::collections::{HashMap, HashSet};

use bevy::{
    app::OnAppExitSystems, asset::AssetId, ecs::system::SystemParam, image::Image,
    pbr::StandardMaterial, prelude::*,
};

use ash_macros::system;

use crate::{
    common::syntax::{OnceLog, OptionTierExt, ResultTierExt},
    render::{
        bake, block_bytes, cache_key, convert_mesh, has_alpha, image_spec, pack_object_record,
        sampler_key, AtlasGpu, BakeError, BakedBcn, BcnCache, BindlessTextureTable, Context,
        ConvertedMesh, FramePool, GpuImage, ImageCache, ImageSpec, InstanceTables, MeshPool,
        PoolRange, StagingCopy, StagingImageCopy, TextureRole, UploadBatch, Uploader,
        MAX_LOD_LEVELS, OBJECT_RECORD_SIZE,
    },
    scene::{
        anchor_level, AssetInvalidations, AssetReadiness, AssetRetirements, InstanceLedger,
        InstanceRow, LodDemand, LodGroups, ResidencySet,
    },
};

/// StandardMaterial 的贴图槽清单(五槽)。
fn texture_slots(m: &StandardMaterial) -> [&Option<Handle<Image>>; 5] {
    [
        &m.base_color_texture,
        &m.metallic_roughness_texture,
        &m.occlusion_texture,
        &m.normal_map_texture,
        &m.emissive_texture,
    ]
}

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
                .run_if(resource_exists::<BindlessTextureTable>)
                .run_if(resource_exists::<AssetInvalidations>)
                .run_if(resource_exists::<AssetReadiness>)
                .run_if(resource_exists::<AssetRetirements>)
                .after(ResidencySet)
                .before(crate::driver::host::draw_frame)
                .before(OnAppExitSystems),
        );
    }
}

/// 上传状态(跨帧 `Local`):warn 去重 + "增量链首次清账"一次性日志。
#[derive(Default)]
struct UploadState {
    /// 转换拒绝已 warn 过的 mesh 身份(一次一资产,不刷屏)。
    warned: HashSet<AssetId<Mesh>>,
    /// 规格映射拒绝已 warn 过的贴图身份(一次一资产,不刷屏)。
    warned_images: HashSet<AssetId<Image>>,
    /// "脏集首次清空且账本非空"报一次(稳态零盘点零上传的基准)。
    settled_logged: OnceLog,
    /// BCn 烘焙缓存(首次有压缩角色贴图时懒建;缓存是优化不是正确性)。
    bcn_cache: Option<BcnCache>,
}

/// mesh 腿三态:完成(驻留且无失效凭据)/失败(身份在 failed 集,不再重试)/待到货
/// (保持脏标跨帧重试,身份进本批)。持失效凭据的已驻留资产走换条目重传
/// (swap-on-commit,驻留不留空洞)。
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
}

/// 一行的腿状态汇总:有 pending 腿就保持脏标(可能还在到货路上),无 pending 腿且
/// 有失败腿才出队。
#[derive(Default)]
struct RowLegs {
    pending: bool,
    failed: bool,
}

impl RowLegs {
    /// 记入一条 mesh 腿,返回"身份需进本批"(待到货)。
    fn tally(&mut self, leg: MeshLeg) -> bool {
        match leg {
            MeshLeg::Done => false,
            MeshLeg::Failed => {
                self.failed = true;
                false
            }
            MeshLeg::Pending => {
                self.pending = true;
                true
            }
        }
    }
}

/// 脏行盘点的产物。
#[derive(Default)]
struct RowScan {
    /// (身份, 柄, 是否换条目重传):第三位决定 commit 走登记还是替换。
    fresh_meshes: Vec<(AssetId<Mesh>, Handle<Mesh>, bool)>,
    fresh_images: Vec<(AssetId<Image>, Handle<Image>, bool)>,
    /// 本次盘点登记的贴图角色（材质五槽点时顺带，同图多角色取优先者）。
    image_roles: HashMap<AssetId<Image>, TextureRole>,
    /// 待写对象记录:(实例槽, 记录),排批时并入 staging。
    staged_records: Vec<(u32, [u8; OBJECT_RECORD_SIZE as usize])>,
    seen: HashSet<AssetId<Mesh>>,
    /// LOD 组档位身份去重集。
    lod_seen: HashSet<AssetId<Mesh>>,
    img_seen: HashSet<AssetId<Image>>,
    materials_pending: usize,
    /// 失败腿出队的行数(防僵尸重试;mesh 未驻留时 DrawList 本就暂缓该行,失败贴图用
    /// fallback 白图照画)。
    failed_settled_rows: usize,
}

/// 待上传贴图条目：(身份, 句柄, 规格, 是否换条目重传, 块压缩直通产物)。
type ReadyImage = (
    AssetId<Image>,
    Handle<Image>,
    ImageSpec,
    bool,
    Option<BakedBcn>,
);

/// 转换/规格映射的产物。
#[derive(Default)]
struct ReadyAssets {
    /// (身份, 转换结果, 是否换条目重传)
    converted: Vec<(AssetId<Mesh>, ConvertedMesh, bool)>,
    /// 尾位 Some = 块压缩直通（BCn 块数据已就绪，排批全链连排）。
    image_specs: Vec<ReadyImage>,
    pending_meshes: usize,
    pending_images: usize,
    marked_meshes: HashSet<AssetId<Mesh>>,
    marked_images: HashSet<AssetId<Image>>,
}

/// flush_uploads 的参数束(SystemParam):系统访问的全部资源。
#[derive(SystemParam)]
struct UploadData<'w> {
    meshes: Res<'w, Assets<Mesh>>,
    std_materials: Res<'w, Assets<StandardMaterial>>,
    image_assets: Res<'w, Assets<Image>>,
    /// 账本(独占:消费脏行、写上传计量)。
    ledger: ResMut<'w, InstanceLedger>,
    /// 资产失效凭据:换内容成功的身份在此销账,未成功的保留供行级重试。
    invalidations: ResMut<'w, AssetInvalidations>,
    /// 异步就绪账面:本系统独占写 pending 侧,失败读侧只写 failed 侧。
    readiness: ResMut<'w, AssetReadiness>,
    /// 卸载回收账面:事件读侧记下的待回收身份,本系统独占消费。
    retirements: ResMut<'w, AssetRetirements>,
    /// GPU 常驻绘制数据:实例槽分配与记录换槽的写点独占本系统;环的写入在 draw_frame。
    instance_tables: ResMut<'w, InstanceTables>,
    /// LOD 组登记处:档位盘点与到货反查。
    lod_groups: Res<'w, LodGroups>,
    /// 需求档位图:residency_tick 同帧发布(先于本系统),位图点名才盘点。
    lod_demand: Res<'w, LodDemand>,
    /// overlay 图集:旧代并入本系统的排空点;overlay 未装时 None。
    atlas: Option<ResMut<'w, AtlasGpu>>,
    ctx: Res<'w, Context>,
    /// 帧槽:销毁前等"图形最后使用"经此接线。
    frames: Res<'w, FramePool>,
    pool: ResMut<'w, MeshPool>,
    image_cache: ResMut<'w, ImageCache>,
    tables: ResMut<'w, BindlessTextureTable>,
    uploader: ResMut<'w, Uploader>,
}

/// 账本脏行 → 去重(mesh 直读 + 贴图经材质五槽)→ 转换/建图 → 容量保证 → 合批提交 →
/// 驻留登记。空批次(无新资产)不提交。
///
/// 提前 return 的失败一律 `or_fatal`/`ok_or_fatal` 冒泡,本系统只管控制流。
#[system]
fn flush_uploads(mut data: UploadData, mut state: Local<UploadState>) -> Result<(), BevyError> {
    let drained_slots = drain_graveyards(&mut data)?;
    reclaim_retired(&mut data, drained_slots);
    // overlay 图集旧代:同样要等帧侧 + 上传票据。旧代的描述符槽已在整传帧交回表,由下一轮
    // drain_retired_slots 归还
    let UploadData {
        atlas,
        uploader,
        frames,
        ..
    } = &mut data;
    if let Some(atlas) = atlas {
        atlas
            .drain_graveyard(uploader, &mut || frames.wait_all_inflight())
            .or_fatal("图集待回收队列排空失败(双条件等待),上传链无法继续")?;
    }
    let mut scan = scan_dirty_rows(&mut data)?;
    // 脏集清空且账本非空 = 全部资产驻留,此后每帧盘点 0 行、上传 0B。必须放空批早退
    // 之前:清空脏集的那帧多半就是空批帧
    if data.ledger.dirty_count() == 0 && !data.ledger.is_empty() {
        state.settled_logged.log(|| {
            info!(
            "增量链首次清空:账本 {} 行全部资产驻留,脏集空——稳态帧零盘点零上传(作为增量判定基准)",
            data.ledger.len()
        )
        });
    }
    let ready = prepare_assets(&mut data, &mut scan, &mut state);
    if ready.converted.is_empty() && ready.image_specs.is_empty() && scan.staged_records.is_empty()
    {
        // 空批不提交;pending 帧末对账照走,未到货的等待帧数照常累计
        data.readiness
            .reconcile(&ready.marked_meshes, &ready.marked_images);
        return Ok(());
    }
    upload_batch(&mut data, &scan, ready)
}

/// 统一排空点(内容重传/卸载/替换换下的旧资源):"图形最后使用 = 全部帧槽 fence"且
/// "上传完成 = 停尸房票据"后才销毁/归还,各路径空房判定,非空才等待。排空失败不重试,
/// 致命退出。
///
/// 对象表排空先于本帧槽位分配:复用槽的旧记录读者已全部收口,新记录写入无竞态。
/// 返回归还的描述符槽数。
fn drain_graveyards(data: &mut UploadData) -> Result<usize, BevyError> {
    let UploadData {
        ledger,
        instance_tables,
        frames,
        pool,
        image_cache,
        tables,
        uploader,
        ..
    } = data;
    let mut wait_draws = || frames.wait_all_inflight();
    image_cache
        .drain_graveyard(uploader, &mut wait_draws)
        .or_fatal("贴图待回收队列排空失败(帧侧等待),上传链无法继续")?;
    pool.drain_graveyard(uploader, &mut wait_draws)
        .or_fatal("网格区间待回收队列排空失败(帧侧等待),上传链无法继续")?;
    let drained_slots = tables
        .drain_retired_slots(&mut wait_draws)
        .or_fatal("描述符槽回收排空失败(帧侧等待),上传链无法继续")?;
    // 行删除/换绑换下的旧实例槽(update_ledger 交接而来)进对象表停尸房
    for slot in ledger.take_freed_slots() {
        instance_tables.retire_object_slot(slot);
    }
    instance_tables
        .drain_graveyard(&mut wait_draws)
        .or_fatal("对象表待回收队列排空失败(帧侧等待),上传链无法继续")?;
    Ok(drained_slots)
}

/// 卸载回收:待回收身份的驻留条目进停尸房(下帧排空),描述符槽进待回收账,失效凭据
/// 一并清除(资产已不在容器)。
///
/// 坑:必须先于脏行盘点——同帧"卸载又重载同身份"的行要看到驻留已出账,否则被旧条目
/// 误判为已完成。
fn reclaim_retired(data: &mut UploadData, drained_slots: usize) {
    let UploadData {
        ledger,
        invalidations,
        retirements,
        lod_groups,
        pool,
        image_cache,
        tables,
        ..
    } = data;
    let mut reclaimed = 0u64;
    for id in retirements.take_meshes() {
        if pool.retire(id).is_some() {
            reclaimed += 1;
        }
        invalidations.consume_mesh(id);
        // 驻留出账后,以该身份为锚的行可能翻暂缓(基准锚直引 + 保底档锚经组反查)
        for entity in ledger.rows_with_mesh(id) {
            ledger.mark_assembly_stale(entity);
        }
        if let Some(base) = lod_groups.base_of(id) {
            for entity in ledger.rows_with_mesh(base) {
                ledger.mark_assembly_stale(entity);
            }
        }
    }
    for id in retirements.take_images() {
        if image_cache.retire(id, tables) {
            reclaimed += 1;
        }
        invalidations.consume_image(id);
    }
    retirements.stats.resident_reclaimed_total += reclaimed;
    retirements.stats.slots_reclaimed_total += drained_slots as u64;
    if reclaimed > 0 || drained_slots > 0 {
        info!(
            "卸载回收完成:驻留条目 {reclaimed} 个(mesh+image,区间/旧图进待回收队列下帧排空),\
             描述符槽归还 {drained_slots} 条"
        );
    }
}

/// 脏行盘点(上传清单 = 账本脏行):每行查各腿资产的驻留状态,未驻留或持失效凭据的
/// 身份进本批;全部腿完成的行就地清标,有 pending 腿的保持脏标重试。锚腿完成的行顺带
/// 分配实例槽与对象记录。
fn scan_dirty_rows(data: &mut UploadData) -> Result<RowScan, BevyError> {
    let mut scan = RowScan::default();
    let mut settled_rows: Vec<Entity> = Vec::new();
    let dirty_entities: Vec<Entity> = data.ledger.dirty_entities().collect();
    for entity in dirty_entities {
        // 被盘点的行可绘输入本帧可能翻面,标记组装重估(draw_frame 组装段消费)
        data.ledger.mark_assembly_stale(entity);
        let Some(row) = data.ledger.row(entity).cloned() else {
            // 行已清(同帧先摘后挂等时序):脏标一并清,不留悬空脏标
            settled_rows.push(entity);
            continue;
        };
        let mut legs = RowLegs::default();
        let (anchor_id, anchor_leg) = scan_mesh_legs(data, &row, &mut legs, &mut scan);
        scan_texture_legs(data, &row, &mut legs, &mut scan);
        if anchor_leg == MeshLeg::Done {
            ensure_object_record(data, entity, &row, anchor_id, &mut scan)?;
        }
        if !legs.pending {
            settled_rows.push(entity);
            if legs.failed {
                scan.failed_settled_rows += 1;
            }
        }
    }
    // 清标不依赖后续步骤成败:行"全驻留"是账本事实,与提交无关
    for entity in settled_rows {
        data.ledger.clear_dirty(entity);
    }
    // 重试队列规模唯一写点:必须在空批早退之前,否则空批帧的该字段滞留上帧值
    data.ledger.stats.dirty_rows = data.ledger.dirty_count();
    data.readiness
        .note_failed_rows_cleared(scan.failed_settled_rows);
    Ok(scan)
}

/// 盘点一行的 mesh 腿(锚 + LOD 档位 + 基准高档),返回 (锚身份, 锚腿状态)。
///
/// floor_anchor 过滤激活且行在登记组内 → 锚 = 保底档(最粗变体),对象记录的绘制参数
/// 取锚槽位(GPU 端 b7 全空时回退到它,即保底,shader 无需改动);基准降为按需高档。
/// 其余情况锚即基准。
fn scan_mesh_legs(
    data: &UploadData,
    row: &InstanceRow,
    legs: &mut RowLegs,
    scan: &mut RowScan,
) -> (AssetId<Mesh>, MeshLeg) {
    let UploadData {
        pool,
        invalidations,
        readiness,
        lod_groups,
        lod_demand,
        ..
    } = data;
    let mesh_id = row.mesh.id();
    let demand_bits = lod_demand
        .active
        .then(|| lod_demand.levels.get(&mesh_id).copied())
        .flatten();
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
    let anchor_leg = MeshLeg::of(anchor_id, pool, invalidations, readiness);
    // LOD 档位不被实体直接引用,本扫描是它们唯一的上传触发点;失败档保持 0,消费端向
    // 更细档回退。异步未到货的档位重试由驻留管理器接管(见 lod_registry 模块文档)。
    // 坑:level 0 = 基准 mesh 必须跳过,它由锚腿或基准高档腿处理,重复入批会让 commit
    // 撞"同身份重复登记"。位图在账时只盘点点名的档位;位图缺席 = 全档盘点。
    for (level, handle) in lod_groups
        .levels_of(mesh_id)
        .map(|levels| levels.iter().enumerate().skip(1))
        .into_iter()
        .flatten()
    {
        let Some(handle) = handle else { continue };
        let level_id = handle.id();
        if level_id == anchor_id {
            continue; // 锚腿承担
        }
        if let Some(bits) = demand_bits
            && (bits >> level) & 1 == 0
        {
            continue; // 非需求档不盘点
        }
        // 完成档跳过;失败档不再重试
        if MeshLeg::of(level_id, pool, invalidations, readiness) == MeshLeg::Pending
            && scan.lod_seen.insert(level_id)
        {
            scan.fresh_meshes
                .push((level_id, handle.clone(), pool.resident(level_id).is_some()));
        }
    }
    if legs.tally(anchor_leg) && scan.seen.insert(anchor_id) {
        scan.fresh_meshes
            .push((anchor_id, anchor_handle, pool.resident(anchor_id).is_some()));
    }
    // 基准高档腿:位图点名才进本批;未点名不算 pending(行可绘性由锚腿承载,期望档位
    // 变化时会重新置脏)
    if anchor_id != mesh_id
        && let Some(bits) = demand_bits
        && bits & 1 != 0
    {
        let base_leg = MeshLeg::of(mesh_id, pool, invalidations, readiness);
        if legs.tally(base_leg) && scan.seen.insert(mesh_id) {
            scan.fresh_meshes
                .push((mesh_id, row.mesh.clone(), pool.resident(mesh_id).is_some()));
        }
    }
    (anchor_id, anchor_leg)
}

/// 盘点一行的贴图腿:material 柄 → 五槽解引用,未驻留或持失效凭据的身份进本批。材质
/// 未到货 = 行保持脏标下帧重试(材质已失败则记失败腿)。
fn scan_texture_legs(data: &UploadData, row: &InstanceRow, legs: &mut RowLegs, scan: &mut RowScan) {
    let UploadData {
        std_materials,
        invalidations,
        readiness,
        image_cache,
        ..
    } = data;
    match std_materials.get(&row.material) {
        Some(material) => {
            // 槽位角色随腿盘点登记：同图多角色取优先者（枚举顺序即优先级）
            const SLOT_ROLES: [TextureRole; 5] = [
                TextureRole::BaseColor,
                TextureRole::MetallicRoughness,
                TextureRole::Occlusion,
                TextureRole::Normal,
                TextureRole::Emissive,
            ];
            for (role, handle) in SLOT_ROLES.into_iter().zip(texture_slots(material)) {
                let Some(handle) = handle else { continue };
                scan.image_roles
                    .entry(handle.id())
                    .and_modify(|r| *r = (*r).min(role))
                    .or_insert(role);
                let id = handle.id();
                let had_old = image_cache.resident(id).is_some();
                if had_old && !invalidations.image_pending(id) {
                    continue; // 该腿完成
                }
                if readiness.image_failed(id) {
                    legs.failed = true;
                } else {
                    legs.pending = true;
                    if scan.img_seen.insert(id) {
                        scan.fresh_images.push((id, handle.clone(), had_old));
                    }
                }
            }
        }
        None => {
            if readiness.material_failed(row.material.id()) {
                legs.failed = true;
            } else {
                legs.pending = true;
                scan.materials_pending += 1;
            }
        }
    }
}

/// 锚腿完成的行:记录身份 (锚身份, 内容票据, 材质身份) 变化时(首次驻留/换绑/换内容)
/// 走"新槽写 + 旧槽进停尸房"。
///
/// 坑:从不原地覆盖,在飞 draw 可能在读旧记录。材质内容不进记录:材质环每帧全量重写,
/// 材质槽索引跨帧稳定;材质缺/贴图 pending 都不拦记录。
fn ensure_object_record(
    data: &mut UploadData,
    entity: Entity,
    row: &InstanceRow,
    anchor_id: AssetId<Mesh>,
    scan: &mut RowScan,
) -> Result<(), BevyError> {
    let UploadData {
        ledger,
        instance_tables,
        pool,
        ..
    } = data;
    // 腿判定刚确认驻留,取不到是状态矛盾
    let anchor_slot = pool
        .resident(anchor_id)
        .ok_or_fatal("锚腿判定完成后驻留条目消失,状态矛盾")?;
    let bound = (anchor_id, anchor_slot.ticket, row.material.id());
    if row.instance_slot.is_some() && row.bound == Some(bound) {
        return Ok(());
    }
    let mat_index = instance_tables
        .ensure_material_slot(row.material.id())
        .or_fatal("材质槽分配失败,上传链无法继续")?;
    let slot = instance_tables
        .alloc_object_slot()
        .or_fatal("对象表实例槽分配失败,上传链无法继续")?;
    if let Some(old) = ledger.set_instance_slot(entity, slot, bound) {
        instance_tables.retire_object_slot(old);
    }
    let (aabb_min, aabb_max) = anchor_slot.local_aabb;
    scan.staged_records.push((
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
    // 新槽的模型与 LOD 表条目都按槽直址,要写进两个帧槽的环
    ledger.mark_model_dirty(entity);
    ledger.mark_lod_dirty(entity);
    Ok(())
}

/// 转换/规格映射:未到货 → pending 记账(身份 → 等待帧数,跨帧累计);拒绝 → 进 failed
/// 集不再重试。
///
/// 坑:expel 必须在日志去重之外——拒绝每次都要记账,否则"拒绝 → Modified 复活 → 再拒绝"
/// 第二次不进 failed 集,脏行无限重试。warned 集只管日志一次一资产。
fn prepare_assets(
    data: &mut UploadData,
    scan: &mut RowScan,
    state: &mut UploadState,
) -> ReadyAssets {
    let UploadData {
        meshes,
        image_assets,
        readiness,
        ..
    } = data;
    let mut ready = ReadyAssets::default();
    for (id, handle, replace) in scan.fresh_meshes.drain(..) {
        match meshes.get(&handle) {
            None => {
                ready.pending_meshes += 1;
                readiness.mark_pending_mesh(id);
                ready.marked_meshes.insert(id);
            }
            Some(mesh) => match convert_mesh(mesh) {
                Ok(c) => ready.converted.push((id, c, replace)),
                Err(e) => {
                    if state.warned.insert(id) {
                        warn!("mesh {id:?} 转换拒绝({e}),本步不绘制该资产,帧循环照常");
                    }
                    readiness.expel_mesh(id);
                }
            },
        }
    }
    for (id, handle, replace) in std::mem::take(&mut scan.fresh_images) {
        match image_assets.get(&handle) {
            None => {
                ready.pending_images += 1;
                readiness.mark_pending_image(id);
                ready.marked_images.insert(id);
            }
            Some(image) => match bcn_direct(state, scan, id, image) {
                Err(e) => {
                    if state.warned_images.insert(id) {
                        warn!("贴图 {id:?} BCn 烘焙拒绝({e}),本步不采样该资产,帧循环照常");
                    }
                    readiness.expel_image(id);
                }
                Ok(bcn) => match image_spec(image) {
                    Ok(mut spec) => {
                        if let Some(baked) = &bcn {
                            // 规格换 BCn：格式/补后尺寸/预烘焙级数全部来自烘焙产物
                            spec.format = baked.format;
                            spec.width = baked.width;
                            spec.height = baked.height;
                            spec.mip_levels = baked.mip_levels;
                        }
                        ready.image_specs.push((id, handle, spec, replace, bcn));
                    }
                    Err(e) => {
                        if state.warned_images.insert(id) {
                            warn!("贴图 {id:?} 规格拒绝({e}),本步不采样该资产,帧循环照常");
                        }
                        readiness.expel_image(id);
                    }
                },
            },
        }
    }
    ready
}

/// BCn 直通判定与烘焙：贴图登记了压缩角色才进这条路径——查磁盘缓存，未命中
/// 现烤并写回。`Ok(None)` = 无角色/角色不压缩（走 RGBA8 现行路径）；
/// `Err` = 烘焙拒绝（数据形状矛盾，调用方按规格拒绝处理）。
fn bcn_direct(
    state: &mut UploadState,
    scan: &RowScan,
    id: AssetId<Image>,
    image: &Image,
) -> Result<Option<BakedBcn>, BakeError> {
    let Some(&role) = scan.image_roles.get(&id) else {
        return Ok(None);
    };
    let Some(data) = image.data.as_ref() else {
        return Ok(None); // 无像素数据交给规格映射拒绝
    };
    if role.target_format(has_alpha(data)).is_none() {
        return Ok(None); // 该角色不压缩（如 metallicRoughness 无 BC7 暂不压）
    }
    let cache = state.bcn_cache.get_or_insert_with(BcnCache::open);
    let key = cache_key(role, data, image.width(), image.height());
    if let Some(baked) = cache.load(key) {
        return Ok(Some(baked));
    }
    let baked = bake(role, data, image.width(), image.height())?;
    if let Err(e) = cache.store(key, &baked) {
        warn!("贴图 {id:?} BCn 缓存写回失败({e}),本次现烤直用不落盘");
    }
    Ok(Some(baked))
}

/// 容量保证 → 建图 → 排批 → 一次 transfer 提交 → 驻留登记与槽位发布 → 销账与计量。
fn upload_batch(
    data: &mut UploadData,
    scan: &RowScan,
    ready: ReadyAssets,
) -> Result<(), BevyError> {
    let UploadData {
        image_assets,
        ledger,
        invalidations,
        readiness,
        instance_tables,
        lod_groups,
        ctx,
        frames,
        pool,
        image_cache,
        tables,
        uploader,
        ..
    } = data;
    let ReadyAssets {
        converted,
        image_specs,
        pending_meshes,
        pending_images,
        marked_meshes,
        marked_images,
    } = ready;
    let staged_records = &scan.staged_records;
    // 容量保证:初次懒建或维护迁移,迁移销毁旧池前要等"图形最后使用"
    let (vertex_bytes, index_bytes) = converted.iter().fold((0u64, 0u64), |(v, i), (_, c, _)| {
        (v + c.vertices.len() as u64, i + c.indices.len() as u64)
    });
    pool.ensure_capacity(uploader, vertex_bytes, index_bytes, &mut || {
        frames.wait_all_inflight()
    })
    .or_fatal("池容量保证失败,上传链无法继续")?;
    let vertex_buffer = pool
        .vertex_buffer()
        .ok_or_fatal("容量保证后顶点池仍缺席,状态矛盾")?;
    let index_buffer = pool
        .index_buffer()
        .ok_or_fatal("容量保证后索引池仍缺席,状态矛盾")?;
    // 建图先于排批(拷贝目标必须已存在);之后失败时局部变量随返回 Drop,不留半建对象。
    // 跨族设备走 CONCURRENT 双族(与池相同)
    let image_sharing = ctx.sharing_families();
    let mut gpu_images: Vec<(AssetId<Image>, GpuImage)> = Vec::with_capacity(image_specs.len());
    for (id, _, spec, _, _) in &image_specs {
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
    // 排批:staging 连续排布(网格两段 + 每贴图一段 + 对象记录);池区间分配必在
    // ensure_capacity 之后,否则会写越界。贴图只排 level 0(RGBA8 两档 4B/texel),mip 链
    // 其余层级由提交内 blit 生成
    let image_bytes: u64 = image_specs
        .iter()
        .map(|(_, _, spec, _, bcn)| match bcn {
            Some(baked) => baked.total_bytes(),
            None => u64::from(spec.width) * u64::from(spec.height) * 4,
        })
        .sum();
    let mut staging = Vec::with_capacity(
        (vertex_bytes + index_bytes + image_bytes) as usize
            + staged_records.len() * OBJECT_RECORD_SIZE as usize,
    );
    let mut uploads = Vec::with_capacity(converted.len() * 2);
    let mut image_uploads = Vec::with_capacity(image_specs.len());
    let mut planned: Vec<(AssetId<Mesh>, bool, ConvertedMesh, PoolRange, PoolRange)> =
        Vec::with_capacity(converted.len());
    for (id, c, replace) in converted {
        let (vertex, index) = pool.alloc(c.vertices.len() as u64, c.indices.len() as u64);
        let v_src = staging.len() as u64;
        staging.extend_from_slice(&c.vertices);
        uploads.push(StagingCopy {
            dst: vertex_buffer,
            src_offset: v_src,
            dst_offset: vertex.offset,
            size: vertex.size,
        });
        let i_src = staging.len() as u64;
        staging.extend_from_slice(&c.indices);
        uploads.push(StagingCopy {
            dst: index_buffer,
            src_offset: i_src,
            dst_offset: index.offset,
            size: index.size,
        });
        planned.push((id, replace, c, vertex, index));
    }
    for ((id, handle, spec, _, bcn), (_, gpu)) in image_specs.iter().zip(&gpu_images) {
        // 上一阶段已验 Some,同一次系统调用内资产不变,取不到是状态矛盾
        let image = image_assets
            .get(handle)
            .ok_or_fatal(format!("贴图 {id:?} 在规格映射与排批之间消失,状态矛盾"))?;
        if let Some(baked) = bcn {
            // 块压缩直通：全链块数据连排，offset 对齐到块字节倍数(VUID 红线)
            let bb = u64::from(block_bytes(baked.format));
            while !(staging.len() as u64).is_multiple_of(bb) {
                staging.push(0);
            }
            let src_offset = staging.len() as u64;
            staging.extend_from_slice(&baked.data);
            image_uploads.push(StagingImageCopy {
                image: gpu.image(),
                src_offset,
                width: spec.width,
                height: spec.height,
                mip_levels: gpu.mip_levels(),
                format: spec.format,
            });
            continue;
        }
        let bytes = image
            .data
            .as_ref()
            .ok_or_fatal(format!("贴图 {id:?} 像素数据消失,状态矛盾"))?;
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
            // 实际建成级数(blit 能力降级后的值,与 view 层级同源)
            mip_levels: gpu.mip_levels(),
            format: spec.format,
        });
    }
    // 对象记录按实例槽定址拷进对象表。读侧由图形提交在 ALL_COMMANDS 等本批票据保护;
    // 写侧由"槽位只来自 bump 或排空点归还"保证无竞态
    let mut record_bytes = 0u64;
    for (slot, record) in staged_records {
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
    // 跨族内存可见性不靠 release/acquire 成对:图像与池一样走 CONCURRENT 双族共享,
    // 批次的图像收尾是纯布局转换,可见性由图形提交等票据信号量收口(见
    // render/frame/record.rs 提交段注释)。
    // 一次合批 = 一个 staging 范围 + 一次 transfer 提交;staging 非空必有提交
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
    // 驻留登记只在提交成功之后。持失效凭据的身份走 replace 换条目(旧区间进停尸房,
    // 排空后归还),其余走 commit 严格登记(重复登记按编排 bug 报错)
    let mut replaced_meshes = 0usize;
    let mut committed_meshes: Vec<AssetId<Mesh>> = Vec::with_capacity(planned.len());
    let planned_count = planned.len();
    for (id, replace, c, vertex, index) in planned {
        if replace {
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
    // 槽位发布:提交成功后分槽(采样器按功能键去重)并随驻留登记。发布失败 = 槽容量
    // 耗尽,致命退出,不静默截断。
    // 坑:换内容必须"新槽发布 + 旧槽延迟回收",原槽重写对在飞 draw 不安全(UAB 不允许
    // 覆盖仍被未完成命令使用的描述符)
    // mip 链计数只算 blit 生成的未压缩图；BCn 图的链是烘焙预生成,单独计量
    let bcn_count = image_specs.iter().filter(|(.., bcn)| bcn.is_some()).count();
    let mip_chained = gpu_images
        .iter()
        .filter(|(gid, gpu)| {
            gpu.mip_levels() > 1
                && image_specs
                    .iter()
                    .find(|(id, ..)| id == gid)
                    .is_some_and(|(.., bcn)| bcn.is_none())
        })
        .count();
    let mut sampler_slots_hit = HashSet::new();
    let mut replaced_images = 0usize;
    for ((id, _, spec, replace, _), (gid, gpu)) in image_specs.iter().zip(gpu_images) {
        // 两表同序构建,错位会把视图挂到错误身份上
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
        // 旧条目随登记退场(旧图进停尸房、旧纹理槽进待回收账):在飞 draw 经旧槽读旧图
        // 直到排空,新 draw 经新槽读新图
        image_cache.swap_commit(*id, gpu, ticket, slots, tables);
    }
    // 成功的身份销账:失效凭据消费,pending 身份到货出账(等待帧数计入到货计量)。
    // 未成功(未到货/被拒)的保留,行保持脏标下帧重试
    for id in committed_meshes {
        invalidations.consume_mesh(id);
        readiness.arrive_mesh(id);
        // LOD 档位到货 → 引用行重挂 LOD 脏标,draw_frame 随后两帧把新参数写进两个帧槽
        if let Some(base) = lod_groups.base_of(id) {
            for entity in ledger.rows_with_mesh(base) {
                ledger.mark_lod_dirty(entity);
            }
        }
    }
    for (id, ..) in &image_specs {
        invalidations.consume_image(*id);
        readiness.arrive_image(*id);
    }
    // 帧末对账:仍被引用的 pending 身份等待 +1,失去引用的出账计取消
    readiness.reconcile(&marked_meshes, &marked_images);
    let srgb_count = image_specs
        .iter()
        .filter(|(_, _, spec, ..)| spec.srgb_role())
        .count();
    // 计量:对象记录字节并入批次字节,槽位读数单独报
    let batch_bytes = vertex_bytes + index_bytes + image_bytes + record_bytes;
    let stats = &mut ledger.stats;
    stats.upload_batches_total += 1;
    stats.upload_bytes_total += batch_bytes;
    stats.last_batch_bytes = batch_bytes;
    stats.object_slots = usize::try_from(instance_tables.used_object_slots()).unwrap_or(usize::MAX);
    stats.material_slots =
        usize::try_from(instance_tables.used_material_slots()).unwrap_or(usize::MAX);
    let RowScan {
        materials_pending,
        failed_settled_rows,
        ..
    } = scan;
    info!(
        "上传批次 #{ticket}:mesh {} 个(其中换内容 {replaced_meshes},顶点 {vertex_bytes}B / 索引 {index_bytes}B)\
         + 贴图 {} 张(其中换内容重传 {replaced_images},{image_bytes}B,sRGB 角色 {srgb_count} / 线性 {},mip 链 {mip_chained} 张,BCn 直通 {bcn_count} 张)\
         + 对象记录 {} 条(换槽重写,{record_bytes}B);\
         槽位发布:纹理槽 {}/{}(含 fallback 0 号)、采样器槽 {} 种(本批涉及 {new_sampler});\
         实例槽 {}/{}(待回收 {})、材质槽 {}/{};\
         未到货跳过 mesh {pending_meshes} / 贴图 {pending_images}(材质缺 {materials_pending}),失败出队行 {failed_settled_rows};\
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
