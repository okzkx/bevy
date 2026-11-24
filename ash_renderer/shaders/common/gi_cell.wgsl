// 9.5 表面缓存格子系统公共片段：Volume/cell 换算、深度重建、调试视图调色。
// 只放纯函数与跨端结构定义，不声明任何 @group/@binding；播种（patch_alloc）、
// 回收三件（scrolling/eviction/defrag）与调试视图（patch_view）共用同一份算术。
//
// 口径（见 9.5 施工计划 §0.1、§0.5 与讲解《patch 池与格子系统讲解》）：
// - 一条 patch 的身份 = (级联, 世界体素, 方向桶)；世界体素 → 存储槽的映射与相机位置无关
//   （级联偏移在代数上被消掉，见 volume_to_storage 注释）。
// - 级联归属用球判据（距离 < voxel·(res/2−0.5)），存储是立方体，角落 cell 判出界不分配。
// - 方向桶复用 octahedral.wgsl 的 4×4 八面体映射（16 桶），不另写一份。
// #import octahedral

// patch 池空槽哨兵（cell→patch 与 patch→cell 双向都用它）。
const INVALID_PATCH: u32 = 0xFFFFFFFFu;
// 方向桶网格边长与每 cell 桶数（与 octahedral 的 res 参数、宿主常量同值）。
const BUCKET_RESOLUTION: u32 = 4u;
const BUCKETS_PER_CELL: u32 = BUCKET_RESOLUTION * BUCKET_RESOLUTION;
// 级联数上限（GI UBO 的级联偏移数组长度）。
const MAX_CASCADES: u32 = 8u;
// 深度不连续剔除门限：窗口深度相对差（reverse-Z 下窗口深度近似线性于 1/z，相对差
// 即视深相对差）；前景/背景交界处的差分法线由它剔除。
const DEPTH_EDGE_RELATIVE: f32 = 0.05;
// 判定线用的位置偏离阈值（体素倍数，离面垂直距离）：视图 7 以此为绿/黄/红分界。
const PATCH_DEVIATION_GREEN: f32 = 0.1;
const PATCH_DEVIATION_RED: f32 = 0.5;

// ---- 跨端记录结构（Rust 镜像在 src/render/resident/tables/surface_cache.rs）----

// patch 几何：位置 + 平面法线（w 未用）。
struct PatchGeometry {
    position: vec4f,
    normal: vec4f,
}

// patch 辐照度：RGBL1 四系数（l0 / l1x / l1y / l1z，分量序 x/y/z，与 9.3 一致）。
struct PatchIrradiance {
    l0: vec4f,
    l1x: vec4f,
    l1y: vec4f,
    l1z: vec4f,
}

// patch 统计：均值/方差 + 状态/请求/更新计数/心跳（48B，见施工记录的口径说明）。
struct PatchStatistics {
    mean: vec4f,
    variance: vec4f,
    // state 低 8 位 = rank（9.5 恒 0；9.6 的 bounce 请求会抬到 1）。
    state: u32,
    // 9.6 的分配请求状态位（9.5 恒 0）。
    request: u32,
    // 已更新次数（9.5 恒 0，种子未参与时域混合）。
    update_count: u32,
    // 心跳帧号低 16 位：淘汰距离按模 65536 的最短距算。
    heartbeat: u32,
}

