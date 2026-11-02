// Hi-Z 遮挡测试（compute，在视锥剔除之后、indirect 组装之前）：逐实例做"AABB 投屏 vs
// 前帧深度金字塔"保守判定。被完全遮挡的实例把视锥标记清零（marks = 视锥 ∧ 遮挡，
// instanceCount=0），遮挡独立标记 occ[slot] 只进诊断读数（分列视锥/遮挡剔除量）。
// CPU 参照 = src/render/passes/visibility/hiz.rs 的 occluded_reverse_z /
// occlusion_mip_level 与 occlusion.rs 的 aabb_screen_rect。
// 原理见 .agents/docs/开发步骤/6-高级剔除LOD与流送/6.1-Hi-Z遮挡/Hi-Z遮挡原理：深度金字塔与保守遮挡判定.md。
//
// 判定语义：
// - 投屏：任一角点 w ≤ 0（在相机平面背后/过近）→ 恒判可见（透视除法无意义）；
//   px = (ndc·(0.5,−0.5)+0.5)×屏幕，Y 翻转与归约的像素网格同一映射，tile 寻址自洽。
// - 物体深度 = 8 角点 NDC z 的 MAX（盒上 z/w 的极值在角点）= 深度值上界，用它判"最近点
//   都在挡板之后"恒保守。z_max > 1（越过近平面）时 occluder ≤ 1 < z_max 恒不剔除，无需
//   clamp。
// - mip 层 = ceil(log2(AABB 像素最大边))：该层 tile 边长 ≥ AABB/2，AABB 每轴至多跨 2
//   tile → 左上对齐的 2×2 tile 必覆盖 AABB；tile 比 AABB 大时多覆盖的像素只会拉低 min，
//   更保守。
// - 判定：obj_max < min(4 tap) → 剔除（reverse-Z：值小 = 远）。等号 = 可见；背景像素
//   深度 0 → occluder = 0 → 恒不剔除。
//
// 接口：group(0) = 实例表（b0 对象表 / b1 模型环 / b3 视锥标记 read_write / b6 遮挡标记
// read_write，组号见 cull.wgsl）；group(1) b0 = Hi-Z 金字塔全链采样视图（独立小布局）；
// push（var<immediate>，80B）：view_proj + screen + bound。
//
// 每 invocation 读写互不重叠的 marks[slot]/occ[slot]，普通 store 无需原子；bound 外的
// 槽不写。相机跳变/金字塔未就绪等情况 Rust 侧整段跳过本 dispatch，b3 保持视锥纯结果，
// b6 由 host 填全可见。
// #import instance

@group(0) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(0) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(0) @binding(3) var<storage, read_write> marks: array<u32>;
@group(0) @binding(6) var<storage, read_write> occ: array<u32>;

@group(1) @binding(0) var hiz: texture_2d<f32>;

var<immediate> push: PushOcclusion;
struct PushOcclusion {
    // 本帧相机 view_proj（列主序，与帧 UBO 同一矩阵约定）。
    view_proj: mat4x4f,
    // 屏幕像素尺寸（= 金字塔 level 0 尺寸 = swapchain extent）。
    screen: vec2<f32>,
    // dispatch 上界 = 实例槽 bump bound。
    bound: u32,
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let slot = gid.x;
    if slot >= push.bound {
        return;
    }
    let frustum_visible = marks[slot];
    let occl = occlusion_verdict(slot);
    occ[slot] = occl;
    marks[slot] = frustum_visible & occl;
}

// 单实例保守遮挡判定：1 = 未能证明被遮挡（可见），0 = 全身被挡。
fn occlusion_verdict(slot: u32) -> u32 {
    let rec = objects[slot];
    let model = models[slot];
    // —— 8 角点世界推导 + 投屏。坑：遮挡需要逐角点投影，不能坍缩成世界 AABB 再投影
    // （世界盒的投影矩形 ≠ 逐角点投影后的矩形）
    var px_min = vec2f(1e30);
    var px_max = vec2f(-1e30);
    var obj_depth_max = -1e30;
    for (var k = 0u; k < 8u; k = k + 1u) {
        let p = (model * vec4f(aabb_corner(rec, k), 1.0)).xyz;
        let clip = push.view_proj * vec4f(p, 1.0);
        if (clip.w <= 0.0) {
            return 1u;
        }
        let ndc = clip.xy / clip.w;
        let px = (ndc * vec2f(0.5, -0.5) + vec2f(0.5)) * push.screen;
        px_min = min(px_min, px);
        px_max = max(px_max, px);
        obj_depth_max = max(obj_depth_max, clip.z / clip.w);
    }
    // —— 矩形 clamp 到屏内（离屏部分不渲染，不参与判定）
    px_min = clamp(px_min, vec2f(0.0), push.screen);
    px_max = clamp(px_max, vec2f(0.0), push.screen);
    // —— mip 层选择（max 1.0 兜底 log2(0)）：下限 1，level 0 不派发、无内容，可查层
    // 从 1 起（2px tile）
    let levels = u32(textureNumLevels(hiz));
    let max_px = max(px_max.x - px_min.x, px_max.y - px_min.y);
    let wanted = u32(ceil(log2(max(max_px, 1.0))));
    let level = select(0u, clamp(wanted, 1u, levels - 1u), levels > 1u);
    let level_i = i32(level);
    let dims_l = max(vec2<u32>(textureDimensions(hiz, level_i)), vec2<u32>(1u, 1u));
    // —— 覆盖 AABB 左上角的 2×2 tile（每轴至多跨 2 tile，见头注），坐标 clamp 进本级
    // 尺寸。坑：textureLoad 越界读 0 会把 tile 假装成"最远"，方向上保守但读数失真
    let last = dims_l - vec2<u32>(1u);
    let scale = vec2<f32>(dims_l) / push.screen;
    let tile = vec2<u32>(clamp(px_min * scale, vec2f(0.0), vec2<f32>(last)));
    let t1 = min(tile + vec2<u32>(1u, 0u), last);
    let t2 = min(tile + vec2<u32>(0u, 1u), last);
    let t3 = min(tile + vec2<u32>(1u), last);
    let a = textureLoad(hiz, tile, level_i).x;
    let b = textureLoad(hiz, t1, level_i).x;
    let c = textureLoad(hiz, t2, level_i).x;
    let d = textureLoad(hiz, t3, level_i).x;
    let occluder = min(min(a, b), min(c, d));
    let occluded = obj_depth_max < occluder;
    return select(1u, 0u, occluded);
}
