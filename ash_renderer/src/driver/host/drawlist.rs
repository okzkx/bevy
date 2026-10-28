//! DrawList 组装与帧槽环写入：从账本行到**可绘行槽位枚举**，以及模型环/
//! 材质环/在场环/LOD 变体表环的每帧写入。
//!
//! 本模块是"槽位枚举器"：逐行查实例槽与驻留账本/容器做可绘判定，产物是可绘行
//! 实例槽号列表（`slots`）——它写进在场环，作为 GPU 侧 build_indirect 的行宇宙
//!（槽位枚举 + 材质环写侧走访是账本在账信息，不是可见集回读）。行的 GPU 内容
//! 由对象表（写一次）+ 模型环/材质环（每帧重写）承载。
//! hazard 纪律：环写入只发生在 `draw_frame` 的 `wait_for_slot` 之后
//!（与本帧 UBO 同一条 fence 证据），详见 `render::resident::instance_table` 模块文档。
//!
//! 行级可绘判定：实例槽缺（未驻留成表）/锚 mesh 未驻留/材质容器缺席 = 该行暂缓
//!（Tier①，自愈后消失）；缺贴图 = fallback 白图照画。暂缓行不进在场环 = 自动
//! 无命令（暂缓语义由枚举源头承载，GPU 侧零特判）。
//!
//! 增量组装：枚举由账本侧 [`crate::scene::InstanceLedger`] 的组装缓存维护——本模块
//! 只注入可绘判定闭包（[`DrawabilitySources::eval`]，锚解析与 flush 锚腿同源）并
//! 触发 `assembly_update`；产物经 `AssemblyView` 借用，行宇宙稳定帧零走访零重建。
//! 全量参照走访（`assemble_reference`）是对照模式的比对基准，供 BRP verify 开关
//! 逐帧核对。

use bevy::asset::AssetId;
use bevy::pbr::StandardMaterial;
use bevy::prelude::*;
use std::collections::BTreeSet;

use crate::common::bytes::put_u32s;
use crate::common::error::VulkanError;
use crate::scene::{
    anchor_level, DrawListVerify, InstanceLedger, InstanceRow, LodGroups,
};
use crate::render::{
    pack_lod_entry, pack_material_record, pack_model, BindlessTables, ImageCache, InstanceTables,
    LOD_ENTRY_SIZE, LOD_LEVEL_SIZE, LOD_TABLE_STRIDE, MATERIAL_RECORD_SIZE, MAX_LOD_LEVELS,
    MODEL_STRIDE, MeshPool, NO_TEXTURE_INDEX,
};

/// 可绘判定的输入来源（驻留池/材质容器/LOD 组/保底锚开关）：增量组装、全量
/// 参照与环写入共用的一束引用。
#[derive(Clone, Copy)]
pub(crate) struct DrawabilitySources<'a> {
    pub(crate) pool: &'a MeshPool,
    pub(crate) std_materials: &'a Assets<StandardMaterial>,
    pub(crate) lod_groups: &'a LodGroups,
    /// 保底锚开关：开启时 LOD 组行的锚 = 组内保底档（最粗变体，level MAX-1），
    /// 尾档缺席退回基准。
    pub(crate) floor_anchor: bool,
}

impl<'a> DrawabilitySources<'a> {
    /// 可绘判定闭包：实例槽缺 / 锚未驻留 / 材质容器缺席 = None（暂缓）；
    /// 命中 = (槽号, 材质身份, alpha<1)。锚解析走 [`anchor_level`] 纯函数。
    pub(crate) fn eval(
        self,
    ) -> impl FnMut(&InstanceRow) -> Option<(u32, AssetId<StandardMaterial>, bool)> + 'a {
        move |row| {
            let slot = row.instance_slot?;
            let mesh_id = row.mesh.id();
            let anchor_id = match self.lod_groups.levels_of(mesh_id) {
                Some(levels) => {
                    let level = anchor_level(self.floor_anchor, levels[(MAX_LOD_LEVELS - 1) as usize].is_some());
                    levels[level as usize]
                        .as_ref()
                        .map(|handle| handle.id())
                        .unwrap_or(mesh_id)
                }
                None => mesh_id,
            };
            self.pool.resident(anchor_id)?;
            let material = self.std_materials.get(&row.material)?;
            let alpha = material.base_color.to_linear().alpha < 1.0;
            Some((slot, row.material.id(), alpha))
        }
    }
}

