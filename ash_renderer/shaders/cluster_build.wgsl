// 8.1.3 透明 Clustered Forward：cluster 光源列表构建（compute 单入口）。
// 视锥按 32px tile × 32 深度片分 3D cluster（开工即 3D，用户 2026-10-20 拍板），
// 逐 cluster 以世界空间 AABB 与点光球求交，写光源索引表（b2）+ 逐 cluster 计数
//（b3，HOST_VISIBLE——ash_renderer/cluster_census 逐 tile 对照的读数面）。
//
// 接口（透明族独立小集，cluster.rs 布局契约——hiz 先例：不进 set2）：
// - b0 ClusterParams（160B：view_proj + inv_view_proj + 标量，每帧槽直写）。
// - b1 LightRecord 数组（光源常驻表本帧槽环，8.2.2 切表——脏行增量重写，
//   稳态零上传；槽号即 cluster 列表引用的稳定身份）。
// - b2 cluster 光源索引表（DEVICE_LOCAL，count + 16 索引/cluster）。
// - b3 逐 cluster 计数（HOST_VISIBLE，本 compute 全量重写——无清零需求）。
//
// AABB 构造：深度片界（视图深度指数分片）经 view_proj 投成 NDC z（轴点式，
// 与 CPU 参照 slice_ndc_z 同式），4 tile 角 × 2 片深共 8 角点经 inv_view_proj
// 反投影成世界 AABB。像素→NDC 的 y 翻转约定与 gbuffer_composite 同源
//（顶行 ↔ shader-NDC +1）。
//
// 并发语义：每 invocation 独占写 lists[ci]/counts[ci]，普通 store 无需原子；
// 索引序 = 光源遍历序（对照按集合语义，不承诺顺序）。

// 64B 记录（_pad 的成员对齐把步长从 48 撑到 64——Rust 侧 pack 同布局，
// light_table.rs 互证）
struct LightRecord {
    pos_radius: vec4f,       // xyz 世界位置 + w 半径（米；0 = 死灯，门卫跳过）
    color_intensity: vec4f,  // 颜色（rgb 线性）× 强度（w，candela 域）
    flags: u32,              // bit0 = shadow（8.3 预留）；bits 8..15 = type
    _pad: vec3<u32>,
}

struct ClusterParams {
    view_proj: mat4x4f,
    inv_view_proj: mat4x4f,
    near: f32,
    far: f32,
    tiles_x: u32,
    tiles_y: u32,
    slices: u32,
    light_count: u32,
    extent_w: f32,
    extent_h: f32,
}

struct ClusterRecord {
    count: u32,
    indices: array<u32, 16>,  // MAX_PER_CLUSTER（cluster.rs 同源）
}

@group(0) @binding(0) var<storage, read> params: ClusterParams;
@group(0) @binding(1) var<storage, read> lights: array<LightRecord>;
@group(0) @binding(2) var<storage, read_write> lists: array<ClusterRecord>;
@group(0) @binding(3) var<storage, read_write> counts: array<u32>;

const MAX_PER_CLUSTER: u32 = 16u;

// 视图深度 → NDC z（与 CPU 参照 slice_ndc_z 同式；bevy 视图右手系，距离取 −z）
fn slice_ndc_z(z_view: f32) -> f32 {
    let clip = params.view_proj * vec4f(0.0, 0.0, -z_view, 1.0);
    return clip.z / clip.w;
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let ci = gid.x;
    let total = params.tiles_x * params.tiles_y * params.slices;
    if ci >= total {
        return;
    }
    let per_layer = params.tiles_x * params.tiles_y;
    let slice = ci / per_layer;
    let tile = ci % per_layer;
    let tx = tile % params.tiles_x;
    let ty = tile / params.tiles_x;
    // 指数分片（与 CPU 参照 slice_view_depth 同式）
    let ratio = params.far / params.near;
    let z0 = params.near * pow(ratio, f32(slice) / f32(params.slices));
    let z1 = params.near * pow(ratio, (f32(slice) + 1.0) / f32(params.slices));
    let cz0 = slice_ndc_z(z0);
    let cz1 = slice_ndc_z(z1);
    // tile 像素矩形 → shader-NDC（y 翻转：顶行 ↔ +1，与 CPU 参照同式）
    let x0 = f32(tx * 32u) / params.extent_w * 2.0 - 1.0;
    let x1 = f32((tx + 1u) * 32u) / params.extent_w * 2.0 - 1.0;
    let y0 = 1.0 - 2.0 * f32(ty * 32u) / params.extent_h;
    let y1 = 1.0 - 2.0 * f32((ty + 1u) * 32u) / params.extent_h;
    var aabb_min = vec3f(1e30);
    var aabb_max = vec3f(-1e30);
    for (var k = 0u; k < 8u; k = k + 1u) {
        let x = select(x0, x1, (k & 1u) == 1u);
        let y = select(y0, y1, (k & 2u) == 2u);
        let cz = select(cz0, cz1, (k & 4u) == 4u);
        let clip = params.inv_view_proj * vec4f(x, y, cz, 1.0);
        let world = clip.xyz / clip.w;
        aabb_min = min(aabb_min, world);
        aabb_max = max(aabb_max, world);
    }
    var count = 0u;
    var record: ClusterRecord;
    for (var l = 0u; l < params.light_count; l = l + 1u) {
        let lp = lights[l].pos_radius.xyz;
        let r = lights[l].pos_radius.w;
        if (r <= 0.0) {
            continue;  // 死灯门卫（8.2.2：回收槽清零语义，CPU 参照同式）
        }
        let closest = clamp(lp, aabb_min, aabb_max);
        if (distance(closest, lp) <= r) {
            if (count < MAX_PER_CLUSTER) {
                record.indices[count] = l;
            }
            count = count + 1u;  // 超容量照增计数（截断语义与 CPU 参照同）
        }
    }
    record.count = min(count, MAX_PER_CLUSTER);
    lists[ci] = record;
    counts[ci] = record.count;
}
