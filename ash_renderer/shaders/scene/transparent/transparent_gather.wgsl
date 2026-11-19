// 透明命令组装（compute 单入口）：透明行索引列表 → 20B 命令环（与
// build_indirect 的 IndirectCommand 同布局）。LOD 选档与 build_indirect 相同
//（common/lod_pick.wgsl：选中档未驻留就向细档回退，最后用基准 mesh）；instanceCount
// 取剔除标记（透明行同样受视锥/遮挡剔除，在场槽的标记每帧都是新的）。列表 =
// collect 的紧凑产物经 transparent_sort 升序排好，本 pass 从尾部反向取。
//
// 接口：set0 = set2（b0 对象表 / b3 剔除标记 / b7 LOD 变体表 / b8 档位环），
// set1 = 透明命令链集（b0 索引列表 / b2 命令环 / b3 计数）。
// push（4B）：count = 在场行数上界（guard 用；实际条数以 t_count[0] 为准，
// dispatch 按 CPU 上界走满、超计数的 invocation 直接返回）。
// #import lod_pick

@group(0) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(0) @binding(3) var<storage, read> marks: array<u32>;
@group(0) @binding(7) var<storage, read> lod_table: array<LodEntry>;
@group(0) @binding(8) var<storage, read> lod_levels: array<u32>;
@group(1) @binding(0) var<storage, read> t_index_list: array<u32>;
@group(1) @binding(2) var<storage, read_write> t_commands: array<IndirectCommand>;
@group(1) @binding(3) var<storage, read> t_count: array<u32>;

var<immediate> push: PushGather;
struct PushGather {
    count: u32,  // 在场行数上界（dispatch 组数口径）
}

const TRANSPARENT_CAPACITY: u32 = 16384u;  // transparent_targets.rs 同名常量

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let count = min(t_count[0], TRANSPARENT_CAPACITY);
    // 上界双重夹：push.count 是在场行数；命令环只按 TRANSPARENT_CAPACITY 分配。
    if i >= push.count || i >= TRANSPARENT_CAPACITY {
        return;
    }
    // 命令环按在场行数消费，但本帧只有 [0, count) 有条目。多出来的下标必须写成
    // 零命令：否则它们保留上一轮（同帧槽两帧前）的命令，透明行数下降时会重放
    // 旧对象（幽灵绘制）；首用还会执行未初始化显存里的任意命令。
    if i >= count {
        t_commands[i] = IndirectCommand(0u, 0u, 0u, 0i, 0u);
        return;
    }
    // 固定容量全排序后，-inf padding 位于前缀，真实键在尾部；从数组末端反向
    // 读取 count 个真实行（最远先画）。
    let slot = t_index_list[TRANSPARENT_CAPACITY - 1u - i];
    let rec = objects[slot];
    let pins = pick_lod_entry(rec, slot, min(lod_levels[slot], MAX_LOD_LEVELS - 1u));
    let visible = marks[slot];
    let instance_count = select(visible, 0u, pins.index_count == 0u);
    t_commands[i] = IndirectCommand(
        pins.index_count,
        instance_count,
        pins.first_index,
        bitcast<i32>(pins.vertex_base),
        slot,  // firstInstance = 实例槽号（与 build_indirect 同语义）
    );
}
