// 透明命令组装（compute 单入口）：透明行索引列表 → 20B 命令环（与
// build_indirect 的 IndirectCommand 同布局）。LOD 引脚选择与 build_indirect 同语义
//（common/lod_pick.wgsl：选中档哨兵向细档回退、基准兜底）；instanceCount 取剔除
// 标记（透明行同样吃视锥/遮挡标记——标记环对在场槽恒新鲜）。列表内容 =
// collect 紧凑产物经 transparent_sort 升序排好，本 pass 从尾部反向取。
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

const TRANSPARENT_CAPACITY: u32 = 16384u;  // transparent.rs 同名常量

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let count = min(t_count[0], TRANSPARENT_CAPACITY);
    if i >= push.count || i >= count {
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
