// indirect 组装（compute 单入口）：读在场环（可绘行槽号列表）+ 剔除标记 +
// 对象表，逐在场行组装一条 `VkDrawIndexedIndirectCommand`（不可见 →
// instanceCount=0）——绘制调用数不随对象数增长，图形侧一次
// cmd_draw_indexed_indirect 消费。CPU 参照语义 = src/render/gpu/indirect.rs。
//
// 与 Bevy 参照（crates/bevy_pbr/src/render/build_indirect_params.wesl）的取舍：
// 同为"标记/计数 → indirect 参数"的 GPU 组装，我们不做批集与动态槽位分配
//（MULTI_DRAW_INDIRECT_COUNT 路线、atomicAdd 抢槽都不要）——固定槽位，
// 第 i 条命令恒对应在场环第 i 行（Bevy 的 1:1 分支同形状），命令条数 =
// 在场行数 = CPU 在账信息（账本可绘行数，非可见集回读）。
//
// 接口（与 cull.wgsl 同一 set2 描述符集对象，compute 管线布局只挂这一个
// set，故组号是 0——组号随管线布局走）：
// - set0 b0：对象表（48B ObjectRecord）——基准档引脚兜底（LOD 表条目全
//   哨兵时回退到它，可绘行的基准 mesh 恒驻留）。
// - set0 b2：材质环（读 base_color.a——透明行 alpha<1 在本环 instanceCount=0，
//   由透明层 transparent_gather 出命令，不重复绘制）。
// - set0 b3：剔除标记（u32/实例槽；cull.wgsl 写或 A/B 关闭时 host 全可见填充）。
// - set0 b4：在场环（u32 可绘行槽号，draw_frame 全量重写——DrawList 槽位
//   枚举进 GPU 的通道；退休槽/暂缓行不在列表 = 天然无命令，幽灵对象问题
//   在列表源头关闭）。
// - set0 b5：命令环（read_write storage，20B/条 IndirectCommand）。
// - set0 b7：LOD 变体表（48B/槽 = 4 条 LodEntry；index_count=0 = 该档
//   未驻留哨兵——从选档结果向下找最近常驻档，不访问未驻留 LOD）。
// - set0 b8：LOD 档位环（u32/实例槽，lod_select 写 / LOD 关闭 host 填 0）。
// - set0 b9：缺口标记环（read_write，u32/实例槽位图）——本 pass 在查
//   引脚处写缺口证据：选中档条目为哨兵 = "命中且未驻留"，写 `1u << level`；
//   cull 已对本帧全部槽清零，此处只置位（不在场槽保持 0，无陈旧缺口）。
//   宿主按帧奇偶纪律读本帧槽（= 两帧前输出），经驻留管理器合并裁决上传。
// - push（var<immediate>，4B）：count = 在场行数（dispatch 上界）。
//
// 并发语义：每 invocation 写互不重叠的 commands[i]，普通 store 无需原子；
// instanceCount 取 marks[slot]，超出 bound 的槽不派不在场、无陈旧
// 读取（marks 对在场槽恒新鲜：dispatch 上界 = bump bound ≥ 任何在场槽号）。
// #import lod_pick

@group(0) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(0) @binding(2) var<storage, read> materials: array<MaterialRecord>;
@group(0) @binding(3) var<storage, read> marks: array<u32>;
@group(0) @binding(4) var<storage, read> presence: array<u32>;
@group(0) @binding(5) var<storage, read_write> commands: array<IndirectCommand>;
@group(0) @binding(7) var<storage, read> lod_table: array<LodEntry>;
@group(0) @binding(8) var<storage, read> lod_levels: array<u32>;
@group(0) @binding(9) var<storage, read_write> gap_marks: array<u32>;

var<immediate> push: PushBuild;
struct PushBuild {
    // dispatch 上界 = 在场行数（draw_frame 从账本可绘行数取，CPU 在账信息）。
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
    // 缺口证据：选中档条目为哨兵 = 本帧命中且未驻留——写档位位图，
    // 回退（向细档找/rec 兜底）照旧执行，缺口只记录不改变绘制。
    let selected = min(lod_levels[slot], MAX_LOD_LEVELS - 1u);
    let selected_entry = lod_table[slot * MAX_LOD_LEVELS + selected];
    gap_marks[slot] = select(0u, 1u << selected, selected_entry.index_count == 0u);
    let pins = pick_lod_entry(rec, slot, selected);
    let visible = marks[slot];
    // 透明行：alpha<1 在不透明环 instanceCount=0——命令由透明层
    // transparent_gather 出（材质环驱动，GPU 决策）
    let transparent = materials[rec.material_index].base_color.a < 1.0;
    // 引脚全缺（理论上不可达：可绘行基准恒驻留）防御为 0 实例——不画不悬空
    let opaque_count = select(visible, 0u, pins.index_count == 0u);
    let instance_count = select(opaque_count, 0u, transparent);
    commands[i] = IndirectCommand(
        pins.index_count,
        instance_count,           // 不可见 → instanceCount = 0
        pins.first_index,
        bitcast<i32>(pins.vertex_base), // vertexOffset 带符号（池引脚值域恒正）
        slot,                     // firstInstance = 实例槽号：VS 按 @builtin(instance_index)
                                  // 读 set2（instanceCount=1 时 instance_index == firstInstance）
    );
}