// 每帧 GI 参数（宿主每帧整份写；304B，偏移见 surface_cache.rs 的静态断言表）。
struct GiUniforms {
    // 上一帧 view_proj（9.6 的种子重投影用；9.5 只写入不使用）。
    prev_view_proj: mat4x4f,
    // xyz = Volume 焦点（= 主相机世界位置）。
    target_pos: vec4f,
    // x = 最细级体素边长（米）；y = res；z = 级联数；w = patch 池容量。
    volume: vec4f,
    // 各级整数体素偏移（xyz；w 留用）。
    cascade_offsets: array<vec4i, MAX_CASCADES>,
    // x = 本帧整理的波数；y = 法线来源（0 平面 / 1 着色）；z = 播种开关；w = 保留。
    ring: vec4u,
    // x = 帧号；y = 淘汰年龄门限（帧）；z = bounce 开关；w = updateMax（9.6 用）。
    frame: vec4u,
    // 9.6 估计参数（u32 组）：x = 样本档；y = 预热倍率；z = 多跳开关；w = 光源清单长度。
    estimation: vec4u,
    // 9.6 估计参数（f32 组）：x = 短时滞后；y = 自发光强度倍数；zw 保留。
    estimation_f: vec4f,
    // 9.6.2 光源侧清单：x = 光源紧凑表活灯数；y = 本帧清单条数；zw 保留。
    estimation_l: vec4u,
}

// ---- 整数与体素算术 ----

// 正模：WGSL 的 % 对负输入保留符号，而存储槽要求落在 [0, m)。单测在 CPU 镜像里钉负输入。
fn pos_mod(a: i32, m: i32) -> i32 {
    let r = a % m;
    return select(r + m, r, r >= 0);
}

// 第 cascade 级体素边长：v_i = voxel_min · 2^i（voxel_min = Size/(Res·2^(C−1))）。
fn voxel_size(voxel_min: f32, cascade: u32) -> f32 {
    return voxel_min * exp2(f32(cascade));
}

// 球判据：位置落在该级内 = 到 Volume 中心距离 < voxel·(res/2 − 0.5)。
fn is_inside_cascade(center: vec3f, p: vec3f, voxel: f32, res: u32) -> bool {
    let radius = voxel * (f32(res) * 0.5 - 0.5);
    let d = p - center;
    return dot(d, d) < radius * radius;
}

// 从最细级起找第一个满足球判据的级联；都不满足返回 INVALID_PATCH。
fn resolve_cascade(center: vec3f, p: vec3f, voxel_min: f32, res: u32, cascades: u32) -> u32 {
    for (var i = 0u; i < cascades; i = i + 1u) {
        if (is_inside_cascade(center, p, voxel_size(voxel_min, i), res)) {
            return i;
        }
    }
    return INVALID_PATCH;
}

// 世界位置 → 体积坐标（clamp 到 [0, res)）：pv = int(p/v − offset + res/2)。
fn volume_position(p: vec3f, voxel: f32, offset: vec3i, res: i32) -> vec3i {
    let v = vec3i(floor(p / voxel)) - offset + vec3i(res / 2);
    return clamp(v, vec3i(0), vec3i(res - 1));
}

// 体积坐标 → 存储槽：storage = (pv + offset) mod res（有符号正模）。
// 代进去 offset 会被消掉：storage = (floor(p/v) + res/2) mod res，即"世界对齐的环形
// 瓦片"——相机移动不搬运数据，滚动只负责释放出界的槽。
fn volume_to_storage(pos_volume: vec3i, offset: vec3i, res: i32) -> vec3i {
    return vec3i(
        pos_mod(pos_volume.x + offset.x, res),
        pos_mod(pos_volume.y + offset.y, res),
        pos_mod(pos_volume.z + offset.z, res),
    );
}

// 存储槽 + 当前偏移 → 该槽代表的体积坐标：pv = (storage − offset) mod res。
// 滚动用它把槽换回世界位置做球判据。
fn storage_to_volume(storage: vec3i, offset: vec3i, res: i32) -> vec3i {
    return vec3i(
        pos_mod(storage.x - offset.x, res),
        pos_mod(storage.y - offset.y, res),
        pos_mod(storage.z - offset.z, res),
    );
}

// 体积坐标（pv）在体积空间里的世界位置：体素中心 = (pv − res/2 + 0.5 + offset)·v。
fn volume_world_center(pos_volume: vec3i, offset: vec3i, voxel: f32, res: i32) -> vec3f {
    let pv = vec3f(pos_volume) - vec3f(f32(res) * 0.5) + vec3f(0.5) + vec3f(offset);
    return pv * voxel;
}

