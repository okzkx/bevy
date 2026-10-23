// 5.2 GPU 视锥剔除（compute 单入口）：对对象表逐实例做"世界包围盒 vs 视锥
// 六平面"保守求交，写可见标记 marks[slot]（1 = 可见/相交，0 = 确定在外）。
// 本段只产标记——绘制侧尚不消费（画面与 5.1 基线一致是判定线），消费归 5.3
// indirect。CPU 参照实现 = vulkan/cull.rs 的 frustum_planes + aabb_visible
// （与逐对象 conservative_world_aabb 同款保守语义：相交即可见，宁多画不错杀）。
//
// 接口：
// - set0 b0/b1：对象表（48B ObjectRecord，与 debug_draw.wgsl 同结构）+ 本帧
//   模型环（64B mat4）——与图形管线共用同一 set2 描述符集对象，但 compute
//   管线布局只挂这一个 set，故组号是 0（组号随管线布局走：图形布局是
//   [set0,set1,set2] 所以它叫 set2；compute 布局是 [set2] 所以它叫 set0；
//   首跑 VUID-07988/08600 实抓的订正）。
// - set0 b3：可见标记（read_write storage，u32/实例槽，HOST_VISIBLE——本段
//   消费者是宿主诊断读数，5.3 GPU 消费进来时再评估迁 DEVICE_LOCAL）。
// - push（var<immediate>，112B）：视锥六平面（6×vec4，Gribb-Hartmann 半空间
//   dot(p, plane) ≥ 0 为内，D3D 型 clip volume Z[0,1]——near= row2、far=
//   row3−row2，reverse-Z 不改变半空间集合）+ bound（dispatch 上界=实例槽 bump）。
//   帧级全局量每帧一次 push 是 push constants 的本职（5.1 退役的是逐对象
//   push，不是帧级 push；帧 UBO 128B 已满，不再扩）。
//
// 并发语义：每 invocation 写互不重叠的 marks[slot]，普通 store 无需原子；
// bound 之外的槽不写（内容为两帧前的陈旧值——只被停尸房/free list 在途槽
// 占用，账本行永远不会枚举它们，验证与读数只对在场行）。
// AABB 世界推导 = 8 角点全变换重收紧，与 Rust conservative_world_aabb 同款
// （判定线 1 逐位可比的前提：两侧同用 8 角点，不用列最大缩放的粗化变体）。

struct ObjectRecord {
    first_index: u32,
    index_count: u32,
    vertex_base: u32,
    material_index: u32,
    aabb_min: vec4f,
    aabb_max: vec4f,
}

@group(0) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(0) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(0) @binding(3) var<storage, read_write> marks: array<u32>;
// 缺口标记环（6.6）：本 pass 只负责清零——dispatch 上界 = 实例槽 bump bound
// 覆盖全部槽，build_indirect（只派在场行）随后对缺口槽置位。"cull 清零 +
// build_indirect 置位"两段合起来 = 缺口环每帧全量重写（标记环同款纪律，
// 不在场槽不残留陈旧缺口）。
@group(0) @binding(9) var<storage, read_write> gap_marks: array<u32>;
// 8.3 shadow 域（第二集，组号随管线布局走）：b0 shadow UBO（四 cascade 平面
// + 矩阵族，384B 平面超 push 上限故走 UBO）/ b5 交错标记环（[槽][cascade]，
// 四标记一次 dispatch 内联写）。
@group(1) @binding(0) var<uniform> shadow: ShadowUniforms;
@group(1) @binding(5) var<storage, read_write> shadow_marks: array<u32>;

struct ShadowUniforms {
    cascade_view_proj: array<mat4x4f, 4>,
    planes: array<vec4f, 24>,
    splits: vec4f,
    camera_forward: vec4f,
    camera_pos: vec4f,
    map_size: vec4f,
    texel_world: vec4f,
    inv_span: vec4f,
}

var<immediate> push: PushCull;
struct PushCull {
    // 视锥六平面（world 空间半空间 (a,b,c,d)，dot(p,plane) >= 0 为内侧）。
    planes: array<vec4f, 6>,
    // dispatch 上界 = 实例槽 bump bound（used_object_slots）。
    bound: u32,
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let slot = gid.x;
    if slot >= push.bound {
        return;
    }
    let rec = objects[slot];
    let model = models[slot];
    // —— 局部包围盒 → 世界保守包围盒：8 角点全变换重收紧（与 CPU 参照
    // conservative_world_aabb 同款逐位可比）。mat4 列主序与模型环字节同一约定
    //（debug_draw vs_main 的 model × vec4 同序）。
    var wmin = vec3f(1e30);
    var wmax = vec3f(-1e30);
    for (var x = 0u; x < 2u; x = x + 1u) {
        for (var y = 0u; y < 2u; y = y + 1u) {
            for (var z = 0u; z < 2u; z = z + 1u) {
                let corner = vec3f(
                    select(rec.aabb_min.x, rec.aabb_max.x, x == 1u),
                    select(rec.aabb_min.y, rec.aabb_max.y, y == 1u),
                    select(rec.aabb_min.z, rec.aabb_max.z, z == 1u),
                );
                let p = (model * vec4f(corner, 1.0)).xyz;
                wmin = min(wmin, p);
                wmax = max(wmax, p);
            }
        }
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
    marks[slot] = visible;
    gap_marks[slot] = 0u;
    // —— 8.3 shadow 标记（四 cascade 内联）：同一世界保守盒逐 cascade 求交。
    // 平面来自 shadow UBO group(1)（4×6=24 平面 384B 超 push 上限的落位）。
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
