//! DrawList 组装与双环写入（5.1/5.3/7.9）：从账本行到**可绘行槽位枚举**，
//! 以及模型环/材质环/在场环的每帧写入。
//!
//! 5.1 分工：本模块是"槽位枚举器"——逐行查实例槽与驻留账本/容器做可绘判定；
//! 5.3 起 `DrawCall`（槽号 + 池引脚）退役，枚举产物只剩可绘行实例槽号列表
//!（`slots`）——它写进在场环（GPU 侧 build_indirect 的行宇宙），录制层的
//! 逐 draw 循环与逐对象 push 一并退役（判定线 1 的走访侧口径：槽位枚举 +
//! 材质环写侧走访是账本在账信息，不是可见集回读；绘制调用与 push 字节恒定）。
//! 行的 GPU 内容由对象表（写一次）+ 模型环/材质环（每帧重写）承载。
//! hazard 纪律：环写入只发生在 `draw_frame` 的 `wait_for_slot` 之后
//!（与本帧 UBO 同一条 fence 证据），详见 `render::assets::instance_table` 模块文档。
//!
//! 行级可绘判定与 4.x 语义一致：实例槽缺（未驻留成表）/mesh 未驻留/材质
//! 容器缺席 = 该行暂缓（Tier①，自愈后消失）；缺贴图 = fallback 白图照画。
//! 暂缓行不进在场环 = 自动无命令（5.3 待决 6 的钉明：暂缓语义由枚举源头
//! 承载，GPU 侧零特判）。6.2 起 LOD 变体表环的脏行增量写也在本模块
//!（write_lod_table_ring——模型环同款两帧奇偶纪律，输入 = 账本 LOD 脏集）。
//!
//! 7.9 增量组装：`assemble` 的 O(账本行) 全走访改为账本侧
//! [`crate::scene::InstanceLedger`] 的组装缓存维护——本模块只注入可绘判定
//! 闭包（`drawability_eval`，锚解析与 6.5 flush 锚腿同源）并触发
//! `assembly_update`；产物经 `AssemblyView` 借用，行宇宙稳定帧零走访零重建。
//! 判定面全量参照（`assemble_reference`）保留为对照模式的比对基准，
//! 供 BRP verify 开关逐帧核对（判定线 2）。

use bevy::asset::AssetId;
use bevy::pbr::StandardMaterial;
use bevy::prelude::*;
use std::collections::BTreeSet;

use crate::common::error::VulkanError;
use crate::scene::{
    anchor_level, InstanceLedger, InstanceRow, LodGroups,
};
use crate::render::{
    pack_lod_entry, pack_material_record, pack_model, BindlessTables, ImageCache, InstanceTables,
    LOD_ENTRY_SIZE, LOD_LEVEL_SIZE, LOD_TABLE_STRIDE, MATERIAL_RECORD_SIZE, MAX_LOD_LEVELS,
    MODEL_STRIDE, MeshPool, NO_TEXTURE_INDEX,
};

/// 组装对照账面（7.9 判定线 2 的 BRP 开关对照模式）：开启时帧循环逐帧跑
/// 全量参照走访与增量缓存比对——只进计量，**不驱动绘制**（诊断性回读边界
/// 照 5.2 拍板）。默认关，稳态零成本。
#[derive(Resource, Default)]
pub struct DrawListVerify {
    /// 对照开关（BRP `drawlist_verify_set` 写）。
    pub enabled: bool,
    /// 已比对帧数累计。
    pub frames_compared_total: u64,
    /// 不一致帧数累计（判定线 2 的证据读数，非零 = 增量路径有缺陷）。
    pub mismatches_total: u64,
    /// 最近一次不一致的摘要（行数差/首差位/字段名）。
    pub last_mismatch: Option<String>,
}

