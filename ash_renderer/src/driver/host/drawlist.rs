//! DrawList 组装与帧槽环写入：账本行 → 可绘行的实例槽号列表 → 写进模型环/材质环/
//! 在场环/LOD 变体表环。
//!
//! 槽号列表写进在场环，GPU 侧 build_indirect 只为列表里的行组装命令；列表来自账本
//! 信息，不是可见集回读。行的 GPU 内容由对象表（写一次）+ 模型环/材质环（每帧）承载。
//! 环写入只发生在 `draw_frame` 的 `wait_for_slot` 之后（fence 证据同本帧 UBO，详见
//! `render::resident::tables::instance_table` 模块文档）。
//!
//! 行级可绘判定：实例槽缺/锚 mesh 未驻留/材质容器缺席 = 该行暂缓（就绪后自愈）；
//! 缺贴图 = fallback 白图照画。暂缓行不进在场环，自然没有绘制命令，GPU 侧无特判。
//!
//! 增量组装：枚举由 [`crate::scene::InstanceLedger`] 的组装缓存维护——本模块只注入
//! 可绘判定闭包（[`DrawabilitySources::eval`]）并触发 `assembly_update`，账本行集不变的
//! 帧零走访零重建。全量走访 [`assemble_reference`] 只用于 BRP 对照模式逐帧核对。

use bevy::asset::AssetId;
use bevy::pbr::StandardMaterial;
use bevy::prelude::*;
use std::collections::BTreeSet;

use crate::common::bytes::put_u32s;
use crate::common::error::VulkanError;
use crate::common::syntax::ResultTierExt;
use crate::scene::{
    anchor_level, AssemblyView, DrawListVerify, InstanceLedger, InstanceRow, LodGroups,
};
use crate::render::{
    pack_lod_entry, pack_material_record, pack_model, BindlessTextureTable, ImageCache, InstanceTables,
    LOD_ENTRY_SIZE, LOD_LEVEL_SIZE, LOD_TABLE_STRIDE, MATERIAL_RECORD_SIZE, MAX_LOD_LEVELS,
    MODEL_STRIDE, MeshPool, NO_TEXTURE_INDEX,
};

/// 可绘判定的输入来源：增量组装、全量参照与环写入共用的一束引用。
#[derive(Clone, Copy)]
pub(crate) struct DrawabilitySources<'a> {
    pub(crate) pool: &'a MeshPool,
    pub(crate) std_materials: &'a Assets<StandardMaterial>,
    pub(crate) lod_groups: &'a LodGroups,
    /// 保底锚开关：开启时 LOD 组行的锚 = 组内最粗变体（level MAX-1），该档缺席退回基准。
    pub(crate) floor_anchor: bool,
}

impl<'a> DrawabilitySources<'a> {
    /// 可绘锚解析来源（组装、对照与环写共用；传字段引用——收整个 scene 会钉死
    /// 后续对 ledger 的可变借用）。
    pub(crate) fn new(
        pool: &'a MeshPool,
        std_materials: &'a Assets<StandardMaterial>,
        lod_groups: &'a LodGroups,
        floor_anchor: bool,
    ) -> Self {
        Self { pool, std_materials, lod_groups, floor_anchor }
    }

