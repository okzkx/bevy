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
    alloc_grid, bounce_alloc_groups, emissive_build_groups, estimation_groups, eviction_groups,
    gi_cascade_offsets, light_sampling_groups, pack_alloc_push, pack_emissive_build_push,
    pack_light_sampling_push, pack_screen_gather_push, pack_spatial_filter_push,
    pack_temporal_filter_push, pack_view_push, plan_estimation_budget, scrolling_groups,
    spatial_filter_groups, DescSet, GiAllocDraw, GiBounceAllocDraw, GiEmissiveBuildDraw,
    GiEstimationDraw, GiFrameDraw, GiLightSampleDraw, GiPipelines, GiScreenGatherDraw, GiSettings,
    GiSpatialFilterDraw, GiTemporalFilterDraw, GiViewDraw, LightRecord, LightTable, ScgiTargets,
    SurfaceCacheTables, SurfaceCacheUniformsData, EVICT_AGE, GI_BOUNCE_REQUEST_CAPACITY,
    MAX_CASCADES, UPDATE_MAX,
};

use super::scene::FrameView;

/// 9.7.2 空间滤波的抖动样本数（Unity `SpatialFiltering` 同款默认 4；9.7.7 若要 A/B 再抬成参数）。
const SPATIAL_FILTER_TAPS: u32 = 4;

