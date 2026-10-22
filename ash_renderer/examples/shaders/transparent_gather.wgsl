// 8.1.3 透明命令组装（compute 单入口）：透明行索引列表 → 20B 命令环（与
// 5.3 IndirectCommand 同布局）。LOD 引脚选择与 build_indirect 同语义
//（选中档哨兵向细档回退、基准兜底）；instanceCount 取剔除标记（透明行同样
// 吃视锥/遮挡标记——标记环对在场槽恒新鲜）。列表内容 = collect 紧凑产物
//（8.1.3 在场序；8.1.4 在其后插 bitonic 排序，本 pass 不感知）。
//
// 接口：set0 = set2（b0 对象表 / b3 剔除标记 / b7 LOD 变体表 / b8 档位环），
// set1 = 透明族小集（b4 索引列表 / b6 命令环 / b7 计数）。
// push（4B）：count = 在场行数上界（guard 用；实际条数以 t_count[0] 为准，
// dispatch 按 CPU 上界走满、超计数的 invocation 直接返回）。

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

struct IndirectCommand {
    index_count: u32,
    instance_count: u32,
    first_index: u32,
    vertex_offset: i32,
    first_instance: u32,
}

const MAX_LOD_LEVELS: u32 = 4u;

@group(0) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(0) @binding(3) var<storage, read> marks: array<u32>;
@group(0) @binding(7) var<storage, read> lod_table: array<LodEntry>;
@group(0) @binding(8) var<storage, read> lod_levels: array<u32>;
@group(1) @binding(4) var<storage, read> t_index_list: array<u32>;
@group(1) @binding(6) var<storage, read_write> t_commands: array<IndirectCommand>;
@group(1) @binding(7) var<storage, read> t_count: array<u32>;

var<immediate> push: PushGather;
struct PushGather {
    count: u32,  // 在场行数上界（dispatch 组数口径）
}

// 与 build_indirect.wgsl 同款引脚选择（选中档哨兵向细档找最近常驻档，
// 全哨兵回退对象记录基准）
fn pick_lod_entry(rec: ObjectRecord, slot: u32) -> LodEntry {
    let level = min(lod_levels[slot], MAX_LOD_LEVELS - 1u);
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

const TRANSPARENT_CAPACITY: u32 = 16384u;

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let count = min(t_count[0], TRANSPARENT_CAPACITY);
    if i >= push.count || i >= count {
        return;
    }
    // 升序数组尾部反向取（8.1.4：最远先画；未排帧仍覆盖全部真实行）
    let slot = t_index_list[count - 1u - i];
    let rec = objects[slot];
    let pins = pick_lod_entry(rec, slot);
    let visible = marks[slot];
    let instance_count = select(visible, 0u, pins.index_count == 0u);
    t_commands[i] = IndirectCommand(
        pins.index_count,
        instance_count,
        pins.first_index,
        bitcast<i32>(pins.vertex_base),
        slot,  // firstInstance = 实例槽号（与 5.3 同语义）
    );
}
