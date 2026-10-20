//! 驻留管理器（6.3.1~6.3.4 + 6.4 细粒度扩展）：显存预算账 + 相机驱动的资产请求/淘汰——
//! "放不下的世界"的搬运工。纯 CPU 侧资源层新账（照 4.3 AssetReadiness
//! 先例），零新 Vulkan 形状：请求 = 引用行重挂脏标（重传链复用），淘汰 =
//! 卸载回收账进账（4.4 同入口），hazard 全由既有链闭合。
//!
//! 决策粒度（段计划待决 1 的施工钉明）：**按资产身份组建组**——共享同一
//! mesh 的行天然同进同出（= "chunk 级为主"的等价形状），引用计数由"身份
//! 只在其引用行距离满足请求条件时保留"兜底；测试场景用 unique 独立资产
//! 时粒度退化为行级（最细可测）。位置 = 引用行世界位置的最近距离。
//!
//! 请求侧（6.3.2）：非驻留身份的最近引用行距离 ≤ request_radius → 全部
//! 引用行 mark_dirty（flush 重传链：容器内资产查无 → 转换 → 上传 → commit，
//! 就绪才发布——判定线 3 的发布语义由 4.3/4.1 既有链承担，本模块零特判）。
//! **淘汰冷却**（防抖钉明，待决 2）：刚淘汰的身份进冷却表（默认 60 帧）——
//! 预算边界上"淘汰→重请求→再淘汰"的逐帧 thrash 被冷却窗吸收；回访恢复 =
//! 相机进入请求半径且冷却已过，数帧内重新驻留（延迟如实计量）。
//!
//! 淘汰侧（6.3.4）：used > 预算时，按"视锥外优先、距离远优先"排序逐个
//! 淘汰直到 ≤ 预算——**先撤引用后回收**的幽灵防线 = DrawList 可绘判定
//! （mesh 未驻留 = 行暂缓不进在场环，5.3 幽灵对象教训的枚举源头语义自动
//! 兜住，判定线 4）；回收复用 4.4 停尸房/free list（retirements 账同一排空点）。
//! 视锥判定复用 cull 的 CPU 参照件（frustum_planes + conservative_world_aabb
//! + aabb_visible，宁多留不错杀——保守方向与本仓剔除一致）。
//!
//! 预算口径（待决 3 钉明）：6.3 预算列 = **mesh 驻留字节**（顶点+索引，
//! recycle_ledger 现成读数）；贴图 bytes 进 census 计量但不受约束——贴图
//! 整资产淘汰会连带材质五槽反向映射与降级白图语义，归 6.4 子资产粒度
//! 一并做。staging 环不计入驻留账（瞬态）。
//!
//! A/B 面（判定线 5）：enabled=false（或预算 0）= 全常驻基线——管理器零
//! 动作，既有 4.x 链语义（进场即驻留永驻）。
//!
//! 6.4 细粒度扩展（`fine_grained` 开关，默认关 = 6.3 整资产粒度）：
//! - **LOD 档逐档驻留**（6.4.2/6.4.4）：LOD 组（6.2 登记）内 level 0 基准
//!   永驻（行可绘前提=回退恒有落点，判定线 2 粗档保底策略），level 1..3
//!   按"组内引用行的期望档位"逐档驻留/淘汰——期望档位 = 6.2 选档公式的
//!   CPU 参照（同 threshold 同 screen），管理器判定无迟滞（请求半径与冷却
//!   即迟滞，钉明）；档位淘汰**同步重挂行 LOD 脏标**（b7 表条目清哨兵——
//!   否则表里残留已回收区间的引脚，free list 复用后画错几何）；请求 =
//!   引用行 mark_dirty（行盘点的 LOD 组段一次收集全部未驻留档 = 判定线 3
//!   请求合并的结构形式）。GPU 侧 6.2 表内回退在本段被真实驱动（细档缺席
//!   → build_indirect 回退更细常驻档）。
//! - **贴图字节预算**（6.4.3）：image 驻留 bytes（resident_bytes 现读）进
//!   预算第二列；超支按"引用行最近距离远优先"淘汰整资产（eject_image 走
//!   同一回收账）——缺席档由 fallback 白图兜底（永驻 0 号槽 = 粗档保底的
//!   极端形式，零悬空采样由 fallback 槽保证，判定线 2 口径钉明）；重驻留 =
//!   材质五槽盘点（引用行 mark_dirty，与 mesh 请求同一次置脏 = 合并）。
//!   **整图替换（粗/细双 GpuImage swap）不做**——整资产粒度压测未证伪，
//!   替换式留待步骤 7 真实资产（待决 1 落定，见施工记录）。
//!
//! 6.5 低保真保底扩展（`floor_anchor` 开关，默认关 = 6.4 现状）：**可绘锚
//! 从最细基准迁到组内保底档**（最粗变体，level MAX-1）——基准档降为按需
//! 高档，高档/基准缺席时画面退到保底继续画而不是整行暂缓。三处联动：
//! - 需求集改形（6.5.1）：`needed[floor] = true` 取代 `needed[0] = true`
//!   （保底恒需求；基准只在有行期望 level 0 时进需求集）；档位淘汰循环
//!   从 level 0 起步（floor_anchor 隐含细粒度语义，与 fine_grained 取或）。
//! - 需求位图发布（6.5.3）：本 tick 把各组需求档算成位图挂 [`LodDemand`]，
//!   flush_uploads 同帧盘点按位图过滤——非需求档不再被顺手整组重传
//!   （6.4 遗留的"上传仅针对需求档"缺口在此闭合）；管理器关闭/B面 =
//!   位图清空，盘点回到不过滤的全常驻语义。
//! - 淘汰豁免：6.3 恒预算淘汰循环只按身份聚合（候选 = 引用行基准身份），
//!   保底档（变体）天然不在候选集；fine 循环按需求集淘汰、保底恒需求
//!   恒不淘汰——"淘汰永不触保底"由需求集结构性保证。
//!
//! 锚语义（6.5.2 施工钉明）：组的可绘锚 = 保底档驻留（DrawList 可绘判定与
//! flush 锚腿同源）；组尾档缺席（登记不完整）时锚退回基准（6.4 语义）；
//! 非 LOD 组资产锚恒为基准（本段不扩大范围——无变体即无保底可迁）。
//!
//! 6.6 GPU 缺口请求扩展（`gap_mode`，默认 Off = 6.5 现状）：GPU 在查引脚处
//! 写"本帧命中且未驻留"的档位位图（缺口标记环 b9），帧循环在 fence 纪律内
//! 采样成 [`GapSnapshot`]，本 tick 消费合并——**GPU 出意见、CPU 拍板**：
//! 缺口归并进需求位图（flush 过滤口）与请求行集，预算/冷却/淘汰裁决不变；
//! 缺口请求豁免请求半径（缺口 = 已命中可见证据，半径是预测口径）但保留
//! 冷却窗防抖。Off = 6.4/6.5 CPU 预测独走；GpuOnly = CPU 期望档预测请求
//! 让位（锚/缺口仍供需求）；Merged = 双源合并（A/B 的对照形态）。

