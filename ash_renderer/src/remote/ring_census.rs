//! 环与常驻表内容对账读数（定责仪器）：`ring_census`——GPU 双帧槽的
//! 模型环/材质环/在场环逐槽字节 + 对象表整表 staging 读回，对照宿主侧同
//! 口径重打包的期望字节。补"画面异常而账面正常"的盲区：drawlist verify
//! 只对账 CPU 两侧组装，本方法把对账推进到 GPU 可见内存的内容级——
//! 三条证据（对象记录/模型环/材质环）逐条定责到写侧还是消费端。
//!
//! 红线照 perf 族：只读账面与 GPU 内容，读数不回灌绘制决策。

use bevy::{asset::Handle, prelude::*};
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::{
    pack_material_record, pack_model, pack_object_record, readback_buffer, BindlessTables,
    ImageCache, InstanceTables, MeshPool, Context, INDIRECT_COMMAND_SIZE, MARK_SIZE,
    MATERIAL_RECORD_SIZE, MODEL_STRIDE, NO_TEXTURE_INDEX, OBJECT_RECORD_SIZE,
};
use crate::scene::mechanism::InstanceLedger;

use super::{
    params::parse_params_or_default,
    readback::{bytes_at, f32_at, u32_at, words},
    BrpError, BrpResult,
};

/// 64B 材质记录解码（`MaterialRecordLayout` 偏移镜像——写侧静态断言表的
/// 只读消费侧，两处同源对账）。
fn decode_material_record(bytes: &[u8]) -> BrpResult {
    Ok(json!({
        "albedo": [f32_at(bytes, 0)?, f32_at(bytes, 4)?, f32_at(bytes, 8)?, f32_at(bytes, 12)?],
        "metallic": f32_at(bytes, 16)?,
        "roughness": f32_at(bytes, 20)?,
        "reflectance": f32_at(bytes, 24)?,
        "tex": [u32_at(bytes, 32)?, u32_at(bytes, 36)?, u32_at(bytes, 40)?, u32_at(bytes, 44)?],
        "sampler": u32_at(bytes, 48)?,
    }))
}

/// 48B 对象记录解码（`ObjectRecordLayout` 偏移镜像，同上对账纪律）。
fn decode_object_record(bytes: &[u8]) -> BrpResult {
    Ok(json!({
        "first_index": u32_at(bytes, 0)?,
        "index_count": u32_at(bytes, 4)?,
        "vertex_base": u32_at(bytes, 8)?,
        "material_index": u32_at(bytes, 12)?,
        "aabb_min": [f32_at(bytes, 16)?, f32_at(bytes, 20)?, f32_at(bytes, 24)?],
        "aabb_max": [f32_at(bytes, 32)?, f32_at(bytes, 36)?, f32_at(bytes, 40)?],
    }))
}

