//! DrawList 组装与双环写入（5.1/5.3）：从账本行到**可绘行槽位枚举**，
//! 以及模型环/材质环/在场环的每帧写入。
//!
//! 5.1 分工：本模块是"槽位枚举器"——逐行查实例槽与驻留账本/容器做可绘判定；
//! 5.3 起 `DrawCall`（槽号 + 池引脚）退役，枚举产物只剩可绘行实例槽号列表
//!（`slots`）——它写进在场环（GPU 侧 build_indirect 的行宇宙），录制层的
//! 逐 draw 循环与逐对象 push 一并退役（判定线 1 的走访侧口径：槽位枚举 +
//! 材质环写侧走访是账本在账信息，不是可见集回读；绘制调用与 push 字节恒定）。
//! 行的 GPU 内容由对象表（写一次）+ 模型环/材质环（每帧重写）承载。
//! hazard 纪律：环写入只发生在 `draw_frame` 的 `wait_for_slot` 之后
//!（与本帧 UBO 同一条 fence 证据），详见 `vulkan::instance_table` 模块文档。
//!
//! 行级可绘判定与 4.x 语义一致：实例槽缺（未驻留成表）/mesh 未驻留/材质
//! 容器缺席 = 该行暂缓（Tier①，自愈后消失）；缺贴图 = fallback 白图照画。
//! 暂缓行不进在场环 = 自动无命令（5.3 待决 6 的钉明：暂缓语义由枚举源头
//! 承载，GPU 侧零特判）。6.2 起 LOD 变体表环的脏行增量写也在本模块
//!（write_lod_table_ring——模型环同款两帧奇偶纪律，输入 = 账本 LOD 脏集）。

use bevy::asset::AssetId;
use bevy::pbr::StandardMaterial;
use bevy::prelude::*;
use std::collections::HashSet;

use crate::common::error::VulkanError;
use crate::scene::{anchor_level, InstanceLedger, LodGroups};
use crate::vulkan::{
    pack_lod_entry, pack_material_record, pack_model, BindlessTables, ImageCache, InstanceTables,
    LOD_ENTRY_SIZE, LOD_LEVEL_SIZE, LOD_TABLE_STRIDE, MATERIAL_RECORD_SIZE, MAX_LOD_LEVELS,
    MODEL_STRIDE, MeshPool,
};

/// 一帧 DrawList 组装产物。
pub(crate) struct DrawAssembly {
    /// 可绘行实例槽号列表（在场环全量重写输入；录制层零逐对象消费）。
    pub slots: Vec<u32>,
    /// 暂缓行数（实例槽缺/mesh 未驻留/材质缺席）。
    pub pending_rows: usize,
    /// alpha<1 的行数（不透明覆盖收账日志用）。
    pub alpha_override: usize,
    /// 可绘行的去重材质身份（材质环全量重写的输入）。
    pub materials: Vec<AssetId<StandardMaterial>>,
}

/// 组装 DrawList：账本行 → 实例槽 + 驻留账本/资产容器 → 可绘行槽号列表。
/// 账本读是纯 BTreeMap 走访——零组件扫描零 tick 比较（4.1 语义不变）。
///
/// 可绘锚（6.5.2）：`floor_anchor` 开启时，LOD 组行的锚 = 组内保底档（最粗
/// 变体，level MAX-1）驻留——基准降为按需高档，缺席不暂缓行；组尾档缺席
/// （登记不完整）锚退回基准。关闭 = 6.4 现状（锚恒为基准）。非组行锚恒为
/// 基准（无变体即无保底可迁，6.5 范围钉明）。
pub(crate) fn assemble(
    ledger: &InstanceLedger,
    pool: &MeshPool,
    std_materials: &Assets<StandardMaterial>,
    lod_groups: &LodGroups,
    floor_anchor: bool,
) -> DrawAssembly {
    let mut slots: Vec<u32> = Vec::with_capacity(ledger.len());
    let mut pending_rows = 0usize;
    let mut alpha_override = 0usize;
    let mut material_seen: HashSet<AssetId<StandardMaterial>> = HashSet::new();
    for (_entity, row) in ledger.rows() {
        let Some(slot) = row.instance_slot else {
            pending_rows += 1;
            continue;
        };
        // 池引脚不再需要（命令由 GPU 从对象记录自取）——驻留判定本身仍按
        // MeshSlot 在账（锚未驻留 = 暂缓；锚身份按 6.5 开关解析，纯函数
        // anchor_level 与 tick 需求集/flush 锚腿同源）
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
        if pool.resident(anchor_id).is_none() {
            pending_rows += 1;
            continue;
        }
        let Some(material) = std_materials.get(&row.material) else {
            pending_rows += 1;
            continue;
        };
        let color = material.base_color.to_linear();
        if color.alpha < 1.0 {
            alpha_override += 1;
        }
        material_seen.insert(row.material.id());
        slots.push(slot);
    }
    DrawAssembly {
        slots,
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

/// 写在场环（全量重写）：可绘行槽号列表 → 本帧槽的环。5.3 的"逐 draw 循环"
/// 退役通道——GPU 侧 build_indirect 以本列表为行宇宙（退休槽/暂缓行不在
/// 列表 = 自动无命令），写发生条件与帧 UBO 同一条 fence 证据。
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
    ledger: &mut InstanceLedger,
    pool: &MeshPool,
    lod_groups: &LodGroups,
    frame: usize,
) -> Result<(), VulkanError> {
    for entity in ledger.take_lod_dirty(frame) {
        let Some(row) = ledger.row(entity) else {
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
/// # Errors
/// 环 buffer 写失败（host-visible 缺席/越界，内部契约矛盾级）。
#[expect(
    clippy::too_many_arguments,
    reason = "环写入的输入即四族账本来源（账本/材质容器/贴图缓存/LOD 组/池），逐项声明与 instance_table 的 hazard 纪律表逐项对账，拆束会藏掉写点清单"
)]
pub(crate) fn write_rings(
    tables: &mut InstanceTables,
    ledger: &mut InstanceLedger,
    std_materials: &Assets<StandardMaterial>,
    image_cache: &ImageCache,
    assembly: &DrawAssembly,
    frame: usize,
    lod_groups: &LodGroups,
    pool: &MeshPool,
) -> Result<(), VulkanError> {
    write_model_ring(tables, ledger, frame)?;
    write_material_ring(tables, std_materials, image_cache, &assembly.materials, frame)?;
    write_presence_ring(tables, &assembly.slots, frame)?;
    write_lod_table_ring(tables, ledger, pool, lod_groups, frame)?;
    Ok(())
}