/// 可绘判定闭包（7.9）：与原全量走访逐行同判定——实例槽缺 / 锚未驻留 /
/// 材质容器缺席 = None（暂缓）；命中 = (槽号, 材质身份, alpha<1)。
/// 锚解析按 6.5 纯函数（floor_anchor 开启 = 组内保底档，尾档缺席退回基准）。
pub(crate) fn drawability_eval<'a>(
    pool: &'a MeshPool,
    std_materials: &'a Assets<StandardMaterial>,
    lod_groups: &'a LodGroups,
    floor_anchor: bool,
) -> impl FnMut(&InstanceRow) -> Option<(u32, AssetId<StandardMaterial>, bool)> + 'a {
    move |row| {
        let slot = row.instance_slot?;
        let mesh_id = row.mesh.id();
        let anchor_id = match lod_groups.levels_of(mesh_id) {
            Some(levels) => {
                let level = anchor_level(floor_anchor, levels[(MAX_LOD_LEVELS - 1) as usize].is_some());
                levels[level as usize]
                    .as_ref()
                    .map(|handle| handle.id())
                    .unwrap_or(mesh_id)
            }
            None => mesh_id,
        };
        pool.resident(anchor_id)?;
        let material = std_materials.get(&row.material)?;
        let alpha = material.base_color.to_linear().alpha < 1.0;
        Some((slot, row.material.id(), alpha))
    }
}

/// 组装 DrawList（7.9 增量入口）：触发账本组装缓存的重估/重建（判定闭包
/// 注入），产物经 `ledger.assembly_view()` 供帧循环全程只读。账本行宇宙
/// 不变帧零走访零 Vec 重建（缓存容量复用）；变更帧成本 O(重估行)。
///
/// 可绘锚（6.5.2）：`floor_anchor` 开启时，LOD 组行的锚 = 组内保底档（最粗
/// 变体，level MAX-1）驻留——判定闭包与全量路径同源（`drawability_eval`）。
pub(crate) fn assemble_update(
    ledger: &mut InstanceLedger,
    pool: &MeshPool,
    std_materials: &Assets<StandardMaterial>,
    lod_groups: &LodGroups,
    floor_anchor: bool,
) {
    ledger.assembly_update(
        &mut drawability_eval(pool, std_materials, lod_groups, floor_anchor),
        floor_anchor,
        lod_groups.generation(),
    );
}

