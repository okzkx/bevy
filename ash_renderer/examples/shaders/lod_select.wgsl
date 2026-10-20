// 6.2.3 GPU LOD 选档（compute 第三 pass，在遮挡测试之后、indirect 组装之前）：
// 逐实例按"AABB 投屏覆盖尺寸 vs 阈值"选 mesh LOD 档位，写档位环 b8——
// build_indirect 按档位从 LOD 变体表 b7 取绘制引脚组装命令（命令三字段
// indexCount/firstIndex/vertexOffset 随档位变，instanceCount 语义不变）。
//
// 判据与迟滞语义（与 vulkan/lod.rs 纯函数逐位同源，判定线 1/2 的 CPU 参照）：
// - 投屏复用 occlusion.wgsl 同款 8 角点推导（一次投影两个消费端的共用点，
//   段施工计划待决 1）——像素矩形 clamp 进屏（离屏部分不渲染），尺寸度量
//   s = clamp 后矩形最大边；
// - 自然档位 raw = floor(log2(threshold / s))：s ≥ 阈值 → 0（最细），
//   s 每减半升一档；clamp 进 [0, MAX_LOD_LEVELS-1]；
// - 迟滞（判定线 2 口径）：向粗切换需 s ≤ 该档自然边界 ×(1−h)（越过阈值带
//   才降档），向细切换即时（靠近立即升档）；prev 读档位环本槽 = 两帧前
//   本帧槽的选档输出（与标记环读数同一条 fence 纪律，2 帧滞后由迟滞带
//   吸收——收敛至多慢两帧，静态场景稳定值不受影响）。
//
// 接口：group(0) = set2 家族（b0 对象表 / b1 模型环 / b8 档位环
// read_write——组号随管线布局走，5.2 首钉同款）；push（var<immediate>，
// 96B）：view_proj + screen + bound + threshold + hysteresis。
//
// 并发语义：每 invocation 读写互不重叠的 lod_levels[slot]，普通 store 无需
// 原子；bound 外的槽不写。LOD 关闭/空场时 Rust 侧整段跳过 dispatch——
// 档位环由 host 填 0（最细档），build_indirect 语义回到 5.3 基准。

struct ObjectRecord {
    first_index: u32,
    index_count: u32,
    vertex_base: u32,
    material_index: u32,
    aabb_min: vec4f,
    aabb_max: vec4f,
}

// 与 instance_table.rs 的 MAX_LOD_LEVELS 同源（表环条目数/步长按它直址）。
const MAX_LOD_LEVELS: u32 = 4u;

@group(0) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(0) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(0) @binding(8) var<storage, read_write> lod_levels: array<u32>;

var<immediate> push: PushLod;
struct PushLod {
    // 本帧相机 view_proj（列主序，与帧 UBO 同一矩阵约定）。
    view_proj: mat4x4f,
    // 屏幕像素尺寸（swapchain extent，与遮挡投屏同一映射）。
    screen: vec2<f32>,
    // dispatch 上界 = 实例槽 bump bound。
    bound: u32,
    // 档位阈值（像素）：s ≥ threshold → 最细档 0。
    threshold: f32,
    // 迟滞系数 h ∈ [0,1)：向粗切换需越过自然边界 ×(1−h)。
    hysteresis: f32,
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let slot = gid.x;
    if slot >= push.bound {
        return;
    }
    let rec = objects[slot];
    let model = models[slot];
    // —— 8 角点世界推导 + 投屏（occlusion.wgsl 同款：逐角点投影，
    //    任一 w ≤ 0 → 恒判最细档 0 = 保守，与遮挡的"恒可见"同方向）
    var px_min = vec2f(1e30);
    var px_max = vec2f(-1e30);
    for (var x = 0u; x < 2u; x = x + 1u) {
        for (var y = 0u; y < 2u; y = y + 1u) {
            for (var z = 0u; z < 2u; z = z + 1u) {
                let corner = vec3f(
                    select(rec.aabb_min.x, rec.aabb_max.x, x == 1u),
                    select(rec.aabb_min.y, rec.aabb_max.y, y == 1u),
                    select(rec.aabb_min.z, rec.aabb_max.z, z == 1u),
                );
                let p = (model * vec4f(corner, 1.0)).xyz;
                let clip = push.view_proj * vec4f(p, 1.0);
                if (clip.w <= 0.0) {
                    lod_levels[slot] = 0u; // 透视无意义，恒最细档（保守）
                    return;
                }
                let ndc = clip.xy / clip.w;
                let px = (ndc * vec2f(0.5, -0.5) + vec2f(0.5)) * push.screen;
                px_min = min(px_min, px);
                px_max = max(px_max, px);
            }
        }
    }
    // —— 矩形 clamp 进屏（与 occlusion.wgsl 同一约定）：离屏部分不渲染，
    //    尺寸度量取 clamp 后覆盖（完全离屏 → 0 → 最粗档，反正不画）
    px_min = clamp(px_min, vec2f(0.0), push.screen);
    px_max = clamp(px_max, vec2f(0.0), push.screen);
    let s = max(px_max.x - px_min.x, px_max.y - px_min.y);
    // —— 自然档位 + 迟滞（lod.rs 纯函数同式）
    let prev = lod_levels[slot];
    let raw = u32(clamp(
        floor(log2(max(push.threshold, 1.0) / max(s, 1.0))),
        0.0,
        f32(MAX_LOD_LEVELS - 1u),
    ));
    var level = raw;
    if (raw > prev) {
        // 变粗（远离）：需低于该档自然边界再留 (1−h) 余量才降档
        let boundary = push.threshold * exp2(-f32(raw));
        level = select(prev, raw, s <= boundary * (1.0 - push.hysteresis));
    } else if (raw < prev) {
        // 变细（靠近）：即时升档（判定线 2 口径）
        level = raw;
    }
    lod_levels[slot] = level;
}
