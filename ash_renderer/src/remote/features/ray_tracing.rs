//! 求交器方法族（9.4）：`ray_tracing_set` 写总开关/调试图子视图，`ray_tracing_stats` 读 光线求交计量
//!（能力分位、BLAS/TLAS 账、射线计数/命中率、光线求交两段 GPU 毫秒），`ray_tracing_dump` 把
//! 调试图拷回宿主写 PPM。光线求交关/设备不支持时全部回 `enabled:false` 形态，不报错。

use std::io::Write;

use ash_macros::system;
use bevy::{prelude::*, remote::BrpResult};
use serde_json::{json, Value};

use crate::remote::common::{
    params::{internal_err, invalid_params, parse_params, res, res_mut},
    readback::{read_buffer, u32_at},
};
use crate::render::{
    half_to_f32, readback_buffer, readback_debug_image, Context, FramePool, GpuTimer,
    InstanceTables, RayScene, RayTracingSettings, RayTracingTargets, BLAS_ADDRESS_SIZE,
    TLAS_INSTANCE_SIZE,
};

/// `ash_renderer/ray_tracing_set`：求交器总开关 + 调试图子视图（0 距离 / 1 法线 / 2 槽 hash）。
/// 请求 `{"enabled": bool, "view"?: 0|1|2}`；`view` 缺省保留当前值。回写生效值。
#[derive(serde::Deserialize)]
struct RayTracingSetParams {
    enabled: bool,
    #[serde(default)]
    view: Option<u32>,
}

/// # Errors
/// 参数缺失/视图越界/RayTracingSettings 资源缺席。
#[system]
pub(crate) fn ray_tracing_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: RayTracingSetParams =
        parse_params(params, "params 缺失：需要 enabled（可选 view 0|1|2）")?;
    if let Some(view) = p.view
        && view > 2
    {
        return Err(invalid_params(format!(
            "view {view} 越界（0 = 命中距离 / 1 = 世界法线 / 2 = 实例槽 hash）"
        )));
    }
    let supported = res::<Context>(world)?.ray_tracing_support().enabled();
    let mut settings = res_mut::<RayTracingSettings>(world)?;
    let previous = settings.enabled;
    settings.enabled = p.enabled;
    if let Some(view) = p.view {
        settings.view = view;
    }
    Ok(json!({
        "enabled": settings.enabled,
        "previous": previous,
        "view": settings.view,
        "supported": supported,
    }))
}

/// `ash_renderer/ray_tracing_stats`：能力分位 + BLAS/TLAS 账 + 射线计数/命中率 + 光线求交两段毫秒。
/// 统计缓冲读"本槽"（下一帧要重录的槽，其上次提交已由帧首 fence 等完 = 两帧内新鲜）。
///
/// # Errors
/// 资源读失败时回 internal；光线求交缺席不算错误（回 enabled:false 形态）。
#[system]
pub(crate) fn ray_tracing_stats(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let caps = res::<Context>(world)?.ray_tracing_support();
    let supported = caps.enabled();
    let settings = world
        .get_resource::<RayTracingSettings>()
        .map_or_else(RayTracingSettings::default, |s| *s);
    let (blas_count, blas_pending, blas_bytes, tlas_capacity, tlas_bytes) =
        match world.get_resource::<RayScene>() {
            Some(ray) => {
                let s = ray.stats();
                (
                    s.blas_count,
                    s.blas_pending,
                    s.blas_bytes,
                    s.tlas_capacity,
                    s.tlas_bytes,
                )
            }
            None => (0, 0, 0, 0, 0),
        };
    let slot = world
        .get_resource::<FramePool>()
        .map_or(0, |f| f.current_index());
    // 统计缓冲（u32×2：射线数、命中数）与实例数
    let (rays, hits, instances) = match world.get_resource_mut::<RayTracingTargets>() {
        Some(mut targets) => {
            let bytes = read_buffer(targets.stats_buffer(slot), 8, "光线求交统计缓冲读失败")?;
            (
                u32_at(&bytes, 0)?,
                u32_at(&bytes, 4)?,
                targets.last_instances(),
            )
        }
        None => (0, 0, 0),
    };
    // 光线求交两段 GPU 毫秒（T1→T9 TLAS、T9→T10 调试图；本槽上上帧读数）
    let (ray_tracing_tlas_ms, ray_tracing_rays_ms) = match world.get_resource_mut::<GpuTimer>() {
        Some(mut timer) => timer
            .fetch_ray_tracing(slot)
            .map_err(internal_err("GPU 光线求交计时取回失败"))?
            .unwrap_or([0.0; 2])
            .into(),
        None => (0.0, 0.0),
    };
    let hit_rate = if rays > 0 {
        f64::from(hits) / f64::from(rays)
    } else {
        0.0
    };
    Ok(json!({
        "enabled": supported && settings.enabled,
        "supported": supported,
        "view": settings.view,
        "support": {
            "enabled": supported,
            "bufferDeviceAddress": caps.caps.buffer_device_address,
            "accelerationStructure": caps.caps.acceleration_structure,
            "rayQuery": caps.caps.ray_query,
            "accelerationStructureExt": caps.caps.ext_acceleration_structure,
            "rayQueryExt": caps.caps.ext_ray_query,
            "deferredHostOperations": caps.caps.ext_deferred_host_operations,
            "missing": caps.missing(),
        },
        "blas": { "count": blas_count, "pending": blas_pending, "bytes": blas_bytes },
        "tlas": { "instances": instances, "capacity": tlas_capacity, "bytes": tlas_bytes },
        "debug": {
            "rays": rays,
            "hits": hits,
            "hit_rate": hit_rate,
            "last_tlas_ms": ray_tracing_tlas_ms,
            "last_rays_ms": ray_tracing_rays_ms,
        },
    }))
}

