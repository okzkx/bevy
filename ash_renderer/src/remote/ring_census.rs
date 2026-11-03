//! 环与常驻表内容对账方法族：`ring_census`——GPU 双帧槽的模型环/材质环/在场环/
//! 标记环/indirect 命令环 + 对象表整表读回，对照宿主侧同口径重打包的期望字节。
//! 用于"画面异常而账面正常"的定责：drawlist verify 只对账 CPU 两侧组装，本方法
//! 把对账推进到 GPU 可见内存的内容级，逐条判定错在写侧还是消费端。只读，读数
//! 不回灌绘制决策。

use bevy::{
    asset::Handle,
    prelude::*,
    remote::{BrpError, BrpResult},
};
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::{
    pack_material_record, pack_model, pack_object_record, readback_buffer, BindlessTextureTable,
    ImageCache, InstanceTables, MeshPool, Context, INDIRECT_COMMAND_SIZE, MARK_SIZE,
    MATERIAL_RECORD_SIZE, MODEL_STRIDE, NO_TEXTURE_INDEX, OBJECT_RECORD_SIZE,
};
use crate::scene::InstanceLedger;

use super::{
    params::{internal_err, parse_params_or_default, res, res_mut},
    readback::{bytes_at, f32_at, read_buffer, u32_at, words},
};

/// 64B 材质记录解码（偏移镜像写侧 `MaterialRecordLayout` 的静态断言表）。
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

/// 48B 对象记录解码（偏移镜像写侧 `ObjectRecordLayout` 的静态断言表）。
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

