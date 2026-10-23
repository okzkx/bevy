// Hi-Z 遮挡测试（compute，在视锥剔除之后、indirect 组装之前）：逐实例做
// "AABB 投屏 vs 前帧深度金字塔"保守遮挡判定——被完全遮挡的实例把视锥标记清零
//（marks = 视锥 ∧ 遮挡，build_indirect 消费形状不变，instanceCount=0），遮挡
// 独立标记 occ[slot] 供 A/B 归因读数（视锥剔除量/遮挡剔除量分列）。
//
// 判定语义（与 bevy occlusion_culling.wesl 同构，reverse-Z 各项钉明；CPU 参照
// = src/render/gpu/hiz.rs 的 reduce_tile_reverse_z / occlusion_mip_level 等纯函数）：
// - 8 角点世界推导同 cull.wgsl（与 conservative_world_aabb 逐位可比）；
// - 投屏：clip = view_proj × corner；任一 w ≤ 0（角点在相机平面背后/过近）
//   → 恒判可见（透视除法无意义，保守方向）；px = (ndc·(0.5,−0.5)+0.5)×屏幕，
//   Y 翻转与"正高度视口直出"的屏面约定一致——归约的像素网格同一映射，tile
//   寻址自洽；
// - 物体深度参照 = 8 角点 NDC z 的 MAX。clip z/w 对盒是拟仿射（分子分母皆
//   仿射、分母 w>0 恒正），极值在角点达到——MAX = 最近角点 = 物体深度值的
//   上界，用它判"最近点都在挡板之后"恒保守（bevy 用 view 空间 z 的 max 后
//   转 NDC，单调等价）；z_max > 1（越过近平面）时 occluder ≤ 1 < z_max 恒
//   不剔除，无需 clamp；
// - mip 层 = ceil(log2(AABB 像素最大边))（clamp 进级数）：该层 tile 边长
//   ≥ AABB/2，AABB 每轴至多横跨 2 tile → 左上对齐的 2×2 tile 必覆盖 AABB；
//   tile 可能比 AABB 大（多覆盖的像素只会拉低 min = 更保守）；
// - 遮挡判定：obj_max < min(4 tap) → 剔除（reverse-Z：值小 = 远。物体最近
//   角点的深度值还小于覆盖区域内"最远"的已缓冲表面 = 全身都在一切表面
//   之后）。等号 = 可见（宁多画不错杀）；背景像素深度 0（clear 远平面）
//   → occluder = 0 → 恒不剔除。
//
// 接口：group(0) = set2 家族（b0 对象表 / b1 模型环 / b3 视锥标记
// read_write / b6 遮挡标记 read_write——组号随管线布局走：图形布局里它叫
// set2，遮挡 compute 布局 [set2, hiz] 里它叫 group 0）；
// group(1) b0 = Hi-Z 金字塔全链采样视图（独立小布局，金字塔随 swapchain
// 重建的描述符 churn 隔离在 hiz 模块，不破 set2"init 一次写入"冻结纪律）；
// push（var<immediate>，80B）：view_proj + screen + bound。
//
// 并发语义：每 invocation 读写互不重叠的 marks[slot]/occ[slot]，普通 store
// 无需原子；bound 外的槽不写（内容为陈旧值，只被停尸房/free list 在途槽
// 占用，在场行永不枚举）。fallback 帧（相机跳变/金字塔未就绪）由 Rust 侧
// 整段跳过本 dispatch——b3 保持视锥纯结果，b6 由 host 填全可见。
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
    // —— 8 角点世界推导 + 投屏（坑点：遮挡需要逐角点投影，不能坍缩成世界
    // AABB 再投影——世界盒的投影矩形 ≠ 逐角点投影后的矩形）
    var px_min = vec2f(1e30);
    var px_max = vec2f(-1e30);
    var obj_depth_max = -1e30;
    for (var k = 0u; k < 8u; k = k + 1u) {
        let p = (model * vec4f(aabb_corner(rec, k), 1.0)).xyz;
        let clip = push.view_proj * vec4f(p, 1.0);
        if (clip.w <= 0.0) {
            return 1u; // 透视无意义，恒保守可见
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
    // —— mip 层选择：ceil(log2(像素最大边))（max 1.0 兜底 log2(0)），下限 1、
    // clamp 进级数——本仓金字塔 level 0 不派发（无内容：省一次全量拷贝，
    // bevy 的 mip0=深度本体形状不搬），可查层从 1 起（2px tile）
    let levels = u32(textureNumLevels(hiz));
    let max_px = max(px_max.x - px_min.x, px_max.y - px_min.y);
    let wanted = u32(ceil(log2(max(max_px, 1.0))));
    let level = select(0u, clamp(wanted, 1u, levels - 1u), levels > 1u);
    let level_i = i32(level);
    let dims_l = max(vec2<u32>(textureDimensions(hiz, level_i)), vec2<u32>(1u, 1u));
    // —— 覆盖 AABB 左上角的 2×2 tile（每轴至多跨 2 tile，见头注），坐标
    // clamp 进本级尺寸（textureLoad 越界读 0 会把 tile 假装成"最远"，
    // 方向上保守但读数失真，显式 clamp）
    let scale = vec2<f32>(dims_l) / push.screen;
    let clamp_max = vec2<f32>(dims_l - vec2<u32>(1u));
    let tile = vec2<u32>(clamp(px_min * scale, vec2f(0.0), clamp_max));
    let t1 = min(tile + vec2<u32>(1u, 0u), dims_l - vec2<u32>(1u));
    let t2 = min(vec2<u32>(tile.x, tile.y + 1u), dims_l - vec2<u32>(1u));
    let t3 = min(tile + vec2<u32>(1u), dims_l - vec2<u32>(1u));
    let a = textureLoad(hiz, tile, level_i).x;
    let b = textureLoad(hiz, t1, level_i).x;
    let c = textureLoad(hiz, t2, level_i).x;
    let d = textureLoad(hiz, t3, level_i).x;
    let occluder = min(min(a, b), min(c, d));
    // reverse-Z 遮挡判定（hiz.rs 单测同源钉方向）：等号保守可见
    let occluded = obj_depth_max < occluder;
    return select(1u, 0u, occluded);
}
