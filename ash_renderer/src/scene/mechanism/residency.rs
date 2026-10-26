//! 驻留管理器：显存预算账 + 相机驱动的资产请求/淘汰——"放不下的世界"的
//! 搬运工。纯 CPU 侧资源层账，不新增 Vulkan 形状：请求 = 引用行重挂脏标
//!（复用 flush 重传链：容器内资产查无 → 转换 → 上传 → commit，就绪才发布），
//! 淘汰 = 卸载回收账进账（[`AssetRetirements`]，与资产 Removed 同入口），
//! hazard 全由既有链闭合。
//!
//! 调度：[`residency_tick`] 挂 Last 的 [`ResidencySet`]，driver 声明上传编排
//!（`flush_uploads`）排在该 set 之后——本 tick 的请求/淘汰与 [`LodDemand`]
//! 位图同帧进上传账。
//!
//! 决策粒度：按 mesh 资产身份聚合——共享同一 mesh 的行同进同出，位置 = 引用行
//! 世界位置的最近距离；unique 独立资产场景下退化为行级。视锥判定复用 cull 的
//! CPU 参照件（frustum_planes + conservative_world_aabb + aabb_visible，宁多留
//! 不错杀）。预算两列：mesh 驻留字节（顶点+索引，recycle_ledger 现读）与贴图
//! 驻留字节（resident_bytes 现读）；staging 环是瞬态，不计入。
//!
//! 全量 tick 的段序：
//! 1. 身份聚合 → 冷却走账；
//! 2. GPU 缺口合并（`gap_mode != Off`）：[`GapSnapshot`] 逐槽位图经账本反查
//!    聚合到基准身份，仍缺且不在冷却的组进请求行集；
//! 3. 半径请求：非驻留 ∧ 最近距离 ≤ request_radius ∧ 冷却已过 → 引用行进
//!    请求行集（floor_anchor 下 LOD 组资产归需求位图供给，不走半径请求）；
//! 4. mesh 预算淘汰：used > budget_mesh_bytes 时按"视锥外优先、距离远优先"
//!    逐个出账，直到 ≤ 预算或候选耗尽（over_budget 如实报出）；
//! 5. LOD 档位段：组内期望档（选档公式 CPU 参照，同 threshold 同 screen，
//!    无迟滞——请求半径与冷却即迟滞）∪ 锚档 ∪ 缺口档 = 需求集；`fine_grained ∨
//!    floor_anchor` 时需求档缺席 → 请求、非需求档驻留 → 淘汰；floor_anchor 下
//!    需求集发布为 [`LodDemand`] 位图供 flush 过滤盘点；
//! 6. 请求行集统一 mark_dirty（行已在脏集 = 幂等无成本）；
//! 7. 贴图预算段：贴图按引用行最近距离聚合，超支远优先整资产淘汰（缺席档由
//!    永驻 0 号槽的 fallback 白图兜底），预算内则半径内缺席贴图照常请求。
//!
//! 锚语义（[`anchor_level`]）：floor_anchor 开且组尾档（最粗变体）在账 → 可绘
//! 锚 = 保底档（恒需求、恒不淘汰，基准降为按需高档）；否则锚 = 基准。"淘汰
//! 永不触保底"由需求集结构性保证：mesh 预算淘汰只按基准身份聚合，档位淘汰
//! 只淘汰非需求档。
//!
//! 缺口模式（[`GapMode`]）：GPU 出意见、CPU 拍板——缺口只进需求位图与请求
//! 行集，预算/冷却/淘汰裁决不变；缺口请求豁免请求半径（缺口 = 本帧已命中可见
//! 的证据，半径是相机预测口径）但保留冷却窗。
//!
//! B 面：enabled=false 或两项预算皆 0 = 全常驻基线——管理器零动作、需求位图
//! 清空（flush 盘点不过滤）。
//!
//! 坑点：
//! - **淘汰冷却**：刚淘汰的身份进冷却表（默认 60 帧），否则预算边界上
//!   "淘汰→重请求→再淘汰"逐帧 thrash。
//! - **关闭下降沿必须全行置脏**：被淘汰资产没有请求者会永远缺席（关闭后
//!   used 停在被淘汰水平）；floor_anchor 翻转同理全行置脏 + LOD 脏。
//! - **档位淘汰必须重挂行 LOD 脏标**：b7 表残留已回收区间的引脚，free list
//!   复用后画错几何。
//! - **基准缺席时期望档退锚槽位计算、档位请求全档检查（含 level 0）**：基准
//!   在 floor_anchor 下可被淘汰，跳过会让期望档永远算不出、高档永不回流。
//! - **预算 0 = 不设限**：淘汰只在预算非零时执法，否则 mesh 预算 0 + 贴图预算
//!   激活时全场 mesh 无限拉锯（行永在置脏集），贴图淘汰被排除法饿死。
//! - **贴图淘汰跳过本 tick 已置脏行的贴图**：置脏行的材质腿同帧重传贴图，对其
//!   eject = 空转拉锯；被淘汰贴图当帧也不排重请求。
//! - **全静闸门不在冷却表非空时早退**：冷却到期→重请求是时间驱动的状态转移，
//!   静态相机下基准回流依赖它。
//!
//! 深原理与验收证据：`.agents/docs/6-高级剔除LOD与流送/`（粗/细粒度流送、
//! 低保真常驻、GPU 需求驱动各专题）。