/// 组装 DrawList（增量入口）：触发账本组装缓存的重估/重建（判定闭包注入），
/// 产物经 `ledger.assembly_view()` 供帧循环全程只读。账本行宇宙不变帧零走访
/// 零 Vec 重建（缓存容量复用）；变更帧成本 O(重估行)。
pub(crate) fn assemble_update(ledger: &mut InstanceLedger, sources: DrawabilitySources) {
    ledger.assembly_update(
        &mut sources.eval(),
        sources.floor_anchor,
        sources.lod_groups.generation(),
    );
}

/// 全量参照走访（对照模式的真值面）：与增量缓存逐行同判定的 O(账本行) 枚举
/// ——只进比对，不进绘制。返回（槽号列表 Entity 序，去重材质身份，暂缓行数，
/// alpha<1 行数）。
pub(crate) fn assemble_reference(
    ledger: &InstanceLedger,
    sources: DrawabilitySources,
) -> (Vec<u32>, BTreeSet<AssetId<StandardMaterial>>, usize, usize) {
    let mut eval = sources.eval();
    let mut slots = Vec::with_capacity(ledger.len());
    let mut pending_rows = 0usize;
    let mut alpha_override = 0usize;
    let mut materials = BTreeSet::new();
    for (_entity, row) in ledger.rows() {
        match eval(row) {
            Some((slot, mat, alpha)) => {
                if alpha {
                    alpha_override += 1;
                }
                materials.insert(mat);
                slots.push(slot);
            }
            None => pending_rows += 1,
        }
    }
    (slots, materials, pending_rows, alpha_override)
}

/// 对照模式逐帧比对：全量参照 vs 增量缓存，逐字段核对（槽号列表逐行、暂缓数、
/// 材质集、alpha<1 计数），不一致进账面（不中断渲染——诊断面只记账）。
pub(crate) fn verify_assembly(
    ledger: &InstanceLedger,
    sources: DrawabilitySources,
    pending_rows: usize,
    verify: &mut DrawListVerify,
) {
    let (ref_slots, ref_materials, ref_pending, ref_alpha) = assemble_reference(ledger, sources);
    let view = ledger.assembly_view();
    verify.frames_compared_total += 1;
    let mut mismatch: Option<String> = None;
    if view.slots != ref_slots.as_slice() {
        let first_diff = view
            .slots
            .iter()
            .zip(ref_slots.iter())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| view.slots.len().min(ref_slots.len()));
        mismatch = Some(format!(
            "槽号列表不一致（增量 {} 行 vs 全量 {} 行，首差异位 {first_diff}）",
            view.slots.len(),
            ref_slots.len(),
        ));
    } else if pending_rows != ref_pending {
        mismatch = Some(format!("暂缓行数不一致（增量 {pending_rows} vs 全量 {ref_pending}）"));
    } else {
        let mut inc_mats: Vec<AssetId<StandardMaterial>> = view.materials.to_vec();
        inc_mats.sort_unstable();
        let ref_mats: Vec<AssetId<StandardMaterial>> = ref_materials.into_iter().collect();
        if inc_mats != ref_mats {
            mismatch = Some(format!(
                "材质集不一致（增量 {} 个 vs 全量 {} 个）",
                inc_mats.len(),
                ref_mats.len(),
            ));
        } else if view.alpha_override != ref_alpha {
            mismatch = Some(format!(
                "alpha<1 行数不一致（增量 {} vs 全量 {ref_alpha}）",
                view.alpha_override,
            ));
        }
    }
    if let Some(m) = mismatch {
        verify.mismatches_total += 1;
        verify.last_mismatch = Some(m);
    }
}

