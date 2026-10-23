use ash_macros::system;
use bevy::prelude::*;
use serde_json::{json, Value};

use crate::{
    common::error::VulkanError,
    render::{
        aabb_visible, conservative_world_aabb, MeshPool,
        ShadowDiagnostics, ShadowSettings, ShadowTargets, OBJECT_TABLE_CAPACITY,
        SHADOW_CASCADES, SHADOW_UBO_SIZE,
    },
    scene::InstanceLedger,
};
use super::{error_codes, invalid_params, BrpError, BrpResult};

fn component_error(message: String) -> BrpError {
    BrpError { code: error_codes::COMPONENT_ERROR, message, data: None }
}

/// `ash_renderer/shadow_set`：阴影开关（A/B 判定线的驱动面）。
/// 请求 `{"enabled": bool}`；关 = shadow 剔除/命令/绘制/采样全链旁路。
#[system]
pub(crate) fn shadow_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要 enabled"))?;
    let p: serde_json::Map<String, Value> =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    let enabled = p
        .get("enabled")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| invalid_params("enabled 缺席或非布尔"))?;
    let old = world.resource::<ShadowSettings>().enabled;
    world.resource_mut::<ShadowSettings>().enabled = enabled;
    Ok(json!({ "enabled": enabled, "previous": old }))
}

/// `ash_renderer/shadow_stats`：逐 cascade GPU 标记 vs CPU 参照对照（判定线 3
/// 的读数面）。读数 = 本帧槽两帧前 compute 输出（marks_frame 标注陈旧度）。
#[system]
pub(crate) fn shadow_stats(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    // 读数快照先行取值（避免与下方标记环可变借用交叠）
    let (marks_frame, diag_enabled) = {
        let d = world.resource::<ShadowDiagnostics>();
        (d.marks_frame, d.enabled)
    };

    // shadow UBO 读回：取 marks_frame 槽的平面族（与 GPU 标记同帧同源——
    // CPU 参照对照的输入一致性前提）
    let mut ubo_bytes = vec![0u8; SHADOW_UBO_SIZE as usize];
    world
        .resource_mut::<ShadowTargets>()
        .ubo(marks_frame)
        .read(0, &mut ubo_bytes)
        .map_err(|e: VulkanError| component_error(format!("shadow UBO 读失败: {e}")))?;
    let f32_at = |o: usize| f32::from_le_bytes(ubo_bytes[o..o + 4].try_into().expect("4B"));
    let mut planes_per_cascade = [[bevy::math::Vec4::ZERO; 6]; SHADOW_CASCADES];
    for (c, cascade) in planes_per_cascade.iter_mut().enumerate() {
        for (p, plane) in cascade.iter_mut().enumerate() {
            let base = 256 + (c * 6 + p) * 16;
            *plane = bevy::math::Vec4::new(
                f32_at(base),
                f32_at(base + 4),
                f32_at(base + 8),
                f32_at(base + 12),
            );
        }
    }

    // CPU 参照：账本行 × 驻留局部包围盒 → 世界保守盒 → 逐 cascade 平面求交
    //（cull_stats 同款保守语义：相交即可见，宁多画不错杀）
    let pool = world.resource::<MeshPool>();
    let mut reference: Vec<(u32, [bool; SHADOW_CASCADES])> = Vec::new();
    for (_entity, row) in world.resource::<InstanceLedger>().rows() {
        let Some(slot) = row.instance_slot else {
            continue;
        };
        let Some(mesh_slot) = pool.resident(row.mesh.id()) else {
            continue;
        };
        let (wmin, wmax) = conservative_world_aabb(
            &row.model,
            mesh_slot.local_aabb.0,
            mesh_slot.local_aabb.1,
        );
        let mut vis = [false; SHADOW_CASCADES];
        for (c, planes) in planes_per_cascade.iter().enumerate() {
            vis[c] = aabb_visible(planes, wmin.into(), wmax.into());
        }
        reference.push((slot, vis));
    }

    // GPU 标记直读（交错 [槽][cascade]，4×u32/槽）
    let mut gpu = vec![0u8; OBJECT_TABLE_CAPACITY as usize * 4 * 4];
    world
        .resource_mut::<ShadowTargets>()
        .mark_mut(marks_frame)
        .read(0, &mut gpu)
        .map_err(|e: VulkanError| component_error(format!("shadow 标记环读失败: {e}")))?;
    let smark_at = |slot: u32, c: usize| {
        let o = slot as usize * 16 + c * 4;
        u32::from_le_bytes(gpu[o..o + 4].try_into().expect("4B"))
    };

    let mut mismatches = [0u32; SHADOW_CASCADES];
    let mut gpu_visible = [0u32; SHADOW_CASCADES];
    let mut cpu_visible = [0u32; SHADOW_CASCADES];
    for (slot, cpu) in reference {
        for (c, &cpu_vis) in cpu.iter().enumerate() {
            let gpu_vis = smark_at(slot, c) == 1;
            gpu_visible[c] += u32::from(gpu_vis);
            cpu_visible[c] += u32::from(cpu_vis);
            mismatches[c] += u32::from(gpu_vis != cpu_vis);
        }
    }
    let diag = world.resource::<ShadowDiagnostics>();
    let splits: Vec<f32> = (0..SHADOW_CASCADES).map(|k| f32_at(640 + k * 4)).collect();
    let mat_at = |c: usize, row: usize, col: usize| {
        // to_cols_array 列主序：flat = col*4 + row
        f32_at(c * 64 + (col * 4 + row) * 4)
    };
    let cascades_detail: Vec<serde_json::Value> = (0..SHADOW_CASCADES)
        .map(|c| {
            json!({
                "gpu_visible": gpu_visible[c],
                "cpu_visible": cpu_visible[c],
                "mismatches": mismatches[c],
                "cascade_view_proj": (0..4).map(|r| (0..4).map(|k| mat_at(c, r, k)).collect::<Vec<_>>()).collect::<Vec<_>>(),
                "texel_world": f32_at(704 + c * 4),
                "inv_span": f32_at(720 + c * 4),
                "planes": (0..6).map(|p| {
                    let base = 256 + (c * 6 + p) * 16;
                    (0..4).map(|k| f32_at(base + k * 4)).collect::<Vec<_>>()
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "enabled": diag_enabled,
        "bound": diag.bound,
        "marks_frame": marks_frame,
        "splits": splits,
        "cascades": cascades_detail,
        "note": "读数 = 本帧槽两帧前 compute 输出（fence 纪律内陈旧 ≤2 帧，主标记同款）",
    }))
}