// cell 线性化：cascadeBase + 16·(x·res² + y·res + z) + bucket（行主序，与 octahedral 一致）。
fn cell_index(cascade: u32, storage: vec3i, bucket: u32, res: u32) -> u32 {
    let plane = u32(storage.x) * res * res + u32(storage.y) * res + u32(storage.z);
    return cascade * (res * res * res * BUCKETS_PER_CELL) + BUCKETS_PER_CELL * plane + bucket;
}

fn cell_cascade(cell: u32, res: u32) -> u32 {
    return cell / (res * res * res * BUCKETS_PER_CELL);
}

fn cell_storage(cell: u32, res: u32) -> vec3i {
    let plane = (cell % (res * res * res * BUCKETS_PER_CELL)) / BUCKETS_PER_CELL;
    return vec3i(
        i32(plane / (res * res)),
        i32((plane / res) % res),
        i32(plane % res),
    );
}

fn cell_bucket(cell: u32) -> u32 {
    return cell % BUCKETS_PER_CELL;
}

// 心跳/帧号回绕：16 位模距离（直接相减会在约 18 分钟处把全场同时判为超龄）。
fn heartbeat_age(frame_index: u32, heartbeat: u32) -> u32 {
    return (frame_index - heartbeat) & 0xFFFFu;
}

// ---- 深度重建 ----

// 深度反投影：uv → shader-NDC（uv.y = 0.5 − 0.5·ndc.y 的逆变换）→ clip → world。
fn world_from_depth(uv: vec2f, depth: f32, inv_view_proj: mat4x4f) -> vec3f {
    let ndc = vec3f(uv.x * 2.0 - 1.0, 1.0 - 2.0 * uv.y, depth);
    let world = inv_view_proj * vec4f(ndc, 1.0);
    return world.xyz / world.w;
}

// 相邻深度是否可参与差分：0 = 背景，相对差超过门限 = 不连续（前景/背景交界）。
fn depth_neighbor_ok(d0: f32, d: f32) -> bool {
    return d > 0.0 && abs(d - d0) <= DEPTH_EDGE_RELATIVE * d0;
}

// 深度一阶差分重建平面法线：4 邻域深度各自反投影，任一轴的两个 tap 都有效才取差；
// 两轴都无效返回零向量（调用方跳过该样本，不播种）。法线统一翻到视向侧。
fn flat_normal(
    p: vec3f,
    uv: vec2f,
    texel: vec2f,
    d0: f32,
    d_left: f32,
    d_right: f32,
    d_down: f32,
    d_up: f32,
    inv_view_proj: mat4x4f,
    camera_pos: vec3f,
) -> vec3f {
    var t = vec3f(0.0);
    var b = vec3f(0.0);
    if (depth_neighbor_ok(d0, d_left) && depth_neighbor_ok(d0, d_right)) {
        let p_left = world_from_depth(uv - vec2f(texel.x, 0.0), d_left, inv_view_proj);
        let p_right = world_from_depth(uv + vec2f(texel.x, 0.0), d_right, inv_view_proj);
        t = p_right - p_left;
    }
    if (depth_neighbor_ok(d0, d_up) && depth_neighbor_ok(d0, d_down)) {
        let p_up = world_from_depth(uv - vec2f(0.0, texel.y), d_up, inv_view_proj);
        let p_down = world_from_depth(uv + vec2f(0.0, texel.y), d_down, inv_view_proj);
        b = p_down - p_up;
    }
    if (dot(t, t) < 1e-12 || dot(b, b) < 1e-12) {
        return vec3f(0.0);
    }
    var n = normalize(cross(t, b));
    if (dot(n, camera_pos - p) < 0.0) {
        n = -n;
    }
    return n;
}

// ---- 调试视图调色（覆盖图与方片共用）----