/// `ash_renderer/ring_census`：对象记录 + 各环逐槽对账（只读）。
/// 可选参数 `slot`（u32）限定单实例槽；缺省盘点全部已驻留行。逐行报：
/// - 对象表记录字节 vs 宿主期望（池驻留区间 + 材质槽重打包）；
/// - 两个帧槽的模型环/材质环字节 vs 宿主期望；
/// - 两个帧槽的标记环与 indirect 命令 vs 宿主期望；
/// - 槽号是否在两个帧槽的在场环（本帧参与绘制的槽集合）里。
///
/// 读的是帧槽直址、handler 不等帧；环为 coherent 映射，宿主写完即 CPU 可见，最坏
/// 读到本帧正在写的半新内容——判读以"两帧槽一致地错"为准。对象表与命令环读回
/// 走一次性命令提交，与帧循环同队列串行（低频调用，不在帧循环内）。
///
/// # Errors
/// 环/对象表读失败（设备丢失等）、槽号落在读回段外、行材质无 GPU 槽。材质容器
/// 缺行不报错，跳过并计入 `materials_missing`；mesh 已淘汰的留槽行记命令不一致
/// 并计入 `rows_mesh_not_resident`。
#[expect(
    clippy::too_many_lines,
    reason = "四族读回（对象表/模型环/材质环/在场环）直线展开，便于与写侧环写入/upload 逐段对照"
)]
#[system]
pub(crate) fn ring_census(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    #[derive(serde::Deserialize, Default)]
    struct P {
        #[serde(default)]
        slot: Option<u32>,
    }
    let p: P = parse_params_or_default(params)?;

    // 期望半边：账本行 × 资产容器/池现读，按写侧逐字节同口径重打包。全程只读
    // 借用（账本/容器的共享借用不与环的可变借用并存）
    let std_materials = res::<Assets<StandardMaterial>>(world)?;
    let image_cache = res::<ImageCache>(world)?;
    let tables = res::<InstanceTables>(world)?;
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
    for (entity, row) in res::<InstanceLedger>(world)?.rows() {
        let Some(slot) = row.instance_slot else {
            rows_without_slot += 1;
            continue;
        };
        if p.slot.is_some_and(|s| s != slot) {
            continue;
        }
        let Some(material) = std_materials.get(&row.material) else {
            materials_missing += 1;
            continue; // 与 write_material_ring 同语义：材质未到货的行暂缓，环侧不补写
        };
        let mat_slot = tables.material_slot(row.material.id()).ok_or_else(|| {
            BrpError::internal(format!(
                "行 {entity:?} 材质 {:?} 无 GPU 槽——实例槽不变式破缺",
                row.material.id()
            ))
        })?;
        let color = material.base_color.to_linear();
        let slots = material
            .base_color_texture
            .as_ref()
            .and_then(|handle| image_cache.slots(handle.id()))
            .unwrap_or_else(BindlessTextureTable::fallback_slots);
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

    // 期望对象记录 = 池驻留条目的索引/顶点区间 + 材质槽重打包
    let expected_records: Vec<Option<[u8; 48]>> = {
        let pool = res::<MeshPool>(world)?;
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

    // GPU 半边：对象表整表一次 staging 读回（DEVICE_LOCAL，依赖其 usage 含
    // TRANSFER_SRC）；逐行两个帧槽的模型环/材质环读后即比对；在场环/标记环按已用
    // 槽上界读回
    const FRAMES: usize = 2; // 帧槽数（双缓冲）
    let mut object_bytes = vec![0u8; used_slots as usize * OBJECT_RECORD_SIZE as usize];
    readback_buffer(
        res::<Context>(world)?,
        res::<InstanceTables>(world)?.object_buffer(),
        &mut object_bytes,
    )
    .map_err(internal_err("对象表读回失败"))?;
    let mut rows_out = Vec::with_capacity(expected.len());
    let mut model_mismatches = 0usize;
    let mut material_mismatches = 0usize;
    let mut record_mismatches = 0usize;
    let mut command_mismatches = 0usize;
    let mut rows_mesh_not_resident = 0usize;
    let mut presence_lists: [Vec<u32>; FRAMES] = [Vec::new(), Vec::new()];
    {
        let mut tables = res_mut::<InstanceTables>(world)?;
        for (frame, list) in presence_lists.iter_mut().enumerate() {
            let bytes = read_buffer(tables.presence_ring(frame), used_slots as usize * 4, "在场环读失败")?;
            // 环重写只覆盖在场列表长度，尾段是旧帧残留；过滤到合法槽域后即为本帧列表
            *list = words(&bytes)
                .into_iter()
                .filter(|s| u64::from(*s) < u64::from(used_slots))
                .collect();
        }
    }
    // 标记环双帧槽（合并标记 = 视锥 ∧ 遮挡）+ 遮挡单列 + indirect 命令环（DEVICE_LOCAL，
    // 一次性提交读回；命令序 = 在场环序，只读在场长度段）
    let mut combined_marks: [Vec<u32>; FRAMES] = [Vec::new(), Vec::new()];
    let mut occ_only_marks: [Vec<u32>; FRAMES] = [Vec::new(), Vec::new()];
    let mut commands: [Vec<[u32; 5]>; FRAMES] = [Vec::new(), Vec::new()];
    let indirect_buffers: [ash::vk::Buffer; FRAMES] = {
        let tables = res::<InstanceTables>(world)?;
        [tables.indirect_buffer(0), tables.indirect_buffer(1)]
    };
    for frame in 0..FRAMES {
        {
            let mut tables = res_mut::<InstanceTables>(world)?;
            let mark_len = used_slots as usize * MARK_SIZE as usize;
            combined_marks[frame] = words(&read_buffer(tables.mark_ring(frame), mark_len, "标记环读失败")?);
            occ_only_marks[frame] =
                words(&read_buffer(tables.occ_mark_ring(frame), mark_len, "遮挡标记环读失败")?);
        }
        let count = presence_lists[frame].len();
        let mut bytes = vec![0u8; count * INDIRECT_COMMAND_SIZE as usize];
        readback_buffer(res::<Context>(world)?, indirect_buffers[frame], &mut bytes)
            .map_err(internal_err("indirect 命令环读回失败"))?;
        commands[frame] = words(&bytes)
            .as_chunks::<{ INDIRECT_COMMAND_SIZE as usize / 4 }>()
            .0
            .to_vec();
    }
    for (row, expected_record) in expected.iter().zip(&expected_records) {
        let mut model_match = [false; FRAMES];
        let mut material_match = [false; FRAMES];
        let mut material_actual = [[0u8; 64]; FRAMES];
        let mut tables = res_mut::<InstanceTables>(world)?;
        for frame in 0..FRAMES {
            let mut model_buf = [0u8; 64];
            tables
                .model_ring(frame)
                .read(u64::from(row.slot) * MODEL_STRIDE, &mut model_buf)
                .map_err(internal_err("模型环读失败"))?;
            model_match[frame] = model_buf == row.model_bytes;
            tables
                .material_ring(frame)
                .read(u64::from(row.mat_slot) * MATERIAL_RECORD_SIZE, &mut material_actual[frame])
                .map_err(internal_err("材质环读失败"))?;
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
        // 命令对账：期望 = (index_count, instanceCount = 合并标记, first_index,
        // vertexOffset = vertex_base, firstInstance = 槽号)，区间取自期望对象记录
        //（与 build_indirect 无 LOD 组时的兜底路径同源）；实际 = 命令环中
        // firstInstance == 槽号的那条（按在场序应恰好一条）。
        // 坑：行保留实例槽而 mesh 已被驻留管理器淘汰时期望记录缺席，无期望可比，
        // 两帧槽一律记命令不一致并单独计数。
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