/// 本帧槽的奇偶脏集（模型环/LOD 表环）：须在取组装视图之前从账本取走——此后
/// 视图对账本共享借用，环写入全程零可变访问。
pub(crate) struct ParityDirty {
    model: Vec<Entity>,
    lod: Vec<Entity>,
}

impl ParityDirty {
    pub(crate) fn take(ledger: &mut InstanceLedger, frame: usize) -> Self {
        Self {
            model: ledger.take_model_dirty(frame),
            lod: ledger.take_lod_dirty(frame),
        }
    }
}

/// 写模型环（增量）：本帧奇偶集的改动行 → 本帧槽的环。改动行跨两个帧槽
/// 各写一次（两帧后两个环都新鲜），静态场景零写入。行已清/槽未落 =
/// 跳过（槽位在停尸房途中或从未分配，环内容无人读）。
fn write_model_ring(
    tables: &mut InstanceTables,
    ledger: &InstanceLedger,
    model_dirty: &[Entity],
    frame: usize,
) -> Result<(), VulkanError> {
    for entity in model_dirty {
        let Some(row) = ledger.row(*entity) else {
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
/// 容器/驻留缓存现读——材质参数变化与贴图槽换指自动次帧生效（flush 无需任何
/// 材质内容通道）。
///
/// 留全量不做增量：全量重写本身就是材质内容变更通道（增量写会把参数变化冻结
/// 在旧记录里，需另开内容变更通道才能补齐语义），简单性优先。
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
        // PBR 三贴图槽缺席写 NO_TEXTURE_INDEX 哨兵（shader 端分支跳过采样），
        // 标量域语义 = Filament gltfio 同款（roughness 记 perceptual 域、
        // reflectance 原值，clamp 换算归 shader 端）
        let optional_slot = |texture: Option<&bevy::asset::Handle<bevy::image::Image>>| {
            texture
                .and_then(|handle| image_cache.slots(handle.id()))
                .map(|slots| slots.texture)
                .unwrap_or(NO_TEXTURE_INDEX)
        };
        let bytes = pack_material_record(
            [color.red, color.green, color.blue, color.alpha],
            material.metallic,
            material.perceptual_roughness,
            material.reflectance,
            [
                slots.texture,
                optional_slot(material.normal_map_texture.as_ref()),
                optional_slot(material.metallic_roughness_texture.as_ref()),
                optional_slot(material.occlusion_texture.as_ref()),
            ],
            slots.sampler,
        );
        tables
            .material_ring(frame)
            .write(u64::from(slot) * MATERIAL_RECORD_SIZE, &bytes)?;
    }
    Ok(())
}

/// 写在场环（全量重写）：可绘行槽号列表 → 本帧槽的环。GPU 侧 build_indirect
/// 以本列表为行宇宙（退休槽/暂缓行不在列表 = 自动无命令），写发生条件与帧
/// UBO 同一条 fence 证据。列表本身是组装缓存的产物，本环每帧照写（环按帧槽
/// 双缓冲，快照语义不可省）。
fn write_presence_ring(
    tables: &mut InstanceTables,
    slots: &[u32],
    frame: usize,
) -> Result<(), VulkanError> {
    if slots.is_empty() {
        return Ok(()); // 空场：环内容无人读（dispatch 上界 = count = 0）
    }
    let mut bytes = vec![0u8; slots.len() * 4];
    put_u32s(&mut bytes, 0, slots);
    tables.presence_ring(frame).write(0, &bytes)?;
    Ok(())
}

/// 写 LOD 变体表环（增量）：LOD 脏行（换槽/档位资产到货）→ 本帧槽的表环写该槽
/// 各档绘制引脚。level 0 = 本行基准 mesh 槽位（保底锚开启时基准降为按需高档，
/// 未驻留 = 哨兵保持 0，消费端 rec 回退引脚此时即保底档引脚——flush 锚腿以保底
/// 槽写记录；保底锚关闭时基准恒驻留）；level 1.. = 登记组变体引脚，未驻留 =
/// 哨兵 0。同槽的档位环一并写 0（换槽后迟滞状态从最细档重新收敛——与描述符
/// init 零填充同语义）。
fn write_lod_table_ring(
    tables: &mut InstanceTables,
    ledger: &InstanceLedger,
    pool: &MeshPool,
    lod_groups: &LodGroups,
    lod_dirty: &[Entity],
    frame: usize,
) -> Result<(), VulkanError> {
    for entity in lod_dirty {
        let Some(row) = ledger.row(*entity) else {
            continue; // 行已清：槽位在停尸房途中或从未分配，表内容无人读
        };
        let Some(slot) = row.instance_slot else {
            continue;
        };
        let mut bytes = vec![0u8; LOD_TABLE_STRIDE as usize];
        if let Some(mesh_slot) = pool.resident(row.mesh.id()) {
            let entry = pack_lod_entry(mesh_slot.first_index(), mesh_slot.index_count, mesh_slot.vertex_base());
            bytes[0..LOD_ENTRY_SIZE as usize].copy_from_slice(&entry);
        }
        if let Some(levels) = lod_groups.levels_of(row.mesh.id()) {
            for (level, handle) in levels.iter().enumerate().skip(1) {
                let Some(handle) = handle else { continue };
                let Some(mesh_slot) = pool.resident(handle.id()) else { continue }; // 该档未驻留 = 哨兵保持 0
                let o = level * LOD_ENTRY_SIZE as usize;
                let entry = pack_lod_entry(mesh_slot.first_index(), mesh_slot.index_count, mesh_slot.vertex_base());
                bytes[o..o + LOD_ENTRY_SIZE as usize].copy_from_slice(&entry);
            }
        }
        tables.lod_table_ring(frame).write(u64::from(slot) * LOD_TABLE_STRIDE, &bytes)?;
        // 迟滞状态复位：换槽 = 新对象，从最细档重新开始（该槽号写入本帧槽环，
        // 另一帧槽环随奇偶脏集下帧补写）
        tables
            .lod_level_ring(frame)
            .write(u64::from(slot) * LOD_LEVEL_SIZE, &0u32.to_le_bytes())?;
    }
    Ok(())
}

/// 帧槽环写入入口（draw_frame 在 wait_for_slot 之后调用）：模型增量 + 材质
/// 全量 + 在场环全量 + LOD 表增量都落在本帧槽的环上。失败按 Tier② 冒泡
///（环写失败 = host-visible 契约被打破，帧循环无法继续）。
///
/// 借用纪律：`ledger` 与 `assembly` 都是账本的共享借用（组装产物住账本缓存）
/// ——奇偶集由调用方在取视图之前先行取走（[`ParityDirty::take`]），本函数对
/// 账本零可变访问。
///
/// # Errors
/// 环 buffer 写失败（host-visible 缺席/越界，内部契约矛盾级）。
pub(crate) fn write_rings(
    tables: &mut InstanceTables,
    ledger: &InstanceLedger,
    sources: DrawabilitySources,
    image_cache: &ImageCache,
    dirty: &ParityDirty,
    assembly: &crate::scene::mechanism::AssemblyView<'_>,
    frame: usize,
) -> Result<(), VulkanError> {
    write_model_ring(tables, ledger, &dirty.model, frame)?;
    write_material_ring(tables, sources.std_materials, image_cache, assembly.materials, frame)?;
    write_presence_ring(tables, assembly.slots, frame)?;
    write_lod_table_ring(tables, ledger, sources.pool, sources.lod_groups, &dirty.lod, frame)?;
    Ok(())
}