/// `ash_renderer/ray_tracing_dump`：调试图 → PPM（P6）。请求 `{"path": "...ppm", "view"?: 0|1|2}`；
/// `view` 给了就必须等于当前 `ray_tracing_set` 的子视图（图里只有一种视图）。
#[derive(serde::Deserialize)]
struct RayTracingDumpParams {
    path: String,
    #[serde(default)]
    view: Option<u32>,
}

/// # Errors
/// 参数缺失/视图与当前设置不符/光线求交调试图缺席/读回或写文件失败。
#[system]
pub(crate) fn ray_tracing_dump(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: RayTracingDumpParams = parse_params(params, "params 缺失：需要 path（写 .ppm）")?;
    let ctx = res::<Context>(world)?;
    let settings = world
        .get_resource::<RayTracingSettings>()
        .map_or_else(RayTracingSettings::default, |s| *s);
    if let Some(view) = p.view
        && view != settings.view
    {
        return Err(invalid_params(format!(
            "调试图当前子视图是 {}，与请求的 {view} 不符——先 ray_tracing_set {{\"enabled\":true,\"view\":{view}}} 再 dump",
            settings.view
        )));
    }
    if !settings.enabled || !ctx.ray_tracing_support().enabled() {
        return Err(invalid_params(
            "光线求交当前关闭（ray_tracing_set enabled=false 或设备不支持）——没有调试图可读",
        ));
    }
    let Some(targets) = world.get_resource::<RayTracingTargets>() else {
        return Err(invalid_params("光线求交调试图资源缺席——初始化链未插入"));
    };
    let slot = world
        .get_resource::<FramePool>()
        .map_or(0, |f| f.current_index());
    let extent = targets.extent();
    let rgba =
        readback_debug_image(ctx, targets, slot).map_err(internal_err("光线求交调试图读回失败"))?;
    let (width, height) = (extent.width as usize, extent.height as usize);
    let mut ppm = Vec::with_capacity(width * height * 3 + 32);
    write!(&mut ppm, "P6\n{width} {height}\n255\n").expect("写内存缓冲不失败");
    // RGBA16F → 8bit：取 rgb 三通道，clamp 到 [0,1]
    for texel in rgba.as_chunks::<2>().0.chunks(4) {
        let mut rgb = [0u8; 3];
        for (c, px) in texel.iter().take(3).enumerate() {
            let v = half_to_f32(u16::from_le_bytes(*px)).clamp(0.0, 1.0);
            rgb[c] = (v * 255.0 + 0.5) as u8;
        }
        ppm.extend_from_slice(&rgb);
    }
    std::fs::write(&p.path, &ppm).map_err(|e| internal_err("写 PPM 失败")(e))?;
    Ok(json!({
        "path": p.path,
        "width": width,
        "height": height,
        "view": settings.view,
        "slot": slot,
        "bytes": ppm.len(),
    }))
}