use std::collections::{HashMap, HashSet};

use bevy::{
    camera::{Camera, Projection},
    ecs::system::SystemParam,
    pbr::StandardMaterial,
    prelude::*,
};
use ash_macros::system;

use crate::{
    scene::{InstanceLedger, LodGroups},
    render::{
        aabb_screen_rect, aabb_visible, conservative_world_aabb, frustum_planes, lod_raw_level,
        ImageCache, LodSettings, MAX_LOD_LEVELS, MeshPool, Swapchain,
    },
};

use super::asset_events::AssetRetirements;

/// LOD 档位数（数组长度口径）。
const LEVELS: usize = MAX_LOD_LEVELS as usize;
/// 保底档下标（组尾档 = 最粗变体）。
const FLOOR_LEVEL: usize = LEVELS - 1;

/// 可绘锚档位（纯函数）：`floor_anchor` 开且保底档（最粗变体）在账 →
/// 保底档位号（MAX-1）；否则 0（基准档）。DrawList 可绘判定 / flush 锚腿 /
/// tick 需求集三处共用同一档位选择——锚解析不漂移；组尾档缺席（登记不
/// 完整）与非组行锚退回基准。
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
    /// 管理器开关（false = 全常驻基线，A/B 的 B 面）。
    pub enabled: bool,
    /// mesh 驻留预算（字节；0 = 不设预算 = 全常驻，同样进 B 面口径）。
    pub budget_mesh_bytes: u64,
    /// 请求半径（世界单位）：引用行最近距离 ≤ 此值的身份必须驻留（尽力）。
    pub request_radius: f32,
    /// 淘汰冷却（帧）：刚淘汰的身份在冷却期内不被重请求（防预算边界 thrash）。
    pub eject_cooldown_frames: u32,
    /// 贴图驻留预算（字节；0 = 不设限）。
    pub budget_image_bytes: u64,
    /// 细粒度开关：true = LOD 档逐档驻留/淘汰（GPU 表内回退被真实驱动）；
    /// false = 整资产粒度（档位全驻留）。
    pub fine_grained: bool,
    /// 低保真保底锚：true = 组内行的可绘锚迁到最粗变体档（保底恒驻留、基准
    /// 按需）；false = 基准锚。隐含细粒度语义——请求/淘汰/盘点过滤按
    /// `fine_grained ∨ floor_anchor` 放行。
    pub floor_anchor: bool,
    /// GPU 缺口请求模式：Off = CPU 预测独走；GpuOnly = CPU 期望档预测请求
    /// 让位（锚/缺口仍供需求）；Merged = 双源合并。
    pub gap_mode: GapMode,
}

/// GPU 缺口请求模式——见 [`ResidencySettings::gap_mode`]。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GapMode {
    /// CPU 预测独走（A/B 的对照面）。
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
/// 不进绘制路径）。
#[derive(Resource, Default, Clone)]
pub struct ResidencyStats {
    pub enabled: bool,
    /// mesh 驻留预算（0 = 不设限）。
    pub budget_mesh_bytes: u64,
    /// mesh 驻留实占（顶点+索引，recycle_ledger 现读）。
    pub used_mesh_bytes: u64,
    /// 驻留贴图张数。
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
    // —— 细粒度段（fine_grained 开时填充）——
    /// 贴图驻留预算（0 = 不设限）。
    pub budget_image_bytes: u64,
    /// 贴图驻留实占（resident_bytes 现读）。
    pub used_image_bytes: u64,
    /// 贴图超预算（候选耗尽仍超，如实报出）。
    pub over_budget_image: bool,
    /// 细粒度开关快照。
    pub fine_grained: bool,
    /// LOD 变体档驻留条数（level 1..3 的组×档条目）。
    pub lod_levels_resident: usize,
    /// LOD 变体档淘汰累计（条目数）。
    pub lod_levels_ejected_total: u64,
    /// 保底锚开关快照。
    pub floor_anchor: bool,
    /// 保底档驻留的组数（登记组中保底变体在池的组数）。
    pub floor_groups_resident: usize,
    // —— GPU 缺口请求段（gap_mode != Off 时填充）——
    /// 缺口模式快照（读数面字符串 = GapMode::as_str）。
    pub gap_mode: &'static str,
    /// 快照中非零缺口槽数（逐槽位图非零的槽，采样帧 = 两帧前）。
    pub gap_slots_last: usize,
    /// 缺口去重后的基准身份数（组级归并）。
    pub gap_assets_last: usize,
    /// 本 tick 缺口触发的置脏行数。
    pub gap_rows_last: usize,
    /// 缺口触发置脏行累计。
    pub gap_rows_total: u64,
    /// 陈旧缺口丢弃累计（槽已无对应行——快照两帧延迟内的行清账/槽回收）。
    pub gap_stale_total: u64,
}

