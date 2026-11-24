//! 表面缓存帧侧编排（9.5）：Volume 跟随（各级整数体素偏移）→ 写 GI UBO/清计数 →
//! 组装 ④.5 段录制束（回收三件 + 播种 + ⑧ 段视图）。GI 关/无相机整段返回 None，
//! 帧路径与 9.3 逐位一致。
//!
//! 状态都在 [`GiFrameLocal`]：上一帧 view_proj（9.6 重投影用）、上一帧各级偏移（滚动
//! 只在偏移变化的帧录）、连续帧号（心跳）与"上一帧是否开着"（重开首帧强制滚动一次）。

use ash::vk;
use bevy::ecs::error::BevyError;
use bevy::ecs::system::SystemParam;
use bevy::math::{IVec3, Mat4};
use bevy::prelude::*;

use crate::common::syntax::ResultTierExt;
use crate::render::{
    alloc_grid, estimation_groups, eviction_groups, gi_cascade_offsets, pack_alloc_push,
    pack_view_push, scrolling_groups, DescSet, GiAllocDraw, GiEstimationDraw, GiFrameDraw,
    GiPipelines, GiSettings, GiViewDraw, SurfaceCacheTables, SurfaceCacheUniformsData, EVICT_AGE,
    MAX_CASCADES, UPDATE_MAX,
};

use super::scene::FrameView;

/// draw_frame 的表面缓存取用束。
#[derive(SystemParam)]
pub(super) struct GiInput<'w> {
    pub(super) settings: Res<'w, GiSettings>,
    pub(super) tables: ResMut<'w, SurfaceCacheTables>,
    pub(super) pipelines: Res<'w, GiPipelines>,
}

/// draw_frame 的 GI 段跨帧状态。
pub(super) struct GiFrameLocal {
    /// 上一帧 view_proj（9.6 的种子重投影；本段只写）。
    prev_view_proj: Mat4,
    /// 上一帧各级偏移（滚动触发判定）。
    prev_offsets: [IVec3; MAX_CASCADES],
    /// 帧号（心跳/淘汰距离；只在开 GI 的帧递增）。
    frame_index: u32,
    /// 上一帧是否开着（重开首帧强制滚动一次）。
    was_enabled: bool,
}

impl Default for GiFrameLocal {
    fn default() -> Self {
        Self {
            prev_view_proj: Mat4::IDENTITY,
            prev_offsets: [IVec3::ZERO; MAX_CASCADES],
            frame_index: 0,
            was_enabled: false,
        }
    }
}