/// draw_frame 的表面缓存取用束。
#[derive(SystemParam)]
pub(super) struct GiInput<'w> {
    pub(super) settings: Res<'w, GiSettings>,
    pub(super) tables: ResMut<'w, SurfaceCacheTables>,
    pub(super) pipelines: Res<'w, GiPipelines>,
    /// 9.7.4 的 1/4 屏幕目标（随 swapchain 尺寸懒重建）。
    pub(super) scgi: ResMut<'w, ScgiTargets>,
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
    lights: &LightTable,
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

    // 9.6.2：把活灯按紧凑顺序写进本帧槽的光源表（清单与 estimation 都按 0..light_count 索引）。
    // 每帧重写 16 KiB：比"变更检测 + 增量"简单，且环表的槽号是持久分配的、不能直接当紧凑下标。
    let light_records: Vec<LightRecord> = lights
        .active_slots()
        .into_iter()
        .filter_map(|slot| lights.record(slot))
        .collect();
    let light_count = light_records.len() as u32;
    gi.tables
        .write_gi_lights(slot, &light_records)
        .or_fatal("光源紧凑表写入失败，渲染链无法继续")?;
    // 清单条数：有灯才采；上限是清单容量（超过就按容量截断，不报错）
    let light_sample_count = if light_count == 0 {
        0
    } else {
        settings
            .light_samples
            .min(crate::render::GI_LIGHT_SAMPLE_CAPACITY)
    };

    // 9.6.5 预算：活跃 patch 数取 ring 水位（上一帧的计数，一次 48B 主机读，误差一帧）
    let mut ring_words = [0u8; crate::render::RING_CONFIG_WORDS * 4];
    gi.tables
        .ring()
        .read(0, &mut ring_words)
        .or_fatal("GI ring 配置读失败，渲染链无法继续")?;
    let active_patches = u32::from_le_bytes([
        ring_words[crate::render::RING_ACTIVE * 4],
        ring_words[crate::render::RING_ACTIVE * 4 + 1],
        ring_words[crate::render::RING_ACTIVE * 4 + 2],
        ring_words[crate::render::RING_ACTIVE * 4 + 3],
    ]);
    let budget_plan =
        plan_estimation_budget(active_patches, settings.samples.max(1), settings.budget);

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
        samples: budget_plan.samples,
        warmup: settings.warmup.max(1),
        multi_bounce: u32::from(settings.estimation && settings.multi_bounce),
        light_samples: settings.light_samples,
        short_hysteresis: settings.short_hysteresis,
        emissive_intensity: settings.emissive_intensity,
        light_count,
        light_sample_count,
        estimation_stride: budget_plan.stride,
        estimation_active: active_patches,
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
    // 9.6.2 自发光表：估计开且光线求交开着就每帧重建（顶点/索引池要带 storage）
    let object_capacity = crate::render::OBJECT_TABLE_CAPACITY;
    let emissive_build = if settings.estimation {
        match (ray_set, pipelines.emissive_build()) {
            (Some(ray_set), Some(pass)) => Some(GiEmissiveBuildDraw {
                pass,
                push: pack_emissive_build_push(object_capacity),
                groups: emissive_build_groups(object_capacity),
                gi_set,
                instance_set,
                ray_set,
                texture_set,
            }),
            _ => None,
        }
    } else {
        None
    };
    // 9.6.2 清单：估计开、有实例（要 TLAS）、有灯、光线求交开着才录
    let light_sampling = if settings.estimation && instance_count > 0 && light_sample_count > 0 {
        match (ray_set, pipelines.light_sampling()) {
            (Some(ray_set), Some(pass)) => Some(GiLightSampleDraw {
                pass,
                push: pack_light_sampling_push(light_sample_count, light_count),
                groups: light_sampling_groups(light_sample_count),
                gi_set,
                instance_set,
                ray_set,
                texture_set,
            }),
            _ => None,
        }
    } else {
        None
    };
    // 9.6.3 bounce 分配：只在估计 + 多跳都开着时录（关掉时请求表无人写）
    let bounce_alloc = if settings.estimation && settings.multi_bounce {
        pipelines.bounce_alloc().map(|pass| GiBounceAllocDraw {
            pass,
            groups: bounce_alloc_groups(GI_BOUNCE_REQUEST_CAPACITY),
            gi_set,
        })
    } else {
        None
    };
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
    // 9.7.2 空间滤波：estimation 录了才录（没有本帧输出可滤）。开关关闭时**仍然录**，
    // push 里 enabled=0 → shader 直拷，`filtered` 表因此永远是"本帧可消费的完整一份"，
    // 9.7.3/9.7.4 不必再判断读哪张表。9.7.6 接上 `scgi` 后整条屏幕链改由它把关。
    let spatial_filter = estimation.as_ref().map(|_| GiSpatialFilterDraw {
        pass: pipelines.spatial_filter(),
        push: pack_spatial_filter_push(
            settings.spatial_filter,
            SPATIAL_FILTER_TAPS,
            frame_index,
            config.capacity,
            settings.spatial_radius,
        ),
        groups: spatial_filter_groups(config.capacity),
        frame_set,
        gi_set,
    });
    // 9.7.3 时域后滤波：跟空间滤波同生灭（没有 A 就没有可混合的输入）。关态同样录，
    // push 里 enabled=0 → B = A 直拷。
    let temporal_filter = spatial_filter.as_ref().map(|_| GiTemporalFilterDraw {
        pass: pipelines.temporal_filter(),
        push: pack_temporal_filter_push(
            settings.temporal_filter,
            config.capacity,
            frame_index,
            settings.temporal_smoothing,
        ),
        groups: spatial_filter_groups(config.capacity),
        frame_set,
        gi_set,
    });
    // 9.7.4 1/4 屏幕查表：时域滤波之后跑（读 B）。目标图已在帧首 ensure 过，尺寸为 0 说明
    // 还没建（首帧前）——此时不录，下帧补上。
    let screen_gather = temporal_filter.as_ref().and_then(|_| {
        let low = gi.scgi.low_extent();
        if low.width == 0 || low.height == 0 {
            return None;
        }
        let (acquire, acquire_count) = gi.scgi.take_acquire();
        Some(GiScreenGatherDraw {
            pass: pipelines.screen_gather(),
            push: pack_screen_gather_push(
                low,
                extent,
                frame_index,
                settings.normal_source,
                settings.lookup_samples,
            ),
            groups: alloc_grid(extent).1,
            frame_set,
            gi_set,
            gbuffer_set,
            target_set: gi.scgi.set(),
            acquire,
            acquire_count,
        })
    });
    if screen_gather.is_some() {
        gi.scgi.mark_written();
    }
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
        emissive_build,
        light_sampling,
        estimation,
        bounce_alloc,
        spatial_filter,
        temporal_filter,
        screen_gather,
        gi_set,
        view: view_draw,
    }))
}