impl ResidencyStats {
    /// 逐 tick 动作读数清零（请求行/淘汰/缺口的 `*_last` 字段）。`requested`
    /// 与 `floor_groups_resident` 不在此列：各早退路径语义不同，由调用方定。
    fn reset_tick_counters(&mut self) {
        self.requested_rows_last = 0;
        self.ejected_last = 0;
        self.gap_slots_last = 0;
        self.gap_assets_last = 0;
        self.gap_rows_last = 0;
    }
}

/// 需求档位图：residency_tick 每帧发布、flush_uploads 同帧盘点过滤消费
///（tick 挂 Last 先于 flush——发布即同帧生效）。键缺席 = 该帧不过滤（管理器
/// 关闭/B 面——全常驻语义，盘点整组不过滤）。
#[derive(Resource, Default)]
pub struct LodDemand {
    /// 过滤激活（floor_anchor ∧ 管理器 active）——false 时 levels 清空。
    pub active: bool,
    /// 基准身份 → 需求档位位图（bit L = level L 本帧需求；保底档位恒置位）。
    pub levels: HashMap<AssetId<Mesh>, u32>,
}

/// GPU 缺口快照：帧循环在 fence 纪律内读缺口标记环（本帧槽 = 两帧前
/// compute 输出）采样——residency_tick 与 BRP census 的读数源。tick 挂 Last
/// 在帧提交之后，直读环会与在飞 compute 写竞争，故采样收口在帧起（frame.rs）。
#[derive(Resource, Default)]
pub struct GapSnapshot {
    /// 采样覆盖的槽上界（= 采样帧的实例槽 bump bound）。
    pub bound: u32,
    /// 逐槽缺口位图（bit L = 该槽选中档 L 未驻留；索引 = 实例槽号）。
    pub bits: Vec<u32>,
}

/// 缺口位图 → 请求身份集（纯函数）：逐置位档位映射到该组对应档位资产身份
///（level 0 = 基准；越界位忽略——环内容异常不放大）。调用方做驻留复核/
/// 冷却/裁决。
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

/// 全静闸门输入键：tick 输出的全部决定量——机位/投影（view_proj 一并覆盖
/// FOV/宽高比变化）、屏尺寸、LOD 判据、管理器预算/半径/开关。浮点量按位比较
///（NaN 不静——非数参数不通过闸门）。键全同 ∧ 账本无脏 ∧ 缺口全零 = 本 tick
/// 输出与上 tick 逐字段相同。
#[derive(Clone, PartialEq)]
pub(crate) struct TickGateKey {
    view_proj: Mat4,
    extent: [u32; 2],
    threshold_bits: u32,
    hysteresis_bits: u32,
    budget_mesh_bytes: u64,
    budget_image_bytes: u64,
    request_radius_bits: u32,
    fine_grained: bool,
    floor_anchor: bool,
    gap_mode_off: bool,
}

/// 管理器内部状态（冷却表：mesh/贴图身份 → 剩余冷却帧——两张表，键型
/// 不同分列，防抖语义同源）。
#[derive(Resource, Default)]
pub(crate) struct EjectCooldown {
    meshes: HashMap<AssetId<Mesh>, u32>,
    images: HashMap<AssetId<Image>, u32>,
}

impl EjectCooldown {
    /// 冷却走账：剩余帧递减，归零出表（mesh/贴图两表同款）。
    fn tick(&mut self) {
        self.meshes.retain(|_, frames| {
            *frames = frames.saturating_sub(1);
            *frames > 0
        });
        self.images.retain(|_, frames| {
            *frames = frames.saturating_sub(1);
            *frames > 0
        });
    }
}

/// tick 的只读资源束（SystemParam）：成排只读 Res 收进一束，系统参数表不
/// 越 16 项上限——与 upload.rs 的 UploadData / frame.rs 的 FrameInput 同款惯例。
#[derive(SystemParam)]
pub(crate) struct TickResources<'w> {
    pool: Res<'w, MeshPool>,
    image_cache: Res<'w, ImageCache>,
    lod_settings: Res<'w, LodSettings>,
    swapchain: Res<'w, Swapchain>,
    snapshot: Res<'w, GapSnapshot>,
}