/// `ash_renderer/ring_census`：对象记录 + 模型环/材质环/在场环逐槽对账（只读）。
/// 可选参数 `slot`（u32）限定单实例槽；缺省盘点全部已驻留行。逐行报：
/// - 对象表记录（DEVICE_LOCAL，一次性 staging 整表读回）字节 vs 宿主期望
///   （池驻留引脚 + 材质槽重打包）；
/// - 两个帧槽的模型环/材质环字节 vs 宿主期望；
/// - 槽号是否在两个帧槽的在场环（draw 宇宙）里。
///
/// 读的是帧槽直址（与 cull_stats 同一"handler 不等帧"纪律；环为 coherent
/// 映射，宿主写完即 CPU 可见，最坏读到本帧正在写的半新内容——判读以
/// "两帧槽一致地错"为准）。对象表读回走 hiz 同款一次性命令提交，与帧循环
/// 同队列串行（低频 BRP，不在帧循环内）。
///
/// # Errors
/// 环/对象表读失败（设备丢失等）或槽号落在读回段外；材质容器缺行按暂缓
/// 语义跳过并记账；mesh 已淘汰的留槽行记命令不一致并计入 `rows_mesh_not_resident`。
#[expect(
    clippy::too_many_lines,
    reason = "定责仪器的四族读回（对象表/模型环/材质环/在场环）直线展开，分拆会藏掉与写侧 write_rings/upload 的逐段对账"
)]
#[system]
pub(crate) fn ring_census(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    #[derive(serde::Deserialize, Default)]
    struct P {
        #[serde(default)]
        slot: Option<u32>,
    }
    let p: P = parse_params_or_default(params)?;

    // —— 期望半边：账本行 × 资产容器/池现读，重打包与写侧逐字节同口径的
    // 期望值。全程只读借用（账本/容器的共享借用不与环的可变借用并存，
    // cull_stats 同款纪律）。
    let std_materials = world.resource::<Assets<StandardMaterial>>();
    let image_cache = world.resource::<ImageCache>();
    let tables = world.resource::<InstanceTables>();
    struct RowExpect {
        entity: Entity,
        slot: u32,
        mat_slot: u32,
        mesh_id: AssetId<Mesh>,
        model_bytes: [u8; 64],
        material_bytes: [u8; 64],
    }
    let mut expected: Vec<RowExpect> = Vec::new();
    let mut rows_without_slot = 0usize;
    let mut materials_missing = 0usize;
    for (entity, row) in world.resource::<InstanceLedger>().rows() {
        let Some(slot) = row.instance_slot else {
            rows_without_slot += 1;
            continue;
        };
        if p.slot.is_some_and(|s| s != slot) {
            continue;
        }
        let Some(material) = std_materials.get(&row.material) else {
            materials_missing += 1;
            continue; // 行级暂缓语义同 write_material_ring：环侧不补写
        };
        let Some(mat_slot) = tables.material_slot(row.material.id()) else {
            return Err(BrpError::internal(format!(
                "行 {entity:?} 材质 {:?} 无 GPU 槽——实例槽不变式破缺",
                row.material.id()
            )));
        };
        let color = material.base_color.to_linear();
        let slots = material
            .base_color_texture
            .as_ref()
            .and_then(|handle| image_cache.slots(handle.id()))
            .unwrap_or_else(BindlessTables::fallback_slots);
        let optional_slot = |texture: Option<&Handle<bevy::image::Image>>| {
            texture
                .and_then(|handle| image_cache.slots(handle.id()))
                .map(|slots| slots.texture)
                .unwrap_or(NO_TEXTURE_INDEX)
        };
        expected.push(RowExpect {
            entity,
            slot,
            mat_slot,
            mesh_id: row.mesh.id(),
            model_bytes: pack_model(&row.model),
            material_bytes: pack_material_record(
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
            ),
        });
    }
    let used_slots = tables.used_object_slots();

    // —— 对象表整表读回（一次 staging 拷贝；TRANSFER_SRC 已在 DeviceStorage
    // usage 冻结）。期望记录 = 池驻留条目引脚 + 材质槽重打包。
    let expected_records: Vec<Option<[u8; 48]>> = {
        let pool = world.resource::<MeshPool>();
        expected
            .iter()
            .map(|row| {
                pool.resident(row.mesh_id).map(|mesh_slot| {
                    let (min, max) = mesh_slot.local_aabb;
                    pack_object_record(
                        mesh_slot.first_index(),
                        mesh_slot.index_count,
                        mesh_slot.vertex_base(),
                        row.mat_slot,
                        min,
                        max,
                    )
                })
            })
            .collect()
    };

    // —— GPU 半边：对象表整表 + 逐行两个帧槽的模型环/材质环，读后即比对
    //（不复用跨行缓冲）；在场环按已用槽上界读回解析。
    const FRAMES: usize = 2;
    let object_bytes = {
        let ctx = world.resource::<Context>();
        let tables = world.resource::<InstanceTables>();
        let mut bytes = vec![0u8; used_slots as usize * OBJECT_RECORD_SIZE as usize];
        readback_buffer(ctx, tables.object_buffer(), &mut bytes)
            .map_err(|e| BrpError::internal(format!("对象表读回失败: {e}")))?;
        bytes
    };
    let mut rows_out = Vec::with_capacity(expected.len());
    let mut model_mismatches = 0usize;
    let mut material_mismatches = 0usize;
    let mut record_mismatches = 0usize;
    let mut command_mismatches = 0usize;
    let mut rows_mesh_not_resident = 0usize;
    let mut presence_lists: [Vec<u32>; FRAMES] = [Vec::new(), Vec::new()];
    {
        let mut tables = world.resource_mut::<InstanceTables>();
        for (frame, list) in presence_lists.iter_mut().enumerate() {
            let mut bytes = vec![0u8; used_slots as usize * 4];
            tables
                .presence_ring(frame)
                .read(0, &mut bytes)
                .map_err(|e| BrpError::internal(format!("在场环读失败: {e}")))?;
            // 旧帧遗留的越界尾值不入表（环全量重写只覆盖在场列表长度，
            // 尾段是上帧残留，过滤到合法槽域后即为本帧列表）。
            *list = words(&bytes)
                .into_iter()
                .filter(|s| u64::from(*s) < u64::from(used_slots))
                .collect();
        }
    }
    // —— 标记环双帧槽（b3 = 视锥 ∧ 遮挡合并）+ 遮挡分列 + indirect
    // 命令环整段读回（DEVICE_LOCAL，hiz readback 同款一次性提交；命令序 =
    // 在场环序，只读在场长度段）。
    let mut combined_marks: [Vec<u32>; FRAMES] = [Vec::new(), Vec::new()];
    let mut occ_only_marks: [Vec<u32>; FRAMES] = [Vec::new(), Vec::new()];
    let mut commands: [Vec<[u32; 5]>; FRAMES] = [Vec::new(), Vec::new()];
    let indirect_buffers: [ash::vk::Buffer; FRAMES] = {
        let tables = world.resource::<InstanceTables>();
        [tables.indirect_buffer(0), tables.indirect_buffer(1)]
    };
    for frame in 0..FRAMES {
        {
            let mut tables = world.resource_mut::<InstanceTables>();
            for (combined, out) in [
                (true, &mut combined_marks[frame]),
                (false, &mut occ_only_marks[frame]),
            ] {
                let mut bytes = vec![0u8; used_slots as usize * MARK_SIZE as usize];
                if combined {
                    tables
                        .mark_ring(frame)
                        .read(0, &mut bytes)
                        .map_err(|e| BrpError::internal(format!("标记环读失败: {e}")))?;
                } else {
                    tables
                        .occ_mark_ring(frame)
                        .read(0, &mut bytes)
                        .map_err(|e| BrpError::internal(format!("遮挡标记环读失败: {e}")))?;
                }
                *out = words(&bytes);
            }
        }
        let count = presence_lists[frame].len();
        let mut bytes = vec![0u8; count * INDIRECT_COMMAND_SIZE as usize];
        {
            let ctx = world.resource::<Context>();
            readback_buffer(ctx, indirect_buffers[frame], &mut bytes)
                .map_err(|e| BrpError::internal(format!("indirect 命令环读回失败: {e}")))?;
        }
        commands[frame] = words(&bytes)
            .as_chunks::<{ INDIRECT_COMMAND_SIZE as usize / 4 }>()
            .0
            .to_vec();
    }
    for (row, expected_record) in expected.iter().zip(&expected_records) {
        let mut model_match = [false; FRAMES];
        let mut material_match = [false; FRAMES];
        let mut material_actual = [[0u8; 64]; FRAMES];
        let mut tables = world.resource_mut::<InstanceTables>();
        for frame in 0..FRAMES {
            let mut model_buf = [0u8; 64];
            tables
                .model_ring(frame)
                .read(u64::from(row.slot) * MODEL_STRIDE, &mut model_buf)
                .map_err(|e| BrpError::internal(format!("模型环读失败: {e}")))?;
            model_match[frame] = model_buf == row.model_bytes;
            tables
                .material_ring(frame)
                .read(u64::from(row.mat_slot) * MATERIAL_RECORD_SIZE, &mut material_actual[frame])
                .map_err(|e| BrpError::internal(format!("材质环读失败: {e}")))?;
            material_match[frame] = material_actual[frame] == row.material_bytes;
        }
        if !model_match.iter().all(|m| *m) {
            model_mismatches += 1;
        }
        if !material_match.iter().all(|m| *m) {
            material_mismatches += 1;
        }
        let rec = bytes_at(
            &object_bytes,
            row.slot as usize * OBJECT_RECORD_SIZE as usize,
            OBJECT_RECORD_SIZE as usize,
        )?;
        let record_match = expected_record
            .as_ref()
            .is_some_and(|exp| exp.as_slice() == rec);
        if !record_match {
            record_mismatches += 1;
        }
        // —— 命令对账：期望 = (池引脚 index_count, instanceCount=合并标记,
        // first_index, vertexOffset=vertex_base, firstInstance=槽号)；实际 =
        // indirect 命令环中 firstInstance==槽 的那条（按在场序应恰好一条）。
        // 期望引脚取自期望记录（池驻留 + 材质槽的宿主重打包），与
        // build_indirect 的 pick_lod_entry 兜底路径同源（无 LOD 组 =
        // 恒走 rec 引脚）。坑：行保留实例槽而 mesh 已被驻留管理器淘汰时
        // 期望记录缺席——无期望可比，两帧槽一律记命令不一致并单独计数。
        let exp_pins = match expected_record {
            Some(exp) => Some((u32_at(exp, 0)?, u32_at(exp, 4)?, u32_at(exp, 8)?)),
            None => {
                rows_mesh_not_resident += 1;
                None
            }
        };
        let mut cmd_match = [false; FRAMES];
        let mut cmd_actual: [[u32; 5]; FRAMES] = [[0; 5]; FRAMES];
        for frame in 0..FRAMES {
            let mark = combined_marks[frame]
                .get(row.slot as usize)
                .copied()
                .unwrap_or(0);
            let found = commands[frame]
                .iter()
                .find(|c| c[4] == row.slot)
                .copied()
                .unwrap_or([0; 5]);
            cmd_match[frame] = exp_pins.is_some_and(|(exp_first, exp_count, exp_vbase)| {
                found[0] == exp_count
                    && found[1] == mark
                    && found[2] == exp_first
                    && found[3] == exp_vbase
                    && found[4] == row.slot
            });
            cmd_actual[frame] = found;
        }
        if !cmd_match.iter().all(|m| *m) {
            command_mismatches += 1;
        }
        rows_out.push(json!({
            "entity": format!("{:?}", row.entity),
            "slot": row.slot,
            "material_slot": row.mat_slot,
            "object_record_match": record_match,
            "object_record_actual": decode_object_record(rec)?,
            "model_ring_match": model_match,
            "material_ring_match": material_match,
            "material_ring_actual": decode_material_record(&material_actual[0])?,
            "mark_combined": combined_marks.iter().map(|l| l.get(row.slot as usize).copied().unwrap_or(0)).collect::<Vec<_>>(),
            "occ_mark": occ_only_marks.iter().map(|l| l.get(row.slot as usize).copied().unwrap_or(0)).collect::<Vec<_>>(),
            "indirect_cmd_match": cmd_match,
            "indirect_cmd_actual": cmd_actual.iter().map(|c| json!({
                "index_count": c[0], "instance_count": c[1], "first_index": c[2],
                "vertex_offset": c[3], "first_instance": c[4],
            })).collect::<Vec<_>>(),
            "in_presence": presence_lists.iter().map(|l| l.contains(&row.slot)).collect::<Vec<_>>(),
        }));
    }
    Ok(json!({
        "rows": rows_out,
        "object_record_all_match": record_mismatches == 0,
        "model_all_match": model_mismatches == 0,
        "material_all_match": material_mismatches == 0,
        "indirect_cmd_all_match": command_mismatches == 0,
        "object_record_mismatch_rows": record_mismatches,
        "model_mismatch_rows": model_mismatches,
        "material_mismatch_rows": material_mismatches,
        "indirect_cmd_mismatch_rows": command_mismatches,
        "rows_without_slot": rows_without_slot,
        "rows_mesh_not_resident": rows_mesh_not_resident,
        "materials_missing": materials_missing,
        "note": "读数 = 帧槽直址快照；判读以两帧槽一致地错为准",
    }))
}