    /// 可绘判定闭包：实例槽缺 / 锚未驻留 / 材质容器缺席 = None（暂缓）；
    /// 可绘 = (槽号, 材质身份, alpha<1)。
    pub(crate) fn eval(
        self,
    ) -> impl FnMut(&InstanceRow) -> Option<(u32, AssetId<StandardMaterial>, bool)> + 'a {
        move |row| {
            let slot = row.instance_slot?;
            let mesh_id = row.mesh.id();
            let anchor_id = match self.lod_groups.levels_of(mesh_id) {
                Some(levels) => {
                    let level = anchor_level(self.floor_anchor, levels[(MAX_LOD_LEVELS - 1) as usize].is_some());
                    levels[level as usize].as_ref().map_or(mesh_id, Handle::id)
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

/// 组装 DrawList（增量）：触发账本组装缓存重估，产物经 `ledger.assembly_view()`
/// 供帧循环只读。行集不变帧零走访，变更帧成本 O(重估行)。
/// 尾随跑对照模式（BRP 开关，[`DrawListVerify`]）：开启时全量走访与增量结果
/// 逐字段比对，只进计量；比对成本计入本段 assemble 计时（仅开启时存在）。
pub(crate) fn assemble_update(
    ledger: &mut InstanceLedger,
    sources: DrawabilitySources,
    verify: &mut DrawListVerify,
) {
    ledger.assembly_update(
        &mut sources.eval(),
        sources.floor_anchor,
        sources.lod_groups.generation(),
    );
    if verify.enabled {
        let pending = ledger.assembly_pending();
        verify_assembly(ledger, sources, pending, verify);
    }
}

/// 全量参照走访：与增量缓存同判定的 O(账本行) 枚举，只进比对不进绘制。
/// 返回（槽号列表 Entity 序，去重材质身份，暂缓行数，alpha<1 行数）。
fn assemble_reference(
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

/// 对照模式逐帧比对：全量参照 vs 增量缓存（槽号列表、暂缓数、材质集、alpha<1 计数），
/// 不一致只记进账面，不中断渲染。
fn verify_assembly(
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
        let mut inc_mats = view.materials.to_vec();
        inc_mats.sort_unstable();
        let ref_mats: Vec<_> = ref_materials.into_iter().collect();
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
/// 视图对账本共享借用，环写入全程不再可变访问账本。
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

/// 账本行及其实例槽。行已清/槽未落 = None：槽位在回收途中或从未分配，环内容无人
/// 读，调用方跳过。
fn row_with_slot(ledger: &InstanceLedger, entity: Entity) -> Option<(&InstanceRow, u32)> {
    let row = ledger.row(entity)?;
    Some((row, row.instance_slot?))
}

/// 写模型环（增量）：改动行跨两个帧槽各写一次（两帧后两个环都新鲜），静态场景零写入。
fn write_model_ring(
    tables: &mut InstanceTables,
    ledger: &InstanceLedger,
    model_dirty: &[Entity],
    frame: usize,
) -> Result<(), VulkanError> {
    for entity in model_dirty {
        let Some((row, slot)) = row_with_slot(ledger, *entity) else { continue };
        let bytes = pack_model(&row.model);
        tables.model_ring(frame).write(u64::from(slot) * MODEL_STRIDE, &bytes)?;
    }
    Ok(())
}

/// 写材质环（全量）：可绘行的去重材质 → 本帧槽的环。内容每帧从资产容器/驻留缓存
/// 现读，材质参数变化与贴图换槽自动次帧生效。
///
/// 不做增量：全量重写本身就是材质内容的变更通道，增量写会把参数变化冻结在旧记录
/// 里，还得另开通道补齐。
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
            continue; // 可绘行的材质槽必在；防御性跳过
        };
        let color = material.base_color.to_linear();
        let resident = |texture: Option<&Handle<Image>>| {
            texture.and_then(|handle| image_cache.slots(handle.id()))
        };
        let slots = resident(material.base_color_texture.as_ref())
            .unwrap_or_else(BindlessTextureTable::fallback_slots);
        // 其余三贴图缺席写 NO_TEXTURE_INDEX（shader 跳过采样）。roughness 记 perceptual
        // 域、reflectance 记原值，clamp 与换算在 shader 端
        let optional_slot = |texture: Option<&Handle<Image>>| {
            resident(texture).map_or(NO_TEXTURE_INDEX, |slots| slots.texture)
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

/// 写在场环（全量）：可绘行槽号列表 → 本帧槽的环。列表来自组装缓存，但环按帧槽
/// 双缓冲，每帧都要写。
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
    tables.presence_ring(frame).write(0, &bytes)
}

/// 写 LOD 变体表环（增量）：换槽或档位资产到货的行 → 本帧槽的表环写该槽各档绘制
/// 参数，未驻留的档保持 0（消费端回退）。level 0 = 本行基准 mesh；保底锚开启时
/// 基准可能未驻留，此时回退对象记录的参数，即保底档。同槽的档位环一并清 0：换槽 =
/// 新对象，从最细档重新收敛（另一帧槽的环随奇偶脏集下帧补写）。
fn write_lod_table_ring(
    tables: &mut InstanceTables,
    ledger: &InstanceLedger,
    pool: &MeshPool,
    lod_groups: &LodGroups,
    lod_dirty: &[Entity],
    frame: usize,
) -> Result<(), VulkanError> {
    for entity in lod_dirty {
        let Some((row, slot)) = row_with_slot(ledger, *entity) else { continue };
        let mut bytes = vec![0u8; LOD_TABLE_STRIDE as usize];
        let mut put_entry = |level: usize, id| {
            if let Some(mesh_slot) = pool.resident(id) {
                let o = level * LOD_ENTRY_SIZE as usize;
                let entry = pack_lod_entry(mesh_slot.first_index(), mesh_slot.index_count, mesh_slot.vertex_base());
                bytes[o..o + LOD_ENTRY_SIZE as usize].copy_from_slice(&entry);
            }
        };
        put_entry(0, row.mesh.id());
        if let Some(levels) = lod_groups.levels_of(row.mesh.id()) {
            for (level, handle) in levels.iter().enumerate().skip(1) {
                if let Some(handle) = handle {
                    put_entry(level, handle.id());
                }
            }
        }
        tables.lod_table_ring(frame).write(u64::from(slot) * LOD_TABLE_STRIDE, &bytes)?;
        tables
            .lod_level_ring(frame)
            .write(u64::from(slot) * LOD_LEVEL_SIZE, &0u32.to_le_bytes())?;
    }
    Ok(())
}

/// 帧槽环写入入口（draw_frame 在 wait_for_slot 之后调用）：模型增量 + 材质全量 +
/// 在场全量 + LOD 表增量，都落在本帧槽的环上。任一环失败 = 渲染链无法继续，按致命冒泡。
pub(crate) fn write_rings(
    tables: &mut InstanceTables,
    ledger: &InstanceLedger,
    sources: DrawabilitySources,
    image_cache: &ImageCache,
    dirty: &ParityDirty,
    assembly: &AssemblyView<'_>,
    frame: usize,
) -> Result<(), BevyError> {
    write_all_rings(tables, ledger, sources, image_cache, dirty, assembly, frame)
        .or_fatal("模型/材质/在场/LOD 环写失败，渲染链无法继续")
}

fn write_all_rings(
    tables: &mut InstanceTables,
    ledger: &InstanceLedger,
    sources: DrawabilitySources,
    image_cache: &ImageCache,
    dirty: &ParityDirty,
    assembly: &AssemblyView<'_>,
    frame: usize,
) -> Result<(), VulkanError> {
    write_model_ring(tables, ledger, &dirty.model, frame)?;
    write_material_ring(tables, sources.std_materials, image_cache, assembly.materials, frame)?;
    write_presence_ring(tables, assembly.slots, frame)?;
    write_lod_table_ring(tables, ledger, sources.pool, sources.lod_groups, &dirty.lod, frame)
}