/// 驻留管理器 tick（Last，先于 flush_uploads——请求/淘汰动作要进同一帧的
/// 上传账）：读数刷新 → 锚切换沿 → B 面早退（含下降沿恢复）→ 全静闸门 →
/// 全量段序（见模块文档）。相机缺席 = 整帧让路（场景装配问题，不动作）。
#[expect(
    clippy::too_many_arguments,
    reason = "bevy 系统的参数表即依赖注入清单（设置/读数/冷却/账本/回收账/相机），逐项声明是框架惯例"
)]
#[system]
pub(crate) fn residency_tick(
    settings: ResMut<ResidencySettings>,
    mut stats: ResMut<ResidencyStats>,
    mut cooldown: ResMut<EjectCooldown>,
    mut ledger: ResMut<InstanceLedger>,
    mut retirements: ResMut<AssetRetirements>,
    mut lod_demand: ResMut<LodDemand>,
    resources: TickResources,
    std_materials: Res<Assets<StandardMaterial>>,
    lod_groups: Res<LodGroups>,
    cameras: Query<(&Projection, &GlobalTransform), With<Camera>>,
    // 激活态下降沿检测：active→inactive 时全行置脏（见模块文档坑点）
    mut prev_active: Local<bool>,
    // 锚切换沿检测：floor_anchor 翻转 = 需求集与锚腿整体换形
    mut prev_floor: Local<bool>,
    // 全静闸门的相机键历史：None = 无历史（首激活 tick 必全量）
    mut prev_cam_key: Local<Option<TickGateKey>>,
) {
    let TickResources {
        ref pool,
        ref image_cache,
        ref lod_settings,
        ref swapchain,
        ref snapshot,
    } = resources;
    stats.enabled = settings.enabled;
    stats.budget_mesh_bytes = settings.budget_mesh_bytes;
    stats.budget_image_bytes = settings.budget_image_bytes;
    stats.fine_grained = settings.fine_grained;
    stats.floor_anchor = settings.floor_anchor;
    stats.rows = ledger.len();
    stats.gap_mode = settings.gap_mode.as_str();
    // 动作字段不在此清零：早退路径（B 面/全静闸门）沿用语义化清零或上 tick
    // 值，全量路径在闸门后统一清零（floor_groups_resident 等累计字段靠它）。
    // 锚切换沿：全行置脏 + LOD 脏（记录引脚基准↔保底换写、b7 表重写、盘点
    // 按新位图过滤）。不受 active 闸门约束——B 面下全常驻，重标只多一次收敛
    // 盘点（bound 未变的行免换槽）。
    if *prev_floor != settings.floor_anchor && !ledger.is_empty() {
        let count = mark_all_dirty(&mut ledger, true);
        info!("保底锚切换为 {}：全行 {count} 行置脏 + LOD 脏（锚腿/需求位图换形）", settings.floor_anchor);
    }
    *prev_floor = settings.floor_anchor;
    let recycle = pool.recycle_ledger();
    let used = recycle.resident_vertex_bytes + recycle.resident_index_bytes;
    stats.used_mesh_bytes = used;
    let used_image = image_cache.resident_bytes();
    stats.used_image_bytes = used_image;
    stats.resident_images = image_cache.resident_count();
    stats.resident_meshes = pool.resident_count();
    stats.cooling = cooldown.meshes.len() + cooldown.images.len();
    let active = settings.enabled
        && (settings.budget_mesh_bytes > 0 || settings.budget_image_bytes > 0)
        && !ledger.is_empty();
    if !active {
        if *prev_active && !ledger.is_empty() {
            // 激活→关闭的下降沿：恢复全常驻基线（容器内资产全部重驻留）
            let count = mark_all_dirty(&mut ledger, false);
            info!("驻留管理器关闭：全行置脏 {count} 行（恢复全常驻基线，重传链逐帧消化）");
        }
        *prev_active = active;
        stats.requested = 0;
        stats.reset_tick_counters();
        stats.floor_groups_resident = 0;
        stats.over_budget = false;
        stats.over_budget_image = false;
        // B 面：需求位图清空 = flush 盘点回到不过滤的全常驻语义
        lod_demand.active = false;
        lod_demand.levels.clear();
        return; // B 面（全常驻基线）或空场：沿处理外零动作，读数照刷
    }
    let was_active = *prev_active;
    *prev_active = true;
    // —— 全静早退闸门（"输出不变才早退"）：tick 输出的全部决定输入（相机键）
    // 与上 tick 全同 ∧ 账本无脏 ∧ 缺口快照无新增 ∧ 冷却表空 → 全量段整段跳过，
    // 输出与全量路径逐字段相同（lod_demand 沿用上 tick 位图——输入全同则位图
    // 逐位相同）。冷却表非空禁止早退（见模块文档坑点）；首个激活 tick
    //（was_active=false 或相机键无历史）不走闸门——需求位图必须先发布一次。
    let cam_key = cameras.single().ok().map(|(projection, global)| TickGateKey {
        view_proj: projection.get_clip_from_view() * global.affine().inverse(),
        extent: [swapchain.extent.width, swapchain.extent.height],
        threshold_bits: lod_settings.threshold_px.to_bits(),
        hysteresis_bits: lod_settings.hysteresis.to_bits(),
        budget_mesh_bytes: settings.budget_mesh_bytes,
        budget_image_bytes: settings.budget_image_bytes,
        request_radius_bits: settings.request_radius.to_bits(),
        fine_grained: settings.fine_grained,
        floor_anchor: settings.floor_anchor,
        gap_mode_off: matches!(settings.gap_mode, GapMode::Off),
    });
    let gap_quiet =
        settings.gap_mode == GapMode::Off || snapshot.bits.iter().all(|&b| b == 0);
    if was_active
        && prev_cam_key.is_some()
        && *prev_cam_key == cam_key
        && ledger.dirty_count() == 0
        && gap_quiet
        && cooldown.meshes.is_empty()
        && cooldown.images.is_empty()
    {
        stats.requested = 0;
        stats.reset_tick_counters();
        return; // 全静：O(rows) 走访整段早退（读数/位图沿用上 tick）
    }
    *prev_cam_key = cam_key;
    stats.reset_tick_counters();
    stats.floor_groups_resident = 0;
    // 激活帧：过滤激活态随 floor_anchor 走，位图逐组重算（LOD 档位段）
    lod_demand.active = settings.floor_anchor;
    lod_demand.levels.clear();
    let Ok((projection, cam_global)) = cameras.single() else {
        return; // 相机缺席：场景装配问题，整帧让路不动作
    };
    let cam_pos = cam_global.translation();
    let view_proj = projection.get_clip_from_view() * cam_global.affine().inverse();
    let groups = aggregate_groups(&ledger, pool, cam_pos, &frustum_planes(&view_proj));
    cooldown.tick();
    let screen = [swapchain.extent.width as f32, swapchain.extent.height as f32];
    let mut pass = FullPass {
        settings: &settings,
        pool,
        lod_groups: &lod_groups,
        stats: &mut stats,
        cooldown: &mut cooldown,
        ledger: &mut ledger,
        retirements: &mut retirements,
    };
    let gap_by_base = pass.collect_gaps(snapshot);
    let gap_rows = pass.gap_request_rows(&groups, &gap_by_base);
    let mut request_rows = pass.radius_request_rows(&groups);
    pass.eject_meshes_over_budget(&groups, used);
    let lod_view = LodView { view_proj: &view_proj, screen, threshold_px: lod_settings.threshold_px };
    pass.lod_pass(&groups, &gap_by_base, &lod_view, &mut lod_demand, &mut request_rows);
    let requested_rows = pass.commit_requests(request_rows, &gap_rows);
    pass.image_pass(image_cache, &std_materials, cam_pos, &requested_rows, used_image);
}