/// `ash_renderer/ray_tracing_census`：TLAS 实例数组与 BLAS 地址表的字节对照（调试/施工取证）。
/// 请求 `{"slots"?: N, "instances"?: N}`（缺省各 16/8，上限 64）。只读，不驱动绘制。
#[derive(serde::Deserialize, Default)]
struct RayTracingCensusParams {
    #[serde(default)]
    slots: Option<u32>,
    #[serde(default)]
    instances: Option<u32>,
}

/// # Errors
/// 光线求交不支持或资源缺席时回 internal；读回失败回 internal。
#[system]
pub(crate) fn ray_tracing_census(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: RayTracingCensusParams = crate::remote::common::params::parse_params_or_default(params)?;
    // 坑：ctx 借用 world，与后面的 res_mut 互斥——能力位先抄出来，ctx 留到只读阶段再取
    let supported = res::<Context>(world)?.ray_tracing_support().enabled();
    if !supported || world.get_resource::<RayScene>().is_none() {
        return Ok(json!({ "supported": supported, "enabled": false }));
    }
    let slot = res::<FramePool>(world)?.current_index();
    let used = res::<InstanceTables>(world)?.used_object_slots();
    let n_slots = used.min(p.slots.unwrap_or(16).min(64)) as usize;
    // 地址表：两帧槽各读前 n_slots 条，只列非 0（0 = 未就绪/缺席，合法）
    let mut address_tables = Vec::new();
    for frame in 0..2 {
        let bytes = {
            let mut tables = res_mut::<InstanceTables>(world)?;
            match tables.blas_address_ring(frame) {
                Some(ring) => read_buffer(
                    ring,
                    n_slots * BLAS_ADDRESS_SIZE as usize,
                    "BLAS 地址表读失败",
                )?,
                None => Vec::new(),
            }
        };
        let mut rows = Vec::new();
        for s in 0..n_slots {
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&bytes[s * 8..s * 8 + 8]);
            let address = u64::from_le_bytes(raw);
            if address != 0 {
                rows.push(json!({ "slot": s, "address": address }));
            }
        }
        address_tables.push(json!({ "frame_slot": frame, "nonzero": rows }));
    }
    // TLAS 实例数组：本槽前 count 条（count = 上一帧 TLAS 实例数）
    let count = world
        .get_resource::<RayTracingTargets>()
        .map_or(0, |t| t.last_instances())
        .min(p.instances.unwrap_or(8).min(64));
    let buffer = res::<InstanceTables>(world)?.tlas_instance_buffer(slot);
    let mut bytes = vec![0u8; count as usize * TLAS_INSTANCE_SIZE as usize];
    if let Some(buffer) = buffer {
        let ctx = res::<Context>(world)?;
        readback_buffer(ctx, buffer, &mut bytes).map_err(internal_err("TLAS 实例数组读回失败"))?;
    }
    let f32_at = |o: usize| f32::from_bits(u32_at(&bytes, o).unwrap_or(0));
    let mut instances = Vec::new();
    for i in 0..count as usize {
        let base = i * TLAS_INSTANCE_SIZE as usize;
        let transform: Vec<f32> = (0..12).map(|k| f32_at(base + k * 4)).collect();
        let custom_index_mask = u32_at(&bytes, base + 48)?;
        let sbt_flags = u32_at(&bytes, base + 52)?;
        let lo = u32_at(&bytes, base + 56)? as u64;
        let hi = u32_at(&bytes, base + 60)? as u64;
        instances.push(json!({
            "i": i,
            "transform": transform,
            "custom_index": custom_index_mask & 0xFFFFFF,
            "mask": custom_index_mask >> 24,
            "sbt_offset": sbt_flags & 0xFFFFFF,
            "flags": sbt_flags >> 24,
            "address": lo | (hi << 32),
        }));
    }
    Ok(json!({
        "supported": true,
        "enabled": true,
        "slot": slot,
        "used_object_slots": used,
        "address_tables": address_tables,
        "instances": instances,
    }))
}