use std::collections::HashMap;

use bevy::{
    camera::{Camera, Projection},
    pbr::StandardMaterial,
    prelude::*,
};
use ash_macros::system;

use crate::{
    driver::upload::flush_uploads,
    scene::{InstanceLedger, LodGroups},
    vulkan::{
        aabb_screen_rect, aabb_visible, conservative_world_aabb, frustum_planes, lod_raw_level,
        ImageCache, LodSettings, MAX_LOD_LEVELS, MeshPool, Swapchain,
    },
};

use super::asset_events::AssetRetirements;

/// 可绘锚档位（6.5.2 纯函数）：`floor_anchor` 开且保底档（最粗变体）在账 →
/// 保底档位号（MAX-1）；否则 0（基准档）。DrawList 可绘判定 / flush 锚腿 /
/// tick 需求集三处共用同一档位选择——锚解析不漂移；组尾档缺席（登记不
/// 完整）与非组行锚退回基准（6.5 范围钉明）。
#[must_use]
pub fn anchor_level(floor_anchor: bool, floor_present: bool) -> u32 {
    if floor_anchor && floor_present {
        MAX_LOD_LEVELS - 1
    } else {
        0
    }
}

/// 驻留管理器设置（BRP `residency_set` 写，residency_tick 读）。
#[derive(Resource)]
pub struct ResidencySettings {
    /// 管理器开关（false = 全常驻基线，A/B 的 B 面——判定线 5）。
    pub enabled: bool,
    /// mesh 驻留预算（字节；0 = 不设预算 = 全常驻，同样进 B 面口径）。
    pub budget_mesh_bytes: u64,
    /// 请求半径（世界单位）：引用行最近距离 ≤ 此值的身份必须驻留（尽力）。
    pub request_radius: f32,
    /// 淘汰冷却（帧）：刚淘汰的身份在冷却期内不被重请求（防预算边界 thrash）。
    pub eject_cooldown_frames: u32,
    /// 贴图驻留预算（字节；0 = 不设限——6.3 口径）。
    pub budget_image_bytes: u64,
    /// 细粒度开关（6.4）：true = LOD 档逐档驻留/淘汰（6.2 fallback 正式被
    /// 驱动）；false = 6.3 整资产粒度（档位全驻留，判定线 4 的 A/B 面）。
    pub fine_grained: bool,
    /// 低保真保底锚（6.5）：true = 组内行的可绘锚迁到最粗变体档（保底恒
    /// 驻留、基准按需）；false = 6.4 现状（基准锚）。隐含细粒度语义——
    /// 请求/淘汰/盘点过滤按 `fine_grained ∨ floor_anchor` 放行。
    pub floor_anchor: bool,
    /// GPU 缺口请求模式（6.6）：Off = CPU 预测独走（6.5 现状）；GpuOnly =
    /// CPU 期望档预测请求让位（锚/缺口仍供需求）；Merged = 双源合并。
    pub gap_mode: GapMode,
}

/// GPU 缺口请求模式（6.6）——见 [`ResidencySettings::gap_mode`]。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GapMode {
    /// CPU 预测独走（6.5 现状，A/B 的对照面）。
    #[default]
    Off,
    /// GPU 缺口独走：CPU 期望档预测请求关闭，缺口 + 锚供需求。
    GpuOnly,
    /// 双源合并：CPU 预测与 GPU 缺口都进请求集。
    Merged,
}

impl GapMode {
    /// BRP/读数面的形态名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::GpuOnly => "gpu_only",
            Self::Merged => "merged",
        }
    }

    /// BRP 参数解析（字符串形态）。
    ///
    /// # Errors
    /// 未知形态名。
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "off" => Ok(Self::Off),
            "gpu_only" => Ok(Self::GpuOnly),
            "merged" => Ok(Self::Merged),
            other => Err(format!("未知 gap_mode {other}（off/gpu_only/merged）")),
        }
    }
}