/// 全行置脏（`with_lod` 时一并重挂 LOD 脏标），返回置脏行数。
fn mark_all_dirty(ledger: &mut InstanceLedger, with_lod: bool) -> usize {
    let entities: Vec<Entity> = ledger.rows().map(|(e, _)| e).collect();
    for e in &entities {
        ledger.mark_dirty(*e);
        if with_lod {
            ledger.mark_lod_dirty(*e);
        }
    }
    entities.len()
}

/// 身份聚合的一组：最近引用行距离、任一行视锥内、引用行清单、驻留态
///（pool.resident 同源）。
struct Group {
    min_dist: f32,
    in_frustum: bool,
    rows: Vec<Entity>,
    resident: bool,
}

/// mesh 身份 → 组。
type Groups = HashMap<AssetId<Mesh>, Group>;

/// 身份聚合（账本只读走访）：视锥判定只对已驻留身份做（未驻留无包围盒）。
fn aggregate_groups(
    ledger: &InstanceLedger,
    pool: &MeshPool,
    cam_pos: Vec3,
    planes: &[Vec4; 6],
) -> Groups {
    let mut groups: Groups = HashMap::new();
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
            g.in_frustum = aabb_visible(planes, wmin.into(), wmax.into());
        }
    }
    groups
}

/// LOD 选档的相机输入（期望档 CPU 参照，与 GPU 选档同 threshold 同 screen）。
struct LodView<'a> {
    view_proj: &'a Mat4,
    screen: [f32; 2],
    threshold_px: f32,
}

/// 全量 tick 的段间共享账：闸门之后各段共用的设置/读源与写入目标。
struct FullPass<'a> {
    settings: &'a ResidencySettings,
    pool: &'a MeshPool,
    lod_groups: &'a LodGroups,
    stats: &'a mut ResidencyStats,
    cooldown: &'a mut EjectCooldown,
    ledger: &'a mut InstanceLedger,
    retirements: &'a mut AssetRetirements,
}

