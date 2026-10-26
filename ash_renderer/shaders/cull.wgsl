// GPU 视锥剔除（compute 单入口）：对对象表逐实例做"世界包围盒 vs 视锥
// 六平面"保守求交，写可见标记 marks[slot]（1 = 可见/相交，0 = 确定在外），
// 由 build_indirect 消费为 instanceCount。CPU 参照实现 = src/render/gpu/cull.rs
// 的 frustum_planes + aabb_visible（与逐对象 conservative_world_aabb 同款保守
// 语义：相交即可见，宁多画不错杀）。
//
// 接口：
// - set0 b0/b1：对象表（48B ObjectRecord）+ 本帧模型环（64B mat4）——与图形管线
//   共用同一 set2 描述符集对象。坑点：组号随管线布局走——图形布局是
//   [set0,set1,set2] 所以它叫 set2，compute 布局只挂这一个 set 所以它叫 set0
//  （写错组号即 VUID-07988/08600）。
// - set0 b3：可见标记（read_write storage，u32/实例槽，HOST_VISIBLE——宿主诊断读数）。
// - set0 b9：缺口标记环（本 pass 只清零，见下）。
// - set1 b0/b5：shadow UBO（四 cascade 平面）/ 交错 shadow 标记环（[槽][cascade]）。
// - push（var<immediate>，112B）：视锥六平面（6×vec4，Gribb-Hartmann 半空间
//   dot(p, plane) ≥ 0 为内，D3D 型 clip volume Z[0,1]——near= row2、far=
//   row3−row2，reverse-Z 不改变半空间集合）+ bound（dispatch 上界=实例槽 bump）
//   + main_cull（主视图剔除启用位，0 = 关：marks 恒写 1——shadow 开启时剔除关
//   仍会 dispatch，不能覆盖成视锥结果）。
//   帧级全局量每帧一次 push 是 push constants 的本职（退役的是逐对象 push）；
//   帧 UBO 不为剔除扩字段。
//
// 并发语义：每 invocation 写互不重叠的 marks[slot]，普通 store 无需原子；
// bound 之外的槽不写（内容为两帧前的陈旧值——只被停尸房/free list 在途槽
// 占用，账本行永远不会枚举它们，验证与读数只对在场行）。
// AABB 世界推导 = 8 角点全变换重收紧，与 Rust conservative_world_aabb 同款
// （逐位可比的前提：两侧同用 8 角点，不用列最大缩放的粗化变体）。
// #import instance
// #import shadow_ubo

@group(0) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(0) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(0) @binding(3) var<storage, read_write> marks: array<u32>;
// 缺口标记环：本 pass 只负责清零——dispatch 上界 = 实例槽 bump bound
// 覆盖全部槽，build_indirect（只派在场行）随后对缺口槽置位。"cull 清零 +
// build_indirect 置位"两段合起来 = 缺口环每帧全量重写（不在场槽不残留陈旧缺口）。
@group(0) @binding(9) var<storage, read_write> gap_marks: array<u32>;
// shadow 域（第二集，组号随管线布局走）：b0 shadow UBO / b5 交错标记环
//（[槽][cascade]，四标记一次 dispatch 内联写）。
@group(1) @binding(0) var<uniform> shadow: ShadowUniforms;
@group(1) @binding(5) var<storage, read_write> shadow_marks: array<u32>;

var<immediate> push: PushCull;
struct PushCull {
    // 视锥六平面（world 空间半空间 (a,b,c,d)，dot(p,plane) >= 0 为内侧）。
    planes: array<vec4f, 6>,
    // dispatch 上界 = 实例槽 bump bound（used_object_slots）。
    bound: u32,
    // 主视图剔除启用位（0 = 关 → marks 全可见；shadow 标记不受影响）。
    main_cull: u32,
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let slot = gid.x;
    if slot >= push.bound {
        return;
    }
    let rec = objects[slot];
    let model = models[slot];
    // —— 局部包围盒 → 世界保守包围盒：8 角点全变换重收紧。mat4 列主序与
    // 模型环字节同一约定（model × vec4）。
    var wmin = vec3f(1e30);
    var wmax = vec3f(-1e30);
    for (var k = 0u; k < 8u; k = k + 1u) {
        let p = (model * vec4f(aabb_corner(rec, k), 1.0)).xyz;
        wmin = min(wmin, p);
        wmax = max(wmax, p);
    }
    // —— 视锥六平面保守求交：p 顶点（法线分量同号的那只角点）落在半空间
    // 外 = 确定在外（可见 = 不被任何平面整盒排除）。法线未归一化不影响
    // 判定（只看符号与相对零点）。
    var visible = 1u;
    for (var p = 0u; p < 6u; p = p + 1u) {
        let pl = push.planes[p];
        let px = select(wmin.x, wmax.x, pl.x >= 0.0);
        let py = select(wmin.y, wmax.y, pl.y >= 0.0);
        let pz = select(wmin.z, wmax.z, pl.z >= 0.0);
        if pl.x * px + pl.y * py + pl.z * pz + pl.w < 0.0 {
            visible = 0u;
        }
    }
    marks[slot] = select(1u, visible, push.main_cull != 0u);
    gap_marks[slot] = 0u;
    // —— shadow 标记（四 cascade 内联）：同一世界保守盒逐 cascade 求交。
    // 平面来自 shadow UBO（4×6=24 平面 384B 超 push 上限）。
    // 两枚语义钉的执行面：这里只做视锥标记——Hi-Z 遮挡不参与 shadow（主相机
    // 深度语义对 shadow 视锥不合法）；LOD 粗化不在标记侧（shadow_build 按
    // 档位环再粗一档取引脚）。
    for (var c = 0u; c < 4u; c = c + 1u) {
        var svis = 1u;
        for (var p = 0u; p < 6u; p = p + 1u) {
            let pl = shadow.planes[c * 6u + p];
            let px = select(wmin.x, wmax.x, pl.x >= 0.0);
            let py = select(wmin.y, wmax.y, pl.y >= 0.0);
            let pz = select(wmin.z, wmax.z, pl.z >= 0.0);
            if pl.x * px + pl.y * py + pl.z * pz + pl.w < 0.0 {
                svis = 0u;
            }
        }
        shadow_marks[slot * 4u + c] = svis;
    }
}