impl Default for ResidencySettings {
    fn default() -> Self {
        Self {
            enabled: false, // 默认关：不改变既有全常驻语义（BRP/场景脚本显式开启）
            budget_mesh_bytes: 0,
            request_radius: 40.0,
            eject_cooldown_frames: 60,
            budget_image_bytes: 0,
            fine_grained: false,
            floor_anchor: false,
            gap_mode: GapMode::Off,
        }
    }
}

/// 驻留管理器读数（每 tick 刷新，BRP census/egui 读数口；只进计量，
/// 不进绘制路径——5.2 诊断边界同款）。
#[derive(Resource, Default, Clone)]
pub struct ResidencyStats {
    pub enabled: bool,
    /// mesh 驻留预算（0 = 不设限）。
    pub budget_mesh_bytes: u64,
    /// mesh 驻留实占（顶点+索引，recycle_ledger 现读）。
    pub used_mesh_bytes: u64,
    /// 贴图驻留计量（张数——byte 账与约束归 6.4，口径钉明见模块注释）。
    pub resident_images: usize,
    /// 驻留 mesh 身份数。
    pub resident_meshes: usize,
    /// 请求半径内的非驻留身份（= 请求中/待重传，冷却期外）。
    pub requested: usize,
    /// 冷却期内的身份（刚淘汰，防抖窗）。
    pub cooling: usize,
    /// 淘汰累计（身份个数）。
    pub ejected_total: u64,
    /// 请求触发累计（mark_dirty 的行次）。
    pub requested_rows_total: u64,
    /// 本 tick 淘汰数/请求行数（逐 tick 刷新）。
    pub ejected_last: usize,
    pub requested_rows_last: usize,
    /// 预算装不下请求半径内全部身份（淘汰已触及请求区，如实报出）。
    pub over_budget: bool,
    /// 账本行数（规模口径）。
    pub rows: usize,
    // —— 6.4 细粒度段（fine_grained 开时填充）——
    /// 贴图驻留预算（0 = 不设限）。
    pub budget_image_bytes: u64,
    /// 贴图驻留实占（resident_bytes 现读）。
    pub used_image_bytes: u64,
    /// 贴图超预算（候选耗尽仍超，如实报出）。
    pub over_budget_image: bool,
    /// 细粒度开关快照。
    pub fine_grained: bool,
    /// LOD 变体档驻留条数（level 1..3 的组×档条目，6.4.2 读数）。
    pub lod_levels_resident: usize,
    /// LOD 变体档淘汰累计（条目数）。
    pub lod_levels_ejected_total: u64,
    /// 保底锚开关快照（6.5）。
    pub floor_anchor: bool,
    /// 保底档驻留的组数（登记组中保底变体在池的组数，判定线 1 的读数面）。
    pub floor_groups_resident: usize,
    // —— 6.6 GPU 缺口请求段（gap_mode != Off 时填充）——
    /// 缺口模式快照（读数面字符串 = GapMode::as_str）。
    pub gap_mode: &'static str,
    /// 快照中非零缺口槽数（逐槽位图非零的槽，采样帧 = 两帧前）。
    pub gap_slots_last: usize,
    /// 缺口去重后的基准身份数（组级归并，判定线 1"同身份去重"读数）。
    pub gap_assets_last: usize,
    /// 本 tick 缺口触发的置脏行数。
    pub gap_rows_last: usize,
    /// 缺口触发置脏行累计。
    pub gap_rows_total: u64,
    /// 陈旧缺口丢弃累计（槽已无对应行——快照两帧延迟内的行清账/槽回收）。
    pub gap_stale_total: u64,
}

/// 需求档位图（6.5.3）：residency_tick 每帧发布、flush_uploads 同帧盘点
/// 过滤消费（tick 挂 Last 先于 flush——发布即同帧生效）。键缺席 = 该帧
/// 不过滤（管理器关闭/B面——4.x 全常驻语义，盘点整组不过滤）。
#[derive(Resource, Default)]
pub struct LodDemand {
    /// 过滤激活（floor_anchor ∧ 管理器 active）——false 时 levels 清空。
    pub active: bool,
    /// 基准身份 → 需求档位位图（bit L = level L 本帧需求；保底档位恒置位）。
    pub levels: HashMap<AssetId<Mesh>, u32>,
}

/// GPU 缺口快照（6.6）：帧循环在 fence 纪律内读缺口标记环（本帧槽 = 两帧前
/// compute 输出）采样——residency_tick 与 BRP census 的读数源。tick 挂 Last
/// 在帧提交之后，直读环会与在飞 compute 写竞争，故采样收口在帧起（frame.rs）。
#[derive(Resource, Default)]
pub struct GapSnapshot {
    /// 采样覆盖的槽上界（= 采样帧的实例槽 bump bound）。
    pub bound: u32,
    /// 逐槽缺口位图（bit L = 该槽选中档 L 未驻留；索引 = 实例槽号）。
    pub bits: Vec<u32>,
}

/// 缺口位图 → 请求身份集（6.6.4 纯函数）：逐置位档位映射到该组对应档位
/// 资产身份（level 0 = 基准；越界位忽略——环内容异常不放大）。调用方做
/// 驻留复核/冷却/裁决。
#[must_use]
pub fn gap_asset_ids(
    bits: u32,
    levels: &[Option<Handle<Mesh>>; MAX_LOD_LEVELS as usize],
    base: AssetId<Mesh>,
) -> Vec<AssetId<Mesh>> {
    let mut out = Vec::new();
    for (level, handle) in levels.iter().enumerate() {
        if bits >> level & 1 == 0 {
            continue;
        }
        let id = handle.as_ref().map(|h| h.id()).unwrap_or(base);
        out.push(id);
    }
    out
}