/// 组装本帧 GI 录制束；返回 None = GI 关/无相机（整段跳过）。
///
/// 顺序即依赖：偏移 → 写 UBO（须在 wait_for_slot 之后）→ 清本槽计数 → 组装束。
#[expect(
    clippy::too_many_arguments,
    reason = "帧侧编排函数的输入就是这些资源的借用，收结构体只会多一层解构"
)]
pub(super) fn prepare_gi(
    gi: &mut GiInput<'_>,
    local: &mut GiFrameLocal,
    slot: usize,
    extent: vk::Extent2D,
    view: &FrameView<'_>,
    frame_set: DescSet,
    gbuffer_set: DescSet,
    ibl_set: DescSet,
    ldr: (vk::ImageView, vk::Image, vk::Extent2D),
    instance_set: DescSet,
    shadow_set: DescSet,
    texture_set: DescSet,
    ray_set: Option<DescSet>,
    instance_count: u32,
) -> Result<Option<GiFrameDraw>, BevyError> {
    let settings = &*gi.settings;
    if !settings.enabled {
        local.was_enabled = false;
        return Ok(None);
    }
    // 无相机 = 没有播种源也没有焦点：不录（空场景判定线）
    let Some(camera) = view.camera else {
        local.was_enabled = false;
        return Ok(None);
    };
    let config = gi.tables.config();
    let center = camera.pos;
    let offsets = gi_cascade_offsets(center, &config);
    let cascades = config.cascades as usize;
    let offsets_changed =
        !local.was_enabled || offsets[..cascades] != local.prev_offsets[..cascades];
    let frame_index = local.frame_index;

    let data = SurfaceCacheUniformsData {
        prev_view_proj: local.prev_view_proj,
        target_pos: center,
        voxel_min: config.voxel_min,
        res: config.res,
        cascades: config.cascades,
        capacity: config.capacity,
        cascade_offsets: offsets,
        defrag_waves: settings.defrag_waves.min(16),
        normal_source: settings.normal_source,
        seed_enabled: u32::from(settings.seed),
        frame_index,
        evict_age: EVICT_AGE,
        // bounce 分配开关（frame.z）：只有估计真的在跑时才算"多跳"，否则 9.5 的淘汰
        // 分支（rank 0→1、本轮不淘汰）会因为 multi_bounce 默认开而被误触发。
        bounce_enabled: u32::from(settings.estimation && settings.multi_bounce),
        update_max: UPDATE_MAX,
        samples: settings.samples.max(1),
        warmup: settings.warmup.max(1),
        multi_bounce: u32::from(settings.estimation && settings.multi_bounce),
        light_samples: settings.light_samples,
        short_hysteresis: settings.short_hysteresis,
        emissive_intensity: settings.emissive_intensity,
    };
    gi.tables
        .write_uniforms(slot, &data)
        .or_fatal("GI UBO 写入失败，渲染链无法继续")?;
    gi.tables
        .clear_counters(slot)
        .or_fatal("GI 计数缓冲清零失败，渲染链无法继续")?;

    local.prev_view_proj = view.view_proj;
    local.prev_offsets = offsets;
    local.frame_index = frame_index.wrapping_add(1);
    local.was_enabled = true;

    let pipelines = &*gi.pipelines;
    let gi_set = gi.tables.set(slot);
    let waves = settings.defrag_waves.min(16);
    let alloc = settings.seed.then(|| {
        let (low, groups) = alloc_grid(extent);
        GiAllocDraw {
            pass: pipelines.alloc(),
            push: pack_alloc_push(low, extent, frame_index, settings.normal_source),
            groups,
            frame_set,
            gbuffer_set,
            ibl_set,
        }
    });
    let cover = settings.cover_enabled().then(|| pipelines.cover());
    // 9.6 estimation：估计开关 + 有实例（0 实例时 TLAS 从未构建，绝不能打射线）+
    // 光线求交开着（关闭时求交集不存在，管线也不建）。
    let estimation = if settings.estimation && instance_count > 0 {
        match (ray_set, pipelines.estimation()) {
            (Some(ray_set), Some(pass)) => Some(GiEstimationDraw {
                pass,
                groups: estimation_groups(config.capacity),
                frame_set,
                gi_set,
                instance_set,
                ray_set,
                shadow_set,
                texture_set,
            }),
            _ => None,
        }
    } else {
        None
    };
    let marker = settings.markers_enabled().then(|| pipelines.marker());
    let view_draw = (cover.is_some() || marker.is_some()).then(|| GiViewDraw {
        cover,
        marker,
        push: pack_view_push(settings.view, settings.normal_source, extent),
        frame_set,
        gbuffer_set,
        gi_set,
        capacity: config.capacity,
        ldr_view: ldr.0,
        ldr_image: ldr.1,
        extent: ldr.2,
    });
    Ok(Some(GiFrameDraw {
        scrolling: offsets_changed.then(|| pipelines.scrolling()),
        scrolling_groups: scrolling_groups(config.cascades, config.res),
        eviction: pipelines.eviction(),
        eviction_groups: eviction_groups(config.capacity),
        defrag: pipelines.defrag(),
        defrag_waves: waves,
        alloc,
        estimation,
        gi_set,
        view: view_draw,
    }))
}