impl FullPass<'_> {
    /// GPU 缺口合并：快照（帧循环在 fence 纪律内采样，本帧槽 = 两帧前 compute
    /// 输出）逐槽位图经账本反查行，聚合到基准身份（同身份多实例去重 = 槽级
    /// 位图 + 组级归并）。快照两帧延迟内的行清账/槽回收 = 陈旧缺口如实计数丢弃。
    fn collect_gaps(&mut self, snapshot: &GapSnapshot) -> HashMap<AssetId<Mesh>, u32> {
        let mut gap_by_base: HashMap<AssetId<Mesh>, u32> = HashMap::new();
        if self.settings.gap_mode == GapMode::Off || snapshot.bits.is_empty() {
            return gap_by_base;
        }
        let mut slot_bases: HashMap<u32, AssetId<Mesh>> = HashMap::new();
        for (_entity, row) in self.ledger.rows() {
            if let Some(slot) = row.instance_slot {
                slot_bases.insert(slot, row.mesh.id());
            }
        }
        for (slot, bits) in snapshot.bits.iter().enumerate() {
            if *bits == 0 {
                continue;
            }
            self.stats.gap_slots_last += 1;
            let Some(&base) = slot_bases.get(&(slot as u32)) else {
                self.stats.gap_stale_total += 1;
                continue;
            };
            *gap_by_base.entry(base).or_insert(0u32) |= bits;
        }
        gap_by_base
    }

    /// 缺口请求裁决（CPU 拍板）：冷却窗仍适用（防抖）；请求半径不适用；逐档
    /// 复核驻留（两帧延迟内可能已到货），仍缺的组引用行进请求行集。
    fn gap_request_rows(
        &mut self,
        groups: &Groups,
        gap_by_base: &HashMap<AssetId<Mesh>, u32>,
    ) -> Vec<Entity> {
        let mut gap_rows: Vec<Entity> = Vec::new();
        for (base_id, bits) in gap_by_base {
            let Some(g) = groups.get(base_id) else { continue };
            let Some(levels) = self.lod_groups.levels_of(*base_id) else { continue };
            if self.cooldown.meshes.contains_key(base_id) {
                continue;
            }
            let any_missing = gap_asset_ids(*bits, levels, *base_id)
                .iter()
                .any(|id| self.pool.resident(*id).is_none());
            if any_missing {
                self.stats.gap_assets_last += 1;
                gap_rows.extend(g.rows.iter().copied());
            }
        }
        gap_rows
    }

    /// 半径请求：非驻留 ∧ 半径内 ∧ 冷却已过 → 全部引用行进请求行集（与 LOD
    /// 档请求、缺口请求合流后统一置脏）。
    fn radius_request_rows(&mut self, groups: &Groups) -> Vec<Entity> {
        let mut request_rows: Vec<Entity> = Vec::new();
        let mut requested_ids = 0usize;
        for (id, g) in groups {
            if g.resident || g.min_dist > self.settings.request_radius {
                continue;
            }
            if self.cooldown.meshes.contains_key(id) {
                continue; // 冷却期：刚淘汰的身份不重请求（防抖窗）
            }
            // floor_anchor：组资产的供给归需求位图（基准 = 按需高档，按期望档进
            // 需求集；按半径整档请求会把最贵基准档无条件拉回——上传按需的反面）
            if self.settings.floor_anchor && self.lod_groups.levels_of(*id).is_some() {
                continue;
            }
            requested_ids += 1;
            request_rows.extend(g.rows.iter().copied());
        }
        self.stats.requested = requested_ids;
        self.stats.requested_rows_last = request_rows.len();
        request_rows
    }

    /// mesh 预算淘汰：used > 预算 → 视锥外优先、距离远优先逐个出账，直到
    /// ≤ 预算（或可淘汰者耗尽——over_budget 如实报出，不硬凑）。预算 0 = 不设限。
    fn eject_meshes_over_budget(&mut self, groups: &Groups, mut used: u64) {
        let budget = self.settings.budget_mesh_bytes;
        self.stats.over_budget = false;
        if budget > 0 && used > budget {
            let mut candidates: Vec<(AssetId<Mesh>, f32, u64)> = groups
                .iter()
                .filter(|(_, g)| g.resident)
                .filter_map(|(id, g)| {
                    // resident 标记与此处同一 pool 查询，恒命中
                    let slot = self.pool.resident(*id)?;
                    Some((*id, g.min_dist, slot.vertex.size + slot.index.size))
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
                if used <= budget {
                    break;
                }
                // 出账进卸载回收账：flush 下帧开头出驻留条目 → 停尸房 → 排空后
                // 区间归还 free list；引用行此后 DrawList 暂缓（mesh 未驻留 = 不进
                // 在场环——先撤引用后回收的幽灵防线）
                self.retirements.eject_mesh(id);
                self.cooldown.meshes.insert(id, self.settings.eject_cooldown_frames);
                used = used.saturating_sub(bytes);
                self.stats.ejected_total += 1;
                self.stats.ejected_last += 1;
            }
            self.stats.over_budget = used > budget;
        }
        self.stats.used_mesh_bytes = used;
    }

    /// LOD 档位段：逐组算需求集（锚 ∪ 期望档 ∪ 缺口档）→ 位图发布 → 需求档
    /// 缺席进请求行集 → 非需求档淘汰。档位驻留计数两面统一（细粒度关 = 档位
    /// 全驻留的读数也要如实）。
    fn lod_pass(
        &mut self,
        groups: &Groups,
        gap_by_base: &HashMap<AssetId<Mesh>, u32>,
        view: &LodView<'_>,
        lod_demand: &mut LodDemand,
        request_rows: &mut Vec<Entity>,
    ) {
        let (settings, pool, lod_groups) = (self.settings, self.pool, self.lod_groups);
        let fine = settings.fine_grained || settings.floor_anchor;
        let mut lod_levels_resident = 0usize;
        for (base_id, g) in groups {
            let Some(group_levels) = lod_groups.levels_of(*base_id) else { continue }; // 非阶梯 mesh 无档位账
            // 锚档恒需求：floor_anchor = 保底档常驻、基准降为按需高档；组尾档
            // 缺席（登记不完整）锚退回基准
            let mut needed = [false; LEVELS];
            needed[anchor_level(settings.floor_anchor, group_levels[FLOOR_LEVEL].is_some())
                as usize] = true;
            // GpuOnly：CPU 期望档预测请求让位——期望档不进需求集
            if settings.gap_mode != GapMode::GpuOnly {
                self.mark_expected_levels(&mut needed, g, group_levels, view);
            }
            // 缺口归并：GPU 选档缺口并入需求集——位图发布与淘汰豁免都以 merged
            // 为准（CPU 拍板 = 接受缺口进需求）；merged[锚] 恒真（保底豁免保持）
            let gap_mask = gap_by_base.get(base_id).copied().unwrap_or(0u32);
            let mut merged = needed;
            for (level, merged_l) in merged.iter_mut().enumerate() {
                *merged_l |= gap_mask >> level & 1 == 1;
            }
            // 需求位图发布：缺口点名的档位由此获得上传通道
            if settings.floor_anchor {
                let mut mask = 0u32;
                for (level, needed_l) in merged.iter().enumerate() {
                    if *needed_l {
                        mask |= 1 << level;
                    }
                }
                lod_demand.levels.insert(*base_id, mask);
            }
            // 保底驻留计数（与需求无关，只报账面事实）
            if let Some(handle) = &group_levels[FLOOR_LEVEL]
                && pool.resident(handle.id()).is_some()
            {
                self.stats.floor_groups_resident += 1;
            }
            // 请求合并：需求档未驻留 → 一次行置脏（盘点收集全部未驻留档）。全档
            // 检查含 level 0（基准可被淘汰，见模块文档坑点）；驻留计数只计变体档
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
                && !self.cooldown.meshes.contains_key(base_id)
            {
                request_rows.extend(g.rows.iter().copied());
            }
            if fine {
                self.eject_unneeded_levels(&merged, g, group_levels);
            }
        }
        self.stats.lod_levels_resident = lod_levels_resident;
    }

    /// 期望档位并集：组内每个引用行按投屏尺寸选档（needed 为按位或，与走访序
    /// 无关）。期望档优先取基准槽位包围盒；基准缺席退锚槽位——保底变体与基准
    /// 同源同界近似，跳过会让基准一旦淘汰就永远算不出期望档（高档永不回流）。
    fn mark_expected_levels(
        &self,
        needed: &mut [bool; LEVELS],
        g: &Group,
        group_levels: &[Option<Handle<Mesh>>; LEVELS],
        view: &LodView<'_>,
    ) {
        let pool = self.pool;
        for row in g.rows.iter().filter_map(|entity| self.ledger.row(*entity)) {
            let Some(slot) = pool.resident(row.mesh.id()).or_else(|| {
                group_levels[FLOOR_LEVEL].as_ref().and_then(|h| pool.resident(h.id()))
            }) else { continue };
            if let Some((px_min, px_max, _)) = aabb_screen_rect(
                view.view_proj, &row.model, slot.local_aabb.0, slot.local_aabb.1, view.screen,
            ) {
                let size = (px_max.x - px_min.x).max(px_max.y - px_min.y);
                let raw = lod_raw_level(size, view.threshold_px, MAX_LOD_LEVELS);
                needed[raw as usize] = true;
            }
        }
    }

    /// 非需求档淘汰（冷却窗同源）：以 merged 为"需求"口径——缺口点名的档位
    /// 不进淘汰候选。floor_anchor 下从 level 0 起步（基准降为可淘汰高档；保底
    /// 恒需求恒不淘汰）。淘汰同步重挂引用行 LOD 脏标（b7 表条目清账：变体区间
    /// 将进 free list，残留引脚 = 画错几何）。
    fn eject_unneeded_levels(
        &mut self,
        merged: &[bool; LEVELS],
        g: &Group,
        group_levels: &[Option<Handle<Mesh>>; LEVELS],
    ) {
        let eject_start = usize::from(!self.settings.floor_anchor);
        for (level, needed_l) in merged.iter().enumerate().skip(eject_start) {
            if *needed_l { continue; }
            let Some(handle) = &group_levels[level] else { continue };
            let level_id = handle.id();
            if self.pool.resident(level_id).is_none() { continue; }
            if self.cooldown.meshes.contains_key(&level_id) { continue; }
            self.retirements.eject_mesh(level_id);
            self.cooldown.meshes.insert(level_id, self.settings.eject_cooldown_frames);
            self.stats.lod_levels_ejected_total += 1;
            for entity in &g.rows {
                self.ledger.mark_lod_dirty(*entity);
            }
        }
    }

    /// 请求行集落账：缺口请求行并入（缺口段已做冷却/驻留复核/组级去重），与
    /// CPU 预测请求同一条置脏通道。返回本 tick 已置脏的行集（贴图段排除用）。
    fn commit_requests(
        &mut self,
        mut request_rows: Vec<Entity>,
        gap_rows: &[Entity],
    ) -> HashSet<Entity> {
        request_rows.extend(gap_rows.iter().copied());
        self.stats.gap_rows_last = gap_rows.len();
        self.stats.gap_rows_total += gap_rows.len() as u64;
        for entity in &request_rows {
            self.ledger.mark_dirty(*entity);
        }
        self.stats.requested_rows_total += request_rows.len() as u64;
        request_rows.into_iter().collect()
    }

    /// 贴图预算段：超支按距离远优先淘汰整资产（eject_image 同一回收账）；
    /// 预算内则半径内缺席贴图请求（引用行置脏，材质五槽盘点重驻留）。
    fn image_pass(
        &mut self,
        image_cache: &ImageCache,
        std_materials: &Assets<StandardMaterial>,
        cam_pos: Vec3,
        requested_rows: &HashSet<Entity>,
        mut used_image: u64,
    ) {
        let settings = self.settings;
        let budget = settings.budget_image_bytes;
        let image_groups = image_groups(self.ledger, std_materials, image_cache, cam_pos);
        let mut over_budget_image = false;
        let mut image_request: Vec<Entity> = Vec::new();
        if budget > 0 && used_image > budget {
            let mut candidates: Vec<(AssetId<Image>, f32, u64)> = image_groups
                .iter()
                .filter_map(|(id, (dist, _))| {
                    let bytes = image_cache.image(*id).map(|g| g.alloc_bytes())?;
                    Some((*id, *dist, bytes))
                })
                .collect();
            candidates.sort_by(|a, b| b.1.total_cmp(&a.1)); // 距离远优先
            for (id, _dist, bytes) in candidates {
                if used_image <= budget {
                    break;
                }
                if self.cooldown.images.contains_key(&id) { continue; }
                // 本 tick 已置脏行的贴图跳过：材质腿同帧重传会让 eject 空转
                //（拉锯），排除后下一 tick 行稳定再淘汰
                if image_groups[&id].1.iter().any(|r| requested_rows.contains(r)) {
                    continue;
                }
                self.retirements.eject_image(id);
                self.cooldown.images.insert(id, settings.eject_cooldown_frames);
                used_image = used_image.saturating_sub(bytes);
                // 重驻留通道 = 预算内分支（冷却过期 ∧ 半径内 → 引用行置脏重传）
                //——eject 当帧不排队（同帧重传 = 自我拉锯，used 恒定全量）
            }
            over_budget_image = used_image > budget;
        } else if budget > 0 {
            // 预算内：半径内缺席贴图照常请求（与 mesh 请求同拍）
            for (id, (dist, rows)) in &image_groups {
                if *dist <= settings.request_radius
                    && image_cache.resident(*id).is_none()
                    && !self.cooldown.images.contains_key(id)
                {
                    image_request.extend(rows.iter().copied());
                }
            }
        }
        for entity in &image_request {
            self.ledger.mark_dirty(*entity);
        }
        self.stats.over_budget_image = over_budget_image;
        self.stats.used_image_bytes = used_image;
    }
}

