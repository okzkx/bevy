// Clustered 光源族：光源常驻表记录 + cluster 网格参数 + 逐 cluster 列表。
// 与 src/render/pipeline/cluster.rs（pack_cluster_uniforms / CLUSTER_* /
// MAX_PER_CLUSTER）和 src/render/shading/light_table.rs（LightRecord::pack）同源。

// 光源记录（64B）：_pad 的 vec3 成员对齐把步长从 48 撑到 64，宿主 pack 同布局。
struct LightRecord {
    pos_radius: vec4f,       // xyz 世界位置 + w 半径（米；≤0 = 死灯，门卫跳过）
    color_intensity: vec4f,  // 颜色（rgb 线性）× 强度（w，candela 域）
    flags: u32,              // bit0 = shadow；bits 8..15 = type
    _pad: vec3<u32>,
}

// cluster 网格参数（160B）：view_proj @0 / inv_view_proj @64 / 标量 @128..。
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
    indices: array<u32, 16>,  // MAX_PER_CLUSTER
}

const MAX_PER_CLUSTER: u32 = 16u;
const CLUSTER_TILE: f32 = 32.0;  // CLUSTER_TILE_SIZE（像素）

// 片元 → cluster 线性索引：屏幕 tile（像素坐标）× 指数深度片（分片公式与
// cluster_build 同式）。view_depth = 世界位置经 view_proj 的 clip.w（透视下即
// 视空间距离，cluster_build 分片口径）。坑点：不能用片元 @builtin(position).w——
// 片段阶段它是 1/clip.w，全屏 pass 里更恒为 1，深度片会退化成固定一片、漏灯。
fn cluster_index(p: ClusterParams, pixel_xy: vec2f, view_depth: f32) -> u32 {
    let tile = vec2<u32>(pixel_xy / vec2f(CLUSTER_TILE));
    let tile_i = min(tile.x, p.tiles_x - 1u) + min(tile.y, p.tiles_y - 1u) * p.tiles_x;
    let ratio = p.far / p.near;
    let z = clamp(view_depth, p.near, p.far);
    let slice = min(u32(f32(p.slices) * log2(z / p.near) / log2(ratio)), p.slices - 1u);
    return tile_i + slice * p.tiles_x * p.tiles_y;
}
