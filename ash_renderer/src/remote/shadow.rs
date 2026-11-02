//! 阴影方法族：`shadow_set` 写阴影开关，`shadow_stats` 做逐 cascade GPU 标记 vs
//! CPU 参照的对照（读 shadow UBO 的 cascade 矩阵/平面/分割）。只读标记/只写开关，
//! 读数不回灌绘制决策。

use ash_macros::system;
use bevy::{
    prelude::*,
    remote::{BrpError, BrpResult},
};
use serde_json::{json, Value};

use crate::{
    render::{
        aabb_visible, conservative_world_aabb, MeshPool,
        ShadowDiagnostics, ShadowSettings, ShadowTargets, OBJECT_TABLE_CAPACITY,
        SHADOW_CASCADES, SHADOW_UBO_SIZE,
    },
    scene::InstanceLedger,
};
use super::{
    params::{parse_enabled, res, res_mut},
    readback::{f32_at, read_buffer, u32_at},
};

// shadow UBO 字节偏移：镜像 render 侧 `ShadowUniformsLayout` 的静态断言表，布局改动
// 须两处同改。cascade_view_proj 在偏移 0（每 cascade 一个 64B mat4）。
const UBO_PLANES: usize = 256; // 每 cascade 6 个 vec4 平面
const UBO_SPLITS: usize = 640;
const UBO_TEXEL_WORLD: usize = 704;
const UBO_INV_SPAN: usize = 720;

/// `ash_renderer/shadow_set`：阴影开关（A/B 对照驱动面），回写前值。
/// 请求 `{"enabled": bool}`；关 = shadow 剔除/命令/绘制/采样全链旁路。
#[system]
pub(crate) fn shadow_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let enabled = parse_enabled(params)?;
    let mut settings = res_mut::<ShadowSettings>(world)?;
    let previous = std::mem::replace(&mut settings.enabled, enabled);
    Ok(json!({ "enabled": enabled, "previous": previous }))
}

/// `ash_renderer/shadow_stats`：逐 cascade GPU 标记 vs CPU 参照对照读数面。
/// 读数 = marks_frame 槽两帧前 compute 输出（marks_frame 标注陈旧度）。
#[system]
pub(crate) fn shadow_stats(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    // 读数快照先取值，避免与下方标记环可变借用交叠
    let (marks_frame, diag_enabled) = {
        let d = res::<ShadowDiagnostics>(world)?;
        (d.marks_frame, d.enabled)
    };

    // shadow UBO 读 marks_frame 槽：与 GPU 标记同帧同源，CPU 参照的输入才一致
    let ubo_bytes = read_buffer(
        res_mut::<ShadowTargets>(world)?.ubo(marks_frame),
        SHADOW_UBO_SIZE as usize,
        "shadow UBO 读失败",
    )?;
    let mut planes_per_cascade = [[Vec4::ZERO; 6]; SHADOW_CASCADES];
    for (c, cascade) in planes_per_cascade.iter_mut().enumerate() {
        for (p, plane) in cascade.iter_mut().enumerate() {
            let base = UBO_PLANES + (c * 6 + p) * 16;
            *plane = Vec4::new(
                f32_at(&ubo_bytes, base)?,
                f32_at(&ubo_bytes, base + 4)?,
                f32_at(&ubo_bytes, base + 8)?,
                f32_at(&ubo_bytes, base + 12)?,
            );
        }
    }

    // CPU 参照：账本行 × 驻留局部包围盒 → 世界保守盒 → 逐 cascade 平面求交
    //（保守语义：相交即可见，宁多画不错杀）
    let pool = res::<MeshPool>(world)?;
    let mut reference: Vec<(u32, [bool; SHADOW_CASCADES])> = Vec::new();
    for (_entity, row) in res::<InstanceLedger>(world)?.rows() {
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
        let vis = std::array::from_fn(|c| aabb_visible(&planes_per_cascade[c], wmin.into(), wmax.into()));
        reference.push((slot, vis));
    }

    // GPU 标记直读（交错 [槽][cascade]，每槽 4 cascade × u32 = 16B）
    let gpu = read_buffer(
        res_mut::<ShadowTargets>(world)?.mark_mut(marks_frame),
        OBJECT_TABLE_CAPACITY as usize * 16,
        "shadow 标记环读失败",
    )?;

    let mut mismatches = [0u32; SHADOW_CASCADES];
    let mut gpu_visible = [0u32; SHADOW_CASCADES];
    let mut cpu_visible = [0u32; SHADOW_CASCADES];
    for (slot, cpu) in reference {
        for (c, &cpu_vis) in cpu.iter().enumerate() {
            let gpu_vis = u32_at(&gpu, slot as usize * 16 + c * 4)? == 1;
            gpu_visible[c] += u32::from(gpu_vis);
            cpu_visible[c] += u32::from(cpu_vis);
            mismatches[c] += u32::from(gpu_vis != cpu_vis);
        }
    }
    let diag = res::<ShadowDiagnostics>(world)?;
    let splits = (0..SHADOW_CASCADES)
        .map(|k| f32_at(&ubo_bytes, UBO_SPLITS + k * 4))
        .collect::<Result<Vec<f32>, _>>()?;
    let mat_at = |c: usize, row: usize, col: usize| {
        // to_cols_array 列主序：flat = col*4 + row
        f32_at(&ubo_bytes, c * 64 + (col * 4 + row) * 4)
    };
    let cascades_detail = (0..SHADOW_CASCADES)
        .map(|c| {
            let view_proj = (0..4)
                .map(|r| (0..4).map(|k| mat_at(c, r, k)).collect::<Result<Vec<_>, _>>())
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({
                "gpu_visible": gpu_visible[c],
                "cpu_visible": cpu_visible[c],
                "mismatches": mismatches[c],
                "cascade_view_proj": view_proj,
                "texel_world": f32_at(&ubo_bytes, UBO_TEXEL_WORLD + c * 4)?,
                "inv_span": f32_at(&ubo_bytes, UBO_INV_SPAN + c * 4)?,
                "planes": planes_per_cascade[c].iter().map(|p| p.to_array()).collect::<Vec<_>>(),
            }))
        })
        .collect::<Result<Vec<Value>, BrpError>>()?;
    Ok(json!({
        "enabled": diag_enabled,
        "bound": diag.bound,
        "marks_frame": marks_frame,
        "splits": splits,
        "cascades": cascades_detail,
        "note": "读数 = 本帧槽两帧前 compute 输出（fence 纪律内陈旧 ≤2 帧，与主可见标记一致）",
    }))
}