/// 贴图聚合：材质 → (最近引用行距离, 引用行) → 贴图 → (最近距离, 引用行)。
/// 未驻留贴图不参与（请求由行腿覆盖）。
fn image_groups(
    ledger: &InstanceLedger,
    std_materials: &Assets<StandardMaterial>,
    image_cache: &ImageCache,
    cam_pos: Vec3,
) -> HashMap<AssetId<Image>, (f32, Vec<Entity>)> {
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
            if image_cache.resident(handle.id()).is_none() {
                continue;
            }
            let e = image_groups.entry(handle.id()).or_insert((f32::INFINITY, Vec::new()));
            e.0 = e.0.min(*dist);
            e.1.extend(rows.iter().copied());
        }
    }
    image_groups
}

/// 驻留 tick 的系统集（Last）：上传编排排在它之后，动作进同一帧上传账。
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResidencySet;

/// 驻留管理器插件：设置/读数/冷却三资源 + tick 挂 Last 的 [`ResidencySet`]
///（上传编排在其后——动作进同一帧上传账）。默认关（全常驻语义不变），
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
                    .in_set(ResidencySet)
                    .run_if(resource_exists::<MeshPool>)
                    .run_if(resource_exists::<crate::render::BindlessTables>)
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

    /// 可绘锚档位（三处共用纯函数）：关闭 = 基准；开启 = 保底档（最粗变体）；
    /// 组尾档缺席（登记不完整）退回基准。
    #[test]
    fn anchor_level_resolution() {
        assert_eq!(anchor_level(false, true), 0, "锚关闭 = 基准档");
        assert_eq!(anchor_level(false, false), 0);
        assert_eq!(
            anchor_level(true, true),
            MAX_LOD_LEVELS - 1,
            "锚开启 = 保底档（最粗变体）"
        );
        assert_eq!(anchor_level(true, false), 0, "尾档缺席退回基准（登记不完整）");
    }

    /// 缺口位图 → 请求身份集（去重前）：逐置位档位映射资产身份（level 0 =
    /// 基准），零位图空集，越界位忽略（环内容异常不放大）。
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

    /// GapMode 字符串往返与默认（BRP 通道的解析纪律：未知形态报错不猜）。
    #[test]
    fn gap_mode_parse_roundtrip() {
        assert_eq!(GapMode::default(), GapMode::Off);
        for mode in [GapMode::Off, GapMode::GpuOnly, GapMode::Merged] {
            assert_eq!(GapMode::parse(mode.as_str()), Ok(mode));
        }
        assert!(GapMode::parse("wat").is_err());
    }
}