/// 全量参照走访（7.9 对照模式的真值面）：与旧版 `assemble` 逐行同判定的
/// O(账本行) 枚举——只进比对，不进绘制。返回（槽号列表 Entity 序，去重
/// 材质身份，暂缓行数，alpha<1 行数）。
pub(crate) fn assemble_reference(
    ledger: &InstanceLedger,
    pool: &MeshPool,
    std_materials: &Assets<StandardMaterial>,
    lod_groups: &LodGroups,
    floor_anchor: bool,
) -> (Vec<u32>, BTreeSet<AssetId<StandardMaterial>>, usize, usize) {
    let mut eval = drawability_eval(pool, std_materials, lod_groups, floor_anchor);
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

/// 对照模式逐帧比对（7.9 判定线 2）：全量参照 vs 增量缓存，逐字段核对
///（槽号列表逐行、暂缓数、材质集、alpha<1 计数），不一致进账面（不中断
/// 渲染——诊断面只记账）。
pub(crate) fn verify_assembly(
    ledger: &InstanceLedger,
    pool: &MeshPool,
    std_materials: &Assets<StandardMaterial>,
    lod_groups: &LodGroups,
    floor_anchor: bool,
    pending_rows: usize,
    verify: &mut DrawListVerify,
) {
    let (ref_slots, ref_materials, ref_pending, ref_alpha) =
        assemble_reference(ledger, pool, std_materials, lod_groups, floor_anchor);
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
/// 容器/驻留缓存现读——材质参数变化与贴图槽换指自动次帧生效（4.2 的
/// 材质半边语义由本环吸收，flush 无需任何材质内容通道）。
///
/// 7.9 评估留全量（裁决与实数入施工记录）：全量重写 = 4.2 材质内容变更
/// 通道的本体（增量写会把参数变化冻结在旧记录里，需另开内容变更通道才能
/// 补齐语义）；占比实测 ~4% 帧墙（rings 段 0.93ms/21ms），简单性优先留全量。
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
        // 8.2.1 PBR 扩列供料：三贴图槽缺席写 NO_TEXTURE_INDEX 哨兵（shader 端
        // 分支跳过采样），标量域语义 = Filament gltfio 同款（roughness 记
        // perceptual 域、reflectance 原值，clamp 换算归 shader 端）
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

/// 写在场环（全量重写）：可绘行槽号列表 → 本帧槽的环。5.3 的"逐 draw 循环"
/// 退役通道——GPU 侧 build_indirect 以本列表为行宇宙（退休槽/暂缓行不在
/// 列表 = 自动无命令），写发生条件与帧 UBO 同一条 fence 证据。列表本身是
/// 组装缓存的产物（7.9），本环每帧照写（环按帧槽双缓冲，快照语义不可省）。
fn write_presence_ring(
    tables: &mut InstanceTables,
    slots: &[u32],
    frame: usize,
) -> Result<(), VulkanError> {
    if slots.is_empty() {
        return Ok(()); // 空场：环内容无人读（dispatch 上界 = count = 0）
    }
    let bytes: Vec<u8> = slots.iter().flat_map(|s| s.to_le_bytes()).collect();
    tables.presence_ring(frame).write(0, &bytes)?;
    Ok(())
}

/// 写 LOD 变体表环（6.2.2，增量）：LOD 脏行（换槽/档位资产到货）→ 本帧槽
/// 的表环写该槽各档绘制引脚。level 0 = 本行基准 mesh 槽位（6.5 保底锚开启
/// 时基准降为按需高档，未驻留 = 哨兵保持 0，消费端 rec 回退引脚此时即保底
/// 档引脚——flush 锚腿以保底槽写记录；6.4 语义下基准恒驻留）；level 1.. =
/// 登记组变体引脚，未驻留 = 哨兵 0。同槽的档位环一并写 0（换槽后迟滞状态
/// 从最细档重新收敛——与描述符 init 零填充同语义）。
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

/// 双环写入入口（draw_frame 在 wait_for_slot 之后调用）：模型增量 + 材质
/// 全量 + 在场环全量 + LOD 表增量都落在本帧槽的环上。失败按 Tier② 冒泡
///（环写失败 = host-visible 契约被打破，帧循环无法继续）。
///
/// 借用纪律（7.9）：`ledger` 与 `assembly` 都是账本的共享借用（组装产物住
/// 账本缓存）——奇偶集（model/lod dirty）由调用方在取视图之前先行取走，
/// 本函数对账本零可变访问。
///
/// # Errors
/// 环 buffer 写失败（host-visible 缺席/越界，内部契约矛盾级）。
#[expect(
    clippy::too_many_arguments,
    reason = "环写入的输入即四族账本来源（账本/材质容器/贴图缓存/LOD 组/池）+ 7.9 借用纪律的奇偶集与产物视图，逐项声明与 instance_table 的 hazard 纪律表逐项对账，拆束会藏掉写点清单"
)]
pub(crate) fn write_rings(
    tables: &mut InstanceTables,
    ledger: &InstanceLedger,
    std_materials: &Assets<StandardMaterial>,
    image_cache: &ImageCache,
    model_dirty: &[Entity],
    lod_dirty: &[Entity],
    assembly: &crate::scene::mechanism::AssemblyView<'_>,
    frame: usize,
    lod_groups: &LodGroups,
    pool: &MeshPool,
) -> Result<(), VulkanError> {
    write_model_ring(tables, ledger, model_dirty, frame)?;
    write_material_ring(tables, std_materials, image_cache, assembly.materials, frame)?;
    write_presence_ring(tables, assembly.slots, frame)?;
    write_lod_table_ring(tables, ledger, pool, lod_groups, lod_dirty, frame)?;
    Ok(())
}