/// 管理器内部状态（冷却表：mesh/贴图身份 → 剩余冷却帧——两张表，键型
/// 不同分列，防抖语义同源）。
#[derive(Resource, Default)]
pub(crate) struct EjectCooldown {
    meshes: HashMap<AssetId<Mesh>, u32>,
    images: HashMap<AssetId<Image>, u32>,
}

/// 驻留管理器 tick（Last，先于 flush_uploads——请求/淘汰动作要进同一帧的
/// 上传账）：预算读数 → 请求侧（半径内非驻留身份重挂引用行）→ 淘汰侧
/// （超预算按视锥外∧距离远逐个出账）→ 冷却走账。相机缺席 = 整帧让路
///（场景装配问题，不动作）。
#[expect(
    clippy::too_many_arguments,
    reason = "bevy 系统的参数表即依赖注入清单（设置/读数/冷却/账本/回收账/两池/相机），逐项声明是框架惯例"
)]
#[system]
pub(crate) fn residency_tick(
    settings: ResMut<ResidencySettings>,
    mut stats: ResMut<ResidencyStats>,
    mut cooldown: ResMut<EjectCooldown>,
    mut ledger: ResMut<InstanceLedger>,
    mut retirements: ResMut<AssetRetirements>,
    mut lod_demand: ResMut<LodDemand>,
    snapshot: Res<GapSnapshot>,
    pool: Res<MeshPool>,
    image_cache: Res<ImageCache>,
    std_materials: Res<Assets<StandardMaterial>>,
    lod_groups: Res<LodGroups>,
    lod_settings: Res<LodSettings>,
    swapchain: Res<Swapchain>,
    cameras: Query<(&Projection, &GlobalTransform), With<Camera>>,
    // 激活态下降沿检测（Local）：active→inactive 时全行置脏——被淘汰资产
    // 无请求者会永远缺席，关闭 B 面（全常驻基线）必须恢复全量（④ 判定线）
    mut prev_active: Local<bool>,
    // 6.5 锚切换沿检测（Local）：floor_anchor 翻转 = 需求集与锚腿整体换形
    mut prev_floor: Local<bool>,
) {
    stats.enabled = settings.enabled;
    stats.budget_mesh_bytes = settings.budget_mesh_bytes;
    stats.budget_image_bytes = settings.budget_image_bytes;
    stats.fine_grained = settings.fine_grained;
    stats.floor_anchor = settings.floor_anchor;
    stats.rows = ledger.len();
    stats.ejected_last = 0;
    stats.requested_rows_last = 0;
    stats.floor_groups_resident = 0;
    stats.gap_mode = settings.gap_mode.as_str();
    stats.gap_slots_last = 0;
    stats.gap_assets_last = 0;
    stats.gap_rows_last = 0;
    // —— 6.5 锚切换沿：floor_anchor 翻转 = 全行置脏 + LOD 脏（记录引脚
    // 基准↔保底换写、b7 表重写、盘点按新位图过滤）。不受 active 闸门约束
    //——B面下全常驻，重标只多一次收敛盘点（bound 未变的行免换槽）。
    if *prev_floor != settings.floor_anchor && !ledger.is_empty() {
        let entities: Vec<Entity> = ledger.rows().map(|(e, _)| e).collect();
        let count = entities.len();
        for e in &entities {
            ledger.mark_dirty(*e);
            ledger.mark_lod_dirty(*e);
        }
        info!("保底锚切换为 {}：全行 {count} 行置脏 + LOD 脏（锚腿/需求位图换形）", settings.floor_anchor);
    }
    *prev_floor = settings.floor_anchor;
    let recycle = pool.recycle_ledger();
    let mut used = recycle.resident_vertex_bytes + recycle.resident_index_bytes;
    stats.used_mesh_bytes = used;
    let mut used_image = image_cache.resident_bytes();
    stats.used_image_bytes = used_image;
    stats.resident_images = image_cache.resident_count();
    stats.resident_meshes = pool.resident_count();
    stats.cooling = cooldown.meshes.len() + cooldown.images.len();
    let active = settings.enabled
        && (settings.budget_mesh_bytes > 0 || settings.budget_image_bytes > 0)
        && !ledger.is_empty();
    if !active {
        if *prev_active && !ledger.is_empty() {
            // 激活→关闭的下降沿：全行置脏——被淘汰的资产没有请求者会永远
            // 缺席（实抓：④ 关后 used 停在被淘汰水平），B 面"全常驻基线"
            // 的完整语义 = 恢复 4.x 既有行为（容器内资产全部重驻留）。
            let entities: Vec<Entity> = ledger.rows().map(|(e, _)| e).collect();
            let count = entities.len();
            for e in entities {
                ledger.mark_dirty(e);
            }
            info!("驻留管理器关闭：全行置脏 {count} 行（恢复全常驻基线，重传链逐帧消化）");
        }
        *prev_active = active;
        stats.requested = 0;
        stats.over_budget = false;
        stats.over_budget_image = false;
        // B面：需求位图清空 = flush 盘点回到不过滤的全常驻语义（6.5.3）
        lod_demand.active = false;
        lod_demand.levels.clear();
        return; // B 面（全常驻基线）或空场：沿处理外零动作，读数照刷
    }
    *prev_active = true;
    // 激活帧：过滤激活态随 floor_anchor 走，位图逐组重算（下翻）
    lod_demand.active = settings.floor_anchor;
    lod_demand.levels.clear();
    let Ok((projection, cam_global)) = cameras.single() else {
        return; // 相机缺席：场景装配问题，整帧让路不动作
    };
    let cam_pos = cam_global.translation();
    let view_proj = projection.get_clip_from_view() * cam_global.affine().inverse();
    let planes = frustum_planes(&view_proj);

    // —— 身份聚合（账本只读走访）：mesh 身份 → (最近引用行距离, 任一行视锥内,
    //     引用行清单)。驻留判定 = pool.resident（4.1 语义同源）。
    struct Group {
        min_dist: f32,
        in_frustum: bool,
        rows: Vec<Entity>,
        resident: bool,
    }
    let mut groups: HashMap<AssetId<Mesh>, Group> = HashMap::new();
    for (entity, row) in ledger.rows() {
        let id = row.mesh.id();
        let dist = (row.model.w_axis.truncate() - cam_pos).length();
        let g = groups.entry(id).or_insert(Group {
            min_dist: f32::INFINITY,
            in_frustum: false,
            rows: Vec::new(),
            resident: pool.resident(id).is_some(),
        });
        g.min_dist = g.min_dist.min(dist);
        g.rows.push(entity);
        if !g.in_frustum
            && let Some(slot) = pool.resident(id)
        {
            let (wmin, wmax) =
                conservative_world_aabb(&row.model, slot.local_aabb.0, slot.local_aabb.1);
            g.in_frustum = aabb_visible(&planes, wmin.into(), wmax.into());
        }
    }

    // —— 冷却走账：剩余帧递减，归零出表（mesh/贴图两表同款）。
    cooldown.meshes.retain(|_, frames| {
        *frames = frames.saturating_sub(1);
        *frames > 0
    });
    cooldown.images.retain(|_, frames| {
        *frames = frames.saturating_sub(1);
        *frames > 0
    });

    // —— 6.6 缺口合并（GPU 一手证据读回）：快照由帧循环在 fence 纪律内采样
    //（本帧槽 = 两帧前 compute 输出），tick 在此消费——逐槽位图经账本反查
    // 行与 LOD 组，聚合到基准身份（同身份多实例去重 = 槽级位图 + 组级归并）。
    // 缺口只出"建议"：进需求位图与请求行集，预算/冷却/淘汰裁决仍在 CPU
    //（判定线 4）；快照两帧延迟内的行清账/槽回收 = 陈旧缺口如实计数丢弃。
    let mut gap_by_base: HashMap<AssetId<Mesh>, u32> = HashMap::new();
    if settings.gap_mode != GapMode::Off && !snapshot.bits.is_empty() {
        let mut slot_bases: HashMap<u32, AssetId<Mesh>> = HashMap::new();
        for (_entity, row) in ledger.rows() {
            if let Some(slot) = row.instance_slot {
                slot_bases.insert(slot, row.mesh.id());
            }
        }
        for (slot, bits) in snapshot.bits.iter().enumerate() {
            if *bits == 0 {
                continue;
            }
            stats.gap_slots_last += 1;
            let Some(&base) = slot_bases.get(&(slot as u32)) else {
                stats.gap_stale_total += 1;
                continue;
            };
            *gap_by_base.entry(base).or_insert(0u32) |= bits;
        }
    }
    // 缺口请求裁决（CPU 拍板）：冷却窗仍适用（防抖）；请求半径不适用——缺口
    // 是"本帧已命中可见"的证据，半径是相机预测口径；逐档复核驻留（两帧延迟
    // 内可能已到货），仍缺的才触发行置脏。
    let mut gap_rows: Vec<Entity> = Vec::new();
    if !gap_by_base.is_empty() {
        for (base_id, bits) in &gap_by_base {
            let Some(g) = groups.get(base_id) else { continue };
            let Some(levels) = lod_groups.levels_of(*base_id) else { continue };
            if cooldown.meshes.contains_key(base_id) {
                continue;
            }
            let any_missing = gap_asset_ids(*bits, levels, *base_id)
                .iter()
                .any(|id| pool.resident(*id).is_none());
            if any_missing {
                stats.gap_assets_last += 1;
                gap_rows.extend(g.rows.iter().copied());
            }
        }
    }

    // —— 请求侧：非驻留 ∧ 半径内 ∧ 冷却已过 → 全部引用行 mark_dirty（重传链
    // 复用；行已在脏集 = 幂等无成本）。
    let mut request_rows: Vec<Entity> = Vec::new();
    let mut requested_ids = 0usize;
    for (id, g) in &groups {
        if g.resident || g.min_dist > settings.request_radius {
            continue;
        }
        if cooldown.meshes.contains_key(id) {
            continue; // 冷却期：刚淘汰的身份不重请求（防抖窗，钉明见模块注释）
        }
        // 6.5：组资产的供给归需求位图（基准 = 按需高档，按期望档进需求集；
        // 按半径整档请求会把最贵基准档无条件拉回——上传按需的反面）
        if settings.floor_anchor && lod_groups.levels_of(*id).is_some() {
            continue;
        }
        requested_ids += 1;
        request_rows.extend(g.rows.iter().copied());
    }
    for entity in &request_rows {
        ledger.mark_dirty(*entity);
    }
    stats.requested = requested_ids;
    stats.requested_rows_last = request_rows.len();
    stats.requested_rows_total += request_rows.len() as u64;
    let mut request_rows_all = request_rows;
    let mut lod_levels_resident = 0usize;
    let mut defer_image_request: Vec<Entity> = Vec::new();
    let mut over_budget_image = false;

    // —— 淘汰侧：used > 预算 → 视锥外优先、距离远优先逐个出账，直到 ≤ 预算
    //（或可淘汰者耗尽——over_budget 如实报出，不硬凑：请求区内身份也按序
    // 淘汰时说明预算装不下需求，判定线 1 的读数面让位给诚实计量）。
    stats.over_budget = false;
    // 预算 0 = 不设限（约定）：淘汰只在预算非零时执法——0 与 used>0 恒真
    // 会把"不限"读成"全淘汰"（贴图账联调实抓：mesh 预算 0 + 贴图预算激活
    // 时全场景 mesh 无限拉锯，行永在置脏集，贴图 eject 被排除法饿死）
    if settings.budget_mesh_bytes > 0 && used > settings.budget_mesh_bytes {
        let mut candidates: Vec<(AssetId<Mesh>, f32, u64)> = groups
            .iter()
            .filter(|(_, g)| g.resident)
            .map(|(id, g)| {
                let slot = pool.resident(*id).expect("resident 标记来自同一次查询");
                (*id, g.min_dist, slot.vertex.size + slot.index.size)
            })
            .collect();
        // 排序键：(视锥内, 距离)——视锥外的先淘汰、同状态距离远的先淘汰。
        // in_frustum 取反进键，sort 升序 = 视锥外(false=0) 在前、距离大在前。
        candidates.sort_by(|a, b| {
            let fa = groups[&a.0].in_frustum;
            let fb = groups[&b.0].in_frustum;
            fb.cmp(&fa).then(b.1.total_cmp(&a.1))
        });
        for (id, _dist, bytes) in candidates {
            if used <= settings.budget_mesh_bytes {
                break;
            }
            // 出账进卸载回收账（4.4 同入口）：flush 下帧开头出驻留条目 →
            // 停尸房 → 排空后区间归还 free list；引用行此后 DrawList 暂缓
            //（mesh 未驻留 = 不进在场环——先撤引用后回收的幽灵防线）。
            retirements.eject_mesh(id);
            cooldown.meshes.insert(id, settings.eject_cooldown_frames);
            used = used.saturating_sub(bytes);
            stats.ejected_total += 1;
            stats.ejected_last += 1;
        }
        stats.over_budget = used > settings.budget_mesh_bytes;
    }
    stats.used_mesh_bytes = used;

    // —— 6.4.2 LOD 档逐档驻留（fine_grained）：组内期望档位（6.2 选档公式
    // CPU 参照同源）决定 level 1..3 的驻留需求——需求档未驻留 → 引用行
    // 置脏（行盘点的 LOD 组段一次收集全部未驻留档 = 请求合并）；非需求档
    // 驻留中 → 淘汰 + 重挂 LOD 脏标（b7 表条目清哨兵——残留引脚在 free
    // list 复用后会画错几何）。level 0 基准永驻（回退恒有落点，判定线 2）。
    // 档位驻留计数两面统一（fine_grained 关 = 档位全驻留的读数也要如实）
    {
        let screen = {
            let (w, h) = (swapchain.extent.width, swapchain.extent.height);
            [w as f32, h as f32]
        };
        let fine = settings.fine_grained || settings.floor_anchor;
        let floor_level_idx = (MAX_LOD_LEVELS - 1) as usize;
        for (base_id, g) in groups.iter() {
            let Some(group_levels) = lod_groups.levels_of(*base_id) else { continue }; // 非阶梯 mesh 无档位账
            // 期望档位并集：粗档保底之外，每个引用行的目标档都应驻留。
            // GpuOnly（6.6）：CPU 期望档预测请求让位——期望档不进需求集，
            // GPU 缺口 + 锚供需求（A/B 的 GPU 独走面）。
            let mut needed = [false; MAX_LOD_LEVELS as usize];
            // 锚档恒需求（6.5）：floor_anchor = 保底档（最粗变体）常驻、基准
            // 降为按需高档；组尾档缺席（登记不完整）锚退回基准 = 6.4 语义
            needed[anchor_level(settings.floor_anchor, group_levels[floor_level_idx].is_some())
                as usize] = true;
            if settings.gap_mode != GapMode::GpuOnly {
                for (entity, row) in ledger.rows() {
                    if !g.rows.contains(&entity) {
                        continue;
                    }
                    // 期望档计算优先取基准槽位；基准缺席（6.5 按需高档被淘汰）
                    // 退锚槽位——保底变体与基准同源同界近似，跳过会让基准一旦
                    // 淘汰就永远算不出期望档（高档永不回流）
                    let Some(slot) = pool.resident(row.mesh.id()).or_else(|| {
                        group_levels[floor_level_idx].as_ref().and_then(|h| pool.resident(h.id()))
                    }) else { continue };
                    if let Some((px_min, px_max, _)) = aabb_screen_rect(
                        &view_proj, &row.model, slot.local_aabb.0, slot.local_aabb.1, screen,
                    ) {
                        let size = (px_max.x - px_min.x).max(px_max.y - px_min.y);
                        let raw = lod_raw_level(size, lod_settings.threshold_px, MAX_LOD_LEVELS);
                        needed[raw as usize] = true;
                    }
                }
            }
            // 6.6 缺口归并：GPU 选档缺口并入需求集（merged）——位图发布与
            // 淘汰豁免都以 merged 为准（CPU 拍板 = 接受缺口进需求，裁决权与
            // 预算不变）；merged[锚] 恒真（保底豁免结构性保持）。
            let gap_mask = gap_by_base.get(base_id).copied().unwrap_or(0u32);
            let mut merged = needed;
            for (level, merged_l) in merged.iter_mut().enumerate() {
                *merged_l |= gap_mask >> level & 1 == 1;
            }
            // 需求位图发布（6.5.3 + 6.6 归并）：tick 算好的需求集交同帧 flush
            // 过滤盘点——缺口点名的档位由此获得上传通道
            if settings.floor_anchor {
                let mut mask = 0u32;
                for (level, needed_l) in merged.iter().enumerate() {
                    if *needed_l {
                        mask |= 1 << level;
                    }
                }
                lod_demand.levels.insert(*base_id, mask);
            }
            // 保底驻留计数（判定线 1 读数面；与需求无关，只报账面事实）
            if let Some(handle) = &group_levels[floor_level_idx]
                && pool.resident(handle.id()).is_some()
            {
                stats.floor_groups_resident += 1;
            }
            // 请求合并：需求档未驻留 → 一次行置脏（盘点收集全部未驻留档）。
            // **全档检查**（6.5 施工订正）：6.4 时 needed[0] 恒真且基准恒驻留
            // 可以 skip(1)；floor_anchor 下基准降为可淘汰高档，level 0 缺失
            // 也要触发请求（否则高档淘汰后永不回流）。驻留计数保持 6.4 口径
            //（只计变体档，level 0 不入 lod_levels_resident）
            let mut any_needed_missing = false;
            for (level, needed_l) in needed.iter().enumerate() {
                if !*needed_l { continue; }
                let Some(handle) = &group_levels[level] else { continue };
                if pool.resident(handle.id()).is_none() {
                    any_needed_missing = true;
                } else if level != 0 {
                    lod_levels_resident += 1;
                }
            }
            if fine
                && settings.gap_mode != GapMode::GpuOnly
                && any_needed_missing
                && g.min_dist <= settings.request_radius
                && !cooldown.meshes.contains_key(base_id)
            {
                request_rows_all.extend(g.rows.iter().copied());
            }
            // 非需求档淘汰（距离远优先；冷却窗同源；fine ∨ floor_anchor 开）。
            // 6.6：以 merged 为"需求"口径——缺口点名的档位不进淘汰候选
            //（GPU 要的 CPU 已接受进需求；淘汰权本身仍在 CPU，判定线 4）。
            if !fine {
                continue;
            }
            // floor_anchor 下淘汰从 level 0 起步——基准降为可淘汰高档（保底
            // 恒需求恒不淘汰，"淘汰永不触保底"由需求集结构性保证）
            let eject_start = usize::from(!settings.floor_anchor);
            for (level, needed_l) in merged.iter().enumerate().skip(eject_start) {
                if *needed_l { continue; }
                let Some(handle) = &group_levels[level] else { continue };
                let level_id = handle.id();
                let Some(slot) = pool.resident(level_id) else { continue };
                if cooldown.meshes.contains_key(&level_id) { continue; }
                retirements.eject_mesh(level_id);
                cooldown.meshes.insert(level_id, settings.eject_cooldown_frames);
                used = used.saturating_sub(slot.vertex.size + slot.index.size);
                stats.lod_levels_ejected_total += 1;
                // b7 表条目清账：变体区间将进 free list，残留引脚 = 画错几何
                for entity in &g.rows {
                    ledger.mark_lod_dirty(*entity);
                }
            }
        }
    }
    stats.lod_levels_resident = lod_levels_resident;
    // 6.6 缺口请求行并入（缺口分支已做冷却/驻留复核/组级去重）——与 CPU
    // 预测请求同一条置脏通道，幂等续期（行在脏集 = 免费）
    request_rows_all.extend(gap_rows.iter().copied());
    stats.gap_rows_last = gap_rows.len();
    stats.gap_rows_total += gap_rows.len() as u64;
    for entity in &request_rows_all {
        ledger.mark_dirty(*entity);
    }
    stats.requested_rows_total += request_rows_all.len() as u64;
    // 本 tick 已置脏的行集（贴图段排除用——置脏行的材质腿会同帧重传贴图，
    // 对其贴图 eject = 同帧白 eject，结构冲突，见施工记录首钉）
    let requested_row_set: std::collections::HashSet<Entity> =
        request_rows_all.iter().copied().collect();

    // —— 6.4.3 贴图字节预算：材质→行距离聚合 → 超支按距离远优先淘汰整资产
    //（eject_image 同一回收账）；缺席档 = fallback 白图（永驻 0 号槽）兜底，
    // 重驻留 = 材质五槽盘点（引用行置脏，与 mesh 请求同一次置脏 = 合并）。
    {
        // 材质 → (最近引用行距离, 引用行清单)；贴图 → (最近距离, 引用行清单)
        let mut mat_dist: HashMap<AssetId<StandardMaterial>, (f32, Vec<Entity>)> = HashMap::new();
        for (entity, row) in ledger.rows() {
            let dist = (row.model.w_axis.truncate() - cam_pos).length();
            let e = mat_dist.entry(row.material.id()).or_insert((f32::INFINITY, Vec::new()));
            e.0 = e.0.min(dist);
            e.1.push(entity);
        }
        let mut image_groups: HashMap<AssetId<Image>, (f32, Vec<Entity>)> = HashMap::new();
        for (mat_id, (dist, rows)) in &mat_dist {
            let Some(material) = std_materials.get(*mat_id) else { continue };
            for handle in [
                &material.base_color_texture,
                &material.metallic_roughness_texture,
                &material.occlusion_texture,
                &material.normal_map_texture,
                &material.emissive_texture,
            ]
            .into_iter()
            .flatten()
            {
                if !image_cache.resident(handle.id()).is_some() {
                    continue; // 未驻留贴图不参与聚合（请求由行腿覆盖）
                }
                let e = image_groups.entry(handle.id()).or_insert((f32::INFINITY, Vec::new()));
                e.0 = e.0.min(*dist);
                e.1.extend(rows.iter().copied());
            }
        }
        if settings.budget_image_bytes > 0 && used_image > settings.budget_image_bytes {
            let mut candidates: Vec<(AssetId<Image>, f32, u64)> = image_groups
                .iter()
                .filter_map(|(id, (dist, _))| {
                    let bytes = image_cache.image(*id).map(|g| g.alloc_bytes())?;
                    Some((*id, *dist, bytes))
                })
                .collect();
            candidates.sort_by(|a, b| b.1.total_cmp(&a.1)); // 距离远优先
            for (id, _dist, bytes) in candidates {
                if used_image <= settings.budget_image_bytes {
                    break;
                }
                if cooldown.images.contains_key(&id) { continue; }
                // 本 tick 已置脏行的贴图跳过：材质腿同帧重传会让 eject 空转
                //（拉锯），排除后下一 tick 行稳定再淘汰
                if image_groups[&id].1.iter().any(|r| requested_row_set.contains(r)) {
                    continue;
                }
                retirements.eject_image(id);
                cooldown.images.insert(id, settings.eject_cooldown_frames);
                used_image = used_image.saturating_sub(bytes);
                // 重驻留通道 = 下方"预算内"分支（冷却过期 ∧ 半径内 → 引用行
                // 置脏重传）——eject 当帧不排队（同帧重传 = 自我拉锯，实抓：
                // used 恒定全量，eject 空转）
            }
            over_budget_image = used_image > settings.budget_image_bytes;
        } else if settings.budget_image_bytes > 0 {
            // 预算内：半径内缺席贴图照常请求（与 mesh 请求同拍）
            for (id, (dist, rows)) in &image_groups {
                if *dist <= settings.request_radius
                    && !image_cache.resident(*id).is_some()
                    && !cooldown.images.contains_key(id)
                {
                    defer_image_request.extend(rows.iter().copied());
                }
            }
        }
        for entity in &defer_image_request {
            ledger.mark_dirty(*entity);
        }
        stats.over_budget_image = over_budget_image;
        stats.used_image_bytes = used_image;
    }
}

