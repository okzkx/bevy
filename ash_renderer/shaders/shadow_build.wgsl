// 8.3 shadow 命令组装（compute 单入口）：读在场环 + shadow 标记 + LOD 档位环，
// 逐在场行 × 每 cascade 组装一条 VkDrawIndexedIndirectCommand 进 shadow 命令
// 环——shadow draw 的绘制决策全在 GPU（与 5.3 build_indirect 同构，红线不碰：
// CPU 只发矩阵与在场行数上界）。
//
// 两枚语义钉的执行面（段施工计划 §0）：
// - **遮挡标记不参与**：本 pass 只读 shadow 标记（cull 视锥半边产出），Hi-Z
//   金字塔是主相机深度语义，对 shadow 视锥不合法。
// - **LOD 再粗一档**：档位 = min(主选档 + 1, MAX-1)（6.2 档位是主相机覆盖
//   语义，shadow 视角直接沿用会过细）；从该档向细档方向找最近常驻条目
//   （哨兵回退与 5.3 同款），全哨兵回退对象记录基准引脚。
//
// 接口：
// - set0：实例表（b0 对象表 / b2 材质环 / b4 在场环 / b7 LOD 表 / b8 档位环）。
// - set1：shadow 域（b5 交错标记环 [槽][cascade] / b6..9 四命令环）。
// - push（var<immediate>，4B）：count = 在场行数（dispatch 上界）。

struct ObjectRecord {
    first_index: u32,
    index_count: u32,
    vertex_base: u32,
    material_index: u32,
    aabb_min: vec4f,
    aabb_max: vec4f,
}

struct LodEntry {
    first_index: u32,
    index_count: u32,
    vertex_base: u32,
}

struct MaterialRecord {
    base_color: vec4f,
    metallic: f32,
    perceptual_roughness: f32,
    reflectance: f32,
    _pad0: u32,
    tex_base_color: u32,
    tex_normal: u32,
    tex_metallic_roughness: u32,
    tex_occlusion: u32,
    sampler_index: u32,
    _pad1: u32,
    _pad2: u32,
    _pad3: u32,
}

struct IndirectCommand {
    index_count: u32,
    instance_count: u32,
    first_index: u32,
    vertex_offset: i32,
    first_instance: u32,
}

const MAX_LOD_LEVELS: u32 = 4u;

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

// 从"主档位 + 1"向细档方向找最近常驻条目（与 build_indirect 的 pick 同款
// 回退，起点再粗一档）。
fn pick_shadow_entry(rec: ObjectRecord, slot: u32) -> LodEntry {
    let main_level = min(lod_levels[slot], MAX_LOD_LEVELS - 1u);
    let level = min(main_level + 1u, MAX_LOD_LEVELS - 1u);
    var chosen = LodEntry(0u, 0u, 0u);
    var l = level;
    loop {
        let e = lod_table[slot * MAX_LOD_LEVELS + l];
        if (e.index_count != 0u) {
            chosen = e;
            break;
        }
        if (l == 0u) {
            break;
        }
        l = l - 1u;
    }
    if (chosen.index_count == 0u) {
        chosen = LodEntry(rec.first_index, rec.index_count, rec.vertex_base);
    }
    return chosen;
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= push.count {
        return;
    }
    let slot = presence[i];
    let rec = objects[slot];
    let pins = pick_shadow_entry(rec, slot);
    let transparent = materials[rec.material_index].base_color.a < 1.0;
    var cmd = IndirectCommand(
        pins.index_count,
        0u,
        pins.first_index,
        bitcast<i32>(pins.vertex_base),
        slot,
    );
    if (pins.index_count == 0u) {
        // 引脚全缺防御（可绘行基准恒驻留，理论不可达）——四环全零实例
        commands_c0[i] = cmd;
        commands_c1[i] = cmd;
        commands_c2[i] = cmd;
        commands_c3[i] = cmd;
        return;
    }
    // 透明行不进 shadow depth（方向光项只在 opaque 光照累积采样）
    let base = select(1u, 0u, transparent);
    cmd.instance_count = base * shadow_marks[slot * 4u + 0u];
    commands_c0[i] = cmd;
    cmd.instance_count = base * shadow_marks[slot * 4u + 1u];
    commands_c1[i] = cmd;
    cmd.instance_count = base * shadow_marks[slot * 4u + 2u];
    commands_c2[i] = cmd;
    cmd.instance_count = base * shadow_marks[slot * 4u + 3u];
    commands_c3[i] = cmd;
}
