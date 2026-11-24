//! 求交器帧侧编排（9.4）：TLAS 容量保证 → 光线求交描述符集换绑 → 统计清零 → BLAS 地址表
//! 增量/整表写入 → 22 段录制束（{@link RayTracingFrameDraw}）。光线求交关/设备不支持时整段返回 None，
//! 帧路径与 9.3 逐位一致。

use ash::vk;
use bevy::ecs::error::BevyError;
use bevy::prelude::*;

use crate::common::syntax::ResultTierExt;
use crate::render::{
    ray_debug_draw, tlas_build_draw, Context, FramePool, InstanceTables, MeshPool,
    RayTracingFrameDraw, MAX_FRAMES_IN_FLIGHT,
};
use crate::scene::{InstanceLedger, LodGroups};

use super::input::RayTracingInput;

/// draw_frame 的 光线求交段跨帧状态。
#[derive(Default)]
pub(super) struct RayTracingFrameLocal {
    /// 上次整表重写地址表时的 BLAS 生成号（建成/销毁递增）。
    last_blas_generation: u64,
}

/// 组装本帧 光线求交录制束；返回 None = 光线求交关/设备不支持/资源未就绪（整段跳过）。
///
/// 顺序即依赖：TLAS 容量保证（可能要等全部在飞帧）→ 句柄齐备后写 光线求交集 → 地址表 →
/// 统计清零 → 产物束。地址表在 光线求交集未就绪时也写（宿主侧写入，无害），保证句柄一到就新鲜。
#[expect(
    clippy::too_many_arguments,
    reason = "帧侧编排函数的输入就是这些资源的借用，收结构体只会多一层解构"
)]
pub(super) fn prepare_ray_tracing<'a>(
    ray_tracing: &'a mut RayTracingInput<'_>,
    local: &mut RayTracingFrameLocal,
    ctx: &Context,
    frames: &mut FramePool,
    instance_tables: &mut InstanceTables,
    ledger: &InstanceLedger,
    pool: &MeshPool,
    lod_groups: &LodGroups,
    dirty_models: &[Entity],
    slot: usize,
    instance_count: u32,
    extent: vk::Extent2D,
) -> Result<Option<RayTracingFrameDraw<'a>>, BevyError> {
    let Some(settings) = ray_tracing.settings.as_ref().map(|s| **s) else {
        return Ok(None);
    };
    if !settings.enabled {
        return Ok(None);
    }
    let Some(pipelines) = ray_tracing.pipelines.as_ref() else {
        return Ok(None);
    };
    let Some(ray) = ray_tracing.ray.as_deref_mut() else {
        return Ok(None);
    };
    let Some(targets) = ray_tracing.targets.as_deref_mut() else {
        return Ok(None);
    };
    // 容量不足时这里会等全部在飞帧（低频：只在在场高水位增长时）
    ray.ensure_tlas_capacity(ctx, instance_count, &mut || frames.wait_all_inflight())
        .or_fatal("TLAS 容量保证失败，渲染链无法继续")?;
    let handles: [vk::AccelerationStructureKHR; MAX_FRAMES_IN_FLIGHT] =
        [ray.tlas_handle(0), ray.tlas_handle(1)];
    let ready = targets
        .ensure_bindings(handles, pool)
        .or_fatal("光线求交描述符集写入失败，渲染链无法继续")?;
    // 地址表：BLAS 集合变过就整表重写（锚档会随 LOD 变体切换），否则只补本帧模型脏行
    let generation = ray.blas_generation();
    if generation != local.last_blas_generation {
        local.last_blas_generation = generation;
        instance_tables.write_blas_addresses_all(ledger, pool, lod_groups, ray)?;
    }
    instance_tables.write_blas_address_rows(ledger, pool, lod_groups, ray, dirty_models, slot)?;
    if !ready {
        return Ok(None);
    }
    targets
        .clear_stats(slot)
        .or_fatal("光线求交统计缓冲清零失败，渲染链无法继续")?;
    targets.note_instances(instance_count);
    let Some(instance_buffer) = instance_tables.tlas_instance_buffer(slot) else {
        return Ok(None);
    };
    let instance_address = ctx.buffer_address(instance_buffer);
    // BLAS 构建输入用本帧池基址（扩容迁移后地址会变，排建时的旧地址不可用）
    let (Some(vertex_base), Some(index_base)) = (pool.vertex_address(), pool.index_address())
    else {
        return Ok(None);
    };
    let tlas_build = tlas_build_draw(pipelines.tlas_build(), instance_count, instance_address);
    // 空场景（0 实例）不录 TLAS 构建，也绝不能打射线：TLAS 从未构建时 ray query 读它是
    // 未定义行为（空场的"没东西可撞"由宿主侧判定，不靠 0 长度命令的边角合法性）
    let debug = (instance_count > 0).then(|| {
        ray_debug_draw(
            pipelines.ray_debug(),
            targets.set(slot),
            targets.debug_image(slot).image(),
            extent,
            settings.view,
        )
    });
    Ok(Some(RayTracingFrameDraw::new(
        ray,
        (vertex_base, index_base),
        tlas_build,
        debug,
    )))
}
