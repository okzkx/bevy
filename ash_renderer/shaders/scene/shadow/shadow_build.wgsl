// shadow 命令组装（compute 单入口）：读在场环 + shadow 标记 + LOD 档位环，逐在场行 ×
// 每 cascade 组装一条 VkDrawIndexedIndirectCommand 进 shadow 命令环。CPU 只发矩阵与在场
// 行数上界，绘制决策全在 GPU。
//
// 两条约束：
// - **遮挡标记不参与**：本 pass 只读 shadow 标记（cull 视锥部分产出），Hi-Z 金字塔是主
//   相机深度，对 shadow 视锥不成立。
// - **LOD 再粗一档**：档位 = min(主选档 + 1, MAX-1)（主选档按主相机覆盖算，shadow 视角
//   直接沿用会过细）；从该档向细档找最近常驻条目，全哨兵回退对象记录基准引脚
//   （common/lod_pick.wgsl）。
//
// 接口：
// - set0：实例表（b0 对象表 / b2 材质环 / b4 在场环 / b7 LOD 表 / b8 档位环）。
// - set1：shadow 域（b5 交错标记环 [槽][cascade] / b6..9 四命令环）。
// - push（var<immediate>，4B）：count = 在场行数（dispatch 上界）。
// #import lod_pick

@group(0) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(0) @binding(2) var<storage, read> materials: array<MaterialRecord>;
@group(0) @binding(4) var<storage, read> presence: array<u32>;
@group(0) @binding(7) var<storage, read> lod_table: array<LodEntry>;
@group(0) @binding(8) var<storage, read> lod_levels: array<u32>;
@group(1) @binding(5) var<storage, read> shadow_marks: array<u32>;
@group(1) @binding(6) var<storage, read_write> commands_c0: array<IndirectCommand>;
@group(1) @binding(7) var<storage, read_write> commands_c1: array<IndirectCommand>;
@group(1) @binding(8) var<storage, read_write> commands_c2: array<IndirectCommand>;
@group(1) @binding(9) var<storage, read_write> commands_c3: array<IndirectCommand>;

var<immediate> push: PushBuild;
struct PushBuild {
    count: u32,
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= push.count {
        return;
    }
    let slot = presence[i];
    let rec = objects[slot];
    let main_level = min(lod_levels[slot], MAX_LOD_LEVELS - 1u);
    let pins = pick_lod_entry(rec, slot, min(main_level + 1u, MAX_LOD_LEVELS - 1u));
    var cmd = IndirectCommand(
        pins.index_count,
        0u,
        pins.first_index,
        bitcast<i32>(pins.vertex_base),
        slot,
    );
    // 透明行不进 shadow depth（方向光项只在 opaque 光照累积采样）；引脚全缺（可绘行基准
    // 恒驻留，理论不可达）防御为 0 实例
    let transparent = materials[rec.material_index].base_color.a < 1.0;
    let base = select(1u, 0u, transparent || pins.index_count == 0u);
    cmd.instance_count = base * shadow_marks[slot * 4u + 0u];
    commands_c0[i] = cmd;
    cmd.instance_count = base * shadow_marks[slot * 4u + 1u];
    commands_c1[i] = cmd;
    cmd.instance_count = base * shadow_marks[slot * 4u + 2u];
    commands_c2[i] = cmd;
    cmd.instance_count = base * shadow_marks[slot * 4u + 3u];
    commands_c3[i] = cmd;
}
