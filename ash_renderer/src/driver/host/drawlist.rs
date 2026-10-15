//! DrawList 组装与双环写入（5.1）：从账本行到 `DrawCall` 的**槽位枚举**，
//! 以及模型环/材质环的每帧写入。
//!
//! 5.1 分工：本模块是"槽位枚举器"——逐行查实例槽与池引脚组 `DrawCall`
//!（push 只剩 4B 实例槽号），逐对象绘制参数的组装职责已随 PushData 退役；
//! 行的 GPU 内容由对象表（写一次）+ 模型环/材质环（每帧重写）承载。
//! hazard 纪律：环写入只发生在 `draw_frame` 的 `wait_for_slot` 之后
//!（与本帧 UBO 同一条 fence 证据），详见 `vulkan::instance_table` 模块文档。
//!
//! 行级可绘判定与 4.x 语义一致：实例槽缺（未驻留成表）/mesh 未驻留/材质
//! 容器缺席 = 该行暂缓（Tier①，自愈后消失）；缺贴图 = fallback 白图照画。

use bevy::asset::AssetId;
use bevy::pbr::StandardMaterial;
use bevy::prelude::*;
use std::collections::HashSet;

use crate::common::error::VulkanError;
use crate::scene::InstanceLedger;
use crate::vulkan::{
    pack_material_record, pack_model, BindlessTables, DrawCall, ImageCache, InstanceTables,
    MATERIAL_RECORD_SIZE, MODEL_STRIDE, MeshPool,
};

/// 一帧 DrawList 组装产物。
pub(crate) struct DrawAssembly {
    /// 槽位枚举（录制层逐 draw push 4B 实例槽号）。
    pub draws: Vec<DrawCall>,
    /// 暂缓行数（实例槽缺/mesh 未驻留/材质缺席）。
    pub pending_rows: usize,
    /// alpha<1 的行数（不透明覆盖收账日志用）。
    pub alpha_override: usize,
    /// 可绘行的去重材质身份（材质环全量重写的输入）。
    pub materials: Vec<AssetId<StandardMaterial>>,
}

/// 组装 DrawList：账本行 → 实例槽 + 驻留账本/资产容器 → 槽位 + 池引脚。
/// 账本读是纯 BTreeMap 走访——零组件扫描零 tick 比较（4.1 语义不变）。
pub(crate) fn assemble(
    ledger: &InstanceLedger,
    pool: &MeshPool,
    std_materials: &Assets<StandardMaterial>,
) -> DrawAssembly {
    let mut draws: Vec<DrawCall> = Vec::with_capacity(ledger.len());
    let mut pending_rows = 0usize;
    let mut alpha_override = 0usize;
    let mut material_seen: HashSet<AssetId<StandardMaterial>> = HashSet::new();
    for (_entity, row) in ledger.rows() {
        let Some(slot) = row.instance_slot else {
            pending_rows += 1;
            continue;
        };
        let Some(mesh_slot) = pool.resident(row.mesh.id()) else {
            pending_rows += 1;
            continue;
        };
        let Some(material) = std_materials.get(&row.material) else {
            pending_rows += 1;
            continue;
        };
        let color = material.base_color.to_linear();
        if color.alpha < 1.0 {
            alpha_override += 1;
        }
        material_seen.insert(row.material.id());
        draws.push(DrawCall {
            instance: slot,
            vertex_base: mesh_slot.vertex_base(),
            first_index: mesh_slot.first_index(),
            index_count: mesh_slot.index_count,
        });
    }
    DrawAssembly {
        draws,
        pending_rows,
        alpha_override,
        materials: material_seen.into_iter().collect(),
    }
}

/// 写模型环（增量）：本帧奇偶集的改动行 → 本帧槽的环。改动行跨两个帧槽
/// 各写一次（两帧后两个环都新鲜），静态场景零写入。行已清/槽未落 =
/// 跳过（槽位在停尸房途中或从未分配，环内容无人读）。
fn write_model_ring(
    tables: &mut InstanceTables,
    ledger: &mut InstanceLedger,
    frame: usize,
) -> Result<(), VulkanError> {
    for entity in ledger.take_model_dirty(frame) {
        let Some(row) = ledger.row(entity) else {
            continue;
        };
        let Some(slot) = row.instance_slot else {
            continue;
        };
        let bytes = pack_model(&row.model);
        tables.model_ring(frame).write(u64::from(slot) * MODEL_STRIDE, &bytes)?;
    }
    Ok(())
}

/// 写材质环（全量重写）：可绘行的去重材质 → 本帧槽的环。内容每帧从资产
/// 容器/驻留缓存现读——材质参数变化与贴图槽换指自动次帧生效（4.2 的
/// 材质半边语义由本环吸收，flush 无需任何材质内容通道）。
fn write_material_ring(
    tables: &mut InstanceTables,
    std_materials: &Assets<StandardMaterial>,
    image_cache: &ImageCache,
    materials: &[AssetId<StandardMaterial>],
    frame: usize,
) -> Result<(), VulkanError> {
    for id in materials {
        let Some(material) = std_materials.get(*id) else {
            continue; // 行级判定已暂缓缺材质的行，环侧不补写
        };
        let Some(slot) = tables.material_slot(*id) else {
            continue; // 实例槽不变式保证可绘行的材质槽必在；防御性跳过
        };
        let color = material.base_color.to_linear();
        let slots = material
            .base_color_texture
            .as_ref()
            .and_then(|handle| image_cache.slots(handle.id()))
            .unwrap_or_else(BindlessTables::fallback_slots);
        let bytes = pack_material_record(
            [color.red, color.green, color.blue, color.alpha],
            slots.texture,
            slots.sampler,
        );
        tables
            .material_ring(frame)
            .write(u64::from(slot) * MATERIAL_RECORD_SIZE, &bytes)?;
    }
    Ok(())
}

/// 双环写入入口（draw_frame 在 wait_for_slot 之后调用）：模型增量 + 材质
/// 全量都落在本帧槽的环上。失败按 Tier② 冒泡（环写失败 = host-visible
/// 契约被打破，帧循环无法继续）。
///
/// # Errors
/// 环 buffer 写失败（host-visible 缺席/越界，内部契约矛盾级）。
pub(crate) fn write_rings(
    tables: &mut InstanceTables,
    ledger: &mut InstanceLedger,
    std_materials: &Assets<StandardMaterial>,
    image_cache: &ImageCache,
    assembly: &DrawAssembly,
    frame: usize,
) -> Result<(), VulkanError> {
    write_model_ring(tables, ledger, frame)?;
    write_material_ring(tables, std_materials, image_cache, &assembly.materials, frame)?;
    Ok(())
}
