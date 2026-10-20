//! 驻留管理器（6.3.1~6.3.4）：显存预算账 + 相机驱动的资产请求/淘汰——
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

use std::collections::HashMap;

use bevy::{
    camera::{Camera, Projection},
    pbr::StandardMaterial,
    prelude::*,
};
use ash_macros::system;

use crate::{
    driver::upload::flush_uploads,
    scene::InstanceLedger,
    vulkan::{aabb_visible, conservative_world_aabb, frustum_planes, ImageCache, MeshPool},
};

use super::asset_events::AssetRetirements;

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
}

impl Default for ResidencySettings {
    fn default() -> Self {
        Self {
            enabled: false, // 默认关：不改变既有全常驻语义（BRP/场景脚本显式开启）
            budget_mesh_bytes: 0,
            request_radius: 40.0,
            eject_cooldown_frames: 60,
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
}

/// 管理器内部状态（冷却表：身份 → 剩余冷却帧）。
#[derive(Resource, Default)]
pub(crate) struct EjectCooldown(HashMap<AssetId<Mesh>, u32>);

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
    pool: Res<MeshPool>,
    image_cache: Res<ImageCache>,
    mut std_materials: ResMut<Assets<StandardMaterial>>,
    cameras: Query<(&Projection, &GlobalTransform), With<Camera>>,
    // 激活态下降沿检测（Local）：active→inactive 时全行置脏——被淘汰资产
    // 无请求者会永远缺席，关闭 B 面（全常驻基线）必须恢复全量（④ 判定线）
    mut prev_active: Local<bool>,
) {
    let _ = &mut std_materials; // 预留：材质腿计量（6.4 贴图账一并）；本 tick 不触
    stats.enabled = settings.enabled;
    stats.budget_mesh_bytes = settings.budget_mesh_bytes;
    stats.rows = ledger.len();
    stats.ejected_last = 0;
    stats.requested_rows_last = 0;
    let recycle = pool.recycle_ledger();
    let mut used = recycle.resident_vertex_bytes + recycle.resident_index_bytes;
    stats.used_mesh_bytes = used;
    stats.resident_images = image_cache.resident_count();
    stats.resident_meshes = pool.resident_count();
    stats.cooling = cooldown.0.len();
    let active = settings.enabled && settings.budget_mesh_bytes > 0 && !ledger.is_empty();
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
        return; // B 面（全常驻基线）或空场：沿处理外零动作，读数照刷
    }
    *prev_active = true;
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

    // —— 冷却走账：剩余帧递减，归零出表。
    cooldown.0.retain(|_, frames| {
        *frames = frames.saturating_sub(1);
        *frames > 0
    });

    // —— 请求侧：非驻留 ∧ 半径内 ∧ 冷却已过 → 全部引用行 mark_dirty（重传链
    // 复用；行已在脏集 = 幂等无成本）。
    let mut request_rows: Vec<Entity> = Vec::new();
    let mut requested_ids = 0usize;
    for (id, g) in &groups {
        if g.resident || g.min_dist > settings.request_radius {
            continue;
        }
        if cooldown.0.contains_key(id) {
            continue; // 冷却期：刚淘汰的身份不重请求（防抖窗，钉明见模块注释）
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

    // —— 淘汰侧：used > 预算 → 视锥外优先、距离远优先逐个出账，直到 ≤ 预算
    //（或可淘汰者耗尽——over_budget 如实报出，不硬凑：请求区内身份也按序
    // 淘汰时说明预算装不下需求，判定线 1 的读数面让位给诚实计量）。
    stats.over_budget = false;
    if used > settings.budget_mesh_bytes {
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
            cooldown.0.insert(id, settings.eject_cooldown_frames);
            used = used.saturating_sub(bytes);
            stats.ejected_total += 1;
            stats.ejected_last += 1;
        }
        stats.over_budget = used > settings.budget_mesh_bytes;
    }
    stats.used_mesh_bytes = used;
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
            .add_systems(
                Last,
                residency_tick
                    .before(flush_uploads)
                    .run_if(resource_exists::<MeshPool>)
                    .run_if(resource_exists::<crate::vulkan::BindlessTables>)
                    .run_if(resource_exists::<AssetRetirements>),
            );
    }
}