/// 驻留管理器插件：设置/读数/冷却三资源 + tick 挂 Last（先于
/// flush_uploads——动作进同一帧上传账）。默认关（全常驻语义不变），
/// BRP `residency_set` 或场景脚本显式开启。
pub struct AshResidencyPlugin;

impl Plugin for AshResidencyPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ResidencySettings>()
            .init_resource::<ResidencyStats>()
            .init_resource::<EjectCooldown>()
            .init_resource::<LodDemand>()
            .init_resource::<GapSnapshot>()
            .add_systems(
                Last,
                residency_tick
                    .before(flush_uploads)
                    .run_if(resource_exists::<MeshPool>)
                    .run_if(resource_exists::<crate::vulkan::BindlessTables>)
                    .run_if(resource_exists::<AssetRetirements>)
                    .run_if(resource_exists::<Swapchain>)
                    .run_if(resource_exists::<LodSettings>),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::{anchor_level, gap_asset_ids, GapMode, MAX_LOD_LEVELS};
    use bevy::prelude::*;

    /// 可绘锚档位（6.5.2 判定前提的三处共用纯函数）：关闭 = 基准（6.4 语义）；
    /// 开启 = 保底档（最粗变体）；组尾档缺席（登记不完整）退回基准。
    #[test]
    fn anchor_level_resolution() {
        assert_eq!(anchor_level(false, true), 0, "锚关闭 = 基准档（6.4 语义）");
        assert_eq!(anchor_level(false, false), 0);
        assert_eq!(
            anchor_level(true, true),
            MAX_LOD_LEVELS - 1,
            "锚开启 = 保底档（最粗变体）"
        );
        assert_eq!(anchor_level(true, false), 0, "尾档缺席退回基准（登记不完整）");
    }

    /// 缺口位图 → 请求身份集（6.6.4 判定线 1 的去重前）：逐置位档位映射
    /// 资产身份（level 0 = 基准），零位图空集，越界位忽略（环内容异常不放大）。
    #[test]
    fn gap_asset_ids_unpack() {
        let mut meshes = Assets::<Mesh>::default();
        let sphere = || {
            Sphere::new(0.5)
                .mesh()
                .ico(1)
                .expect("细分上限内不失败")
        };
        let handles: Vec<Handle<Mesh>> = (0..MAX_LOD_LEVELS).map(|_| meshes.add(sphere())).collect();
        let mut levels: [Option<Handle<Mesh>>; MAX_LOD_LEVELS as usize] = Default::default();
        for (i, h) in handles.iter().enumerate() {
            levels[i] = Some(h.clone());
        }
        let base = handles[0].id();
        assert!(gap_asset_ids(0, &levels, base).is_empty(), "零位图 = 无缺口");
        let ids = gap_asset_ids(0b1001, &levels, base);
        assert_eq!(ids, vec![base, handles[3].id()], "bit0 与 bit3 → 基准与保底");
        // 越界位忽略（bit 4.. 不在 MAX 内）
        let only_inrange = gap_asset_ids(u32::MAX, &levels, base);
        assert_eq!(only_inrange.len(), MAX_LOD_LEVELS as usize);
    }

    /// GapMode 字符串往返与默认（6.6 BRP 通道的解析纪律：未知形态报错不猜）。
    #[test]
    fn gap_mode_parse_roundtrip() {
        assert_eq!(GapMode::default(), GapMode::Off);
        for mode in [GapMode::Off, GapMode::GpuOnly, GapMode::Merged] {
            assert_eq!(GapMode::parse(mode.as_str()), Ok(mode));
        }
        assert!(GapMode::parse("wat").is_err());
    }
}
