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
// - **按 alpha 模式分流两套命令环**：不透明行走 b6..9（无 FS 的 depth-only 管线，early-Z
//   有效），MASK 行在近级联走 b10..13（片元端裁切）、远级联并回 b6..9 做实心阴影。
//   未命中的环写 0 实例，两族命令条数相同。
//
// 接口：
// - set0：实例表（b0 对象表 / b2 材质环 / b4 在场环 / b7 LOD 表 / b8 档位环）。
// - set1：shadow 域（b5 交错标记环 [槽][cascade] / b6..9 不透明命令环 / b10..13 MASK 命令环）。
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
@group(1) @binding(10) var<storage, read_write> mask_commands_c0: array<IndirectCommand>;
@group(1) @binding(11) var<storage, read_write> mask_commands_c1: array<IndirectCommand>;
@group(1) @binding(12) var<storage, read_write> mask_commands_c2: array<IndirectCommand>;
@group(1) @binding(13) var<storage, read_write> mask_commands_c3: array<IndirectCommand>;

var<immediate> push: PushBuild;
struct PushBuild {
    count: u32,
}

// MASK 行进前 N 级联做镂空裁切，更远的级联退回实心阴影（并入不透明环）：远级联里树叶
// 只占几个像素，镂空收益看不见，裁切填充却是 shadow pass 的大头；退实心保住接地阴影。
const MASK_SHADOW_CASCADES: u32 = 1u;

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
    // 透明行不进 shadow depth（方向光项只在 opaque 光照累积采样）；MASK 行进（片元端
    // 裁切）；引脚全缺（可绘行基准恒驻留，理论不可达）防御为 0 实例
    let mat = materials[rec.material_index];
    let base = select(1u, 0u, material_is_transparent(mat) || pins.index_count == 0u);
    let mask = mat.alpha_mode == ALPHA_MODE_MASK;
    let opaque_base = select(0u, base, !mask);
    let mask_base = select(0u, base, mask);
    // c0 恒定走镂空：不透明环收纯不透明行，MASK 环收 MASK 行
    cmd.instance_count = opaque_base * shadow_marks[slot * 4u + 0u];
    commands_c0[i] = cmd;
    cmd.instance_count = mask_base * shadow_marks[slot * 4u + 0u];
    mask_commands_c0[i] = cmd;
    // 远级联：MASK 行并回不透明环（实心阴影），MASK 环写 0——保住接地阴影且不付裁切填充
    let o1 = select(base, opaque_base, MASK_SHADOW_CASCADES > 1u);
    let m1 = select(0u, mask_base, MASK_SHADOW_CASCADES > 1u);
    cmd.instance_count = o1 * shadow_marks[slot * 4u + 1u];
    commands_c1[i] = cmd;
    cmd.instance_count = m1 * shadow_marks[slot * 4u + 1u];
    mask_commands_c1[i] = cmd;
    let o2 = select(base, opaque_base, MASK_SHADOW_CASCADES > 2u);
    let m2 = select(0u, mask_base, MASK_SHADOW_CASCADES > 2u);
    cmd.instance_count = o2 * shadow_marks[slot * 4u + 2u];
    commands_c2[i] = cmd;
    cmd.instance_count = m2 * shadow_marks[slot * 4u + 2u];
    mask_commands_c2[i] = cmd;
    let o3 = select(base, opaque_base, MASK_SHADOW_CASCADES > 3u);
    let m3 = select(0u, mask_base, MASK_SHADOW_CASCADES > 3u);
    cmd.instance_count = o3 * shadow_marks[slot * 4u + 3u];
    commands_c3[i] = cmd;
    cmd.instance_count = m3 * shadow_marks[slot * 4u + 3u];
    mask_commands_c3[i] = cmd;
}