// 整数哈希 → 伪随机颜色（cell 归属分区用）。
fn hash_color(h: u32) -> vec3f {
    var x = h * 747796405u + 2891336453u;
    x = ((x >> ((x >> 28u) + 4u)) ^ x) * 277803737u;
    x = (x >> 22u) ^ x;
    return vec3f(f32(x & 255u), f32((x >> 8u) & 255u), f32((x >> 16u) & 255u)) / 255.0;
}

// 16 桶调色：桶中心方向的 RGB 编码掺一点哈希，保证相邻桶不混淆。
fn bucket_color(bucket: u32) -> vec3f {
    let dir = bucket_index_to_direction(bucket, BUCKET_RESOLUTION);
    return abs(dir) * 0.75 + hash_color(bucket * 2654435761u) * 0.25;
}

// 级联四色（粗级更暖）。
fn cascade_color(cascade: u32) -> vec3f {
    switch cascade {
        case 0u: { return vec3f(0.20, 0.75, 0.95); }
        case 1u: { return vec3f(0.30, 0.90, 0.35); }
        case 2u: { return vec3f(0.95, 0.80, 0.20); }
        default: { return vec3f(0.95, 0.35, 0.20); }
    }
}

// 心跳年龄：新绿 → 老红（阈值 = 淘汰门限）。
fn age_color(age: u32, evict_age: u32) -> vec3f {
    let t = clamp(f32(age) / max(f32(evict_age), 1.0), 0.0, 1.0);
    return mix(vec3f(0.15, 0.85, 0.20), vec3f(0.95, 0.15, 0.10), t);
}

// 覆盖状态（视图 1）：有 patch 绿 / 无 patch 红（出 Volume 的蓝由调用方先判）。
fn coverage_color(patch_index: u32) -> vec3f {
    return select(vec3f(0.90, 0.15, 0.12), vec3f(0.15, 0.85, 0.25), patch_index != INVALID_PATCH);
}

// 位置偏离（视图 7）：patch 位置到当前表面切平面的垂直距离 / 体素边长，
// 绿 < 0.1、黄 < 0.5、红更大。
fn deviation_color(deviation: f32) -> vec3f {
    if (deviation < PATCH_DEVIATION_GREEN) {
        return vec3f(0.15, 0.85, 0.25);
    }
    if (deviation < PATCH_DEVIATION_RED) {
        return vec3f(0.95, 0.80, 0.20);
    }
    return vec3f(0.90, 0.12, 0.10);
}

// 子视图统一调色（覆盖图与方片共用；视图 6 的顶部条只在覆盖图里另画）。
// 编号：0 关 / 1 覆盖 / 2 级联 / 3 桶 / 4 rank / 5 心跳 / 6 池水位 / 7 位置偏离 / 8 cell 归属。
fn view_color(
    view: u32,
    cell: u32,
    patch_index: u32,
    cascade: u32,
    bucket: u32,
    age: u32,
    evict_age: u32,
    deviation: f32,
) -> vec3f {
    switch view {
        case 1u: { return coverage_color(patch_index); }
        case 2u: { return cascade_color(cascade); }
        case 3u: { return bucket_color(bucket); }
        case 4u: {
            // rank 存 state 低 8 位；9.5 恒 0（全绿），9.6 的 bounce 请求会抬到 1（黄）。
            let rank = 0u;
            return select(vec3f(0.95, 0.80, 0.20), vec3f(0.15, 0.85, 0.25), rank == 0u);
        }
        case 5u: { return age_color(age, evict_age); }
        case 7u: { return deviation_color(deviation); }
        case 8u: { return hash_color(cell / BUCKETS_PER_CELL); }
        default: { return hash_color(cell); }
    }
}

// LDR 目标按线性值直存，最后写 swapchain 时 SRGB 视图做一次编码；调色板按"上屏观感"
// 给值，这里预补偿到线性域，避免整张视图发白。
fn display_encode(c: vec3f) -> vec3f {
    return pow(clamp(c, vec3f(0.0), vec3f(1.0)), vec3f(2.2));
}
