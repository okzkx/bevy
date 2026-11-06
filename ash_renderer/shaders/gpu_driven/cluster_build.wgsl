// cluster 光源列表构建（compute 单入口）：视锥按 32px tile × 32 深度片分 3D
// cluster，逐 cluster 以世界空间 AABB 与点光球求交，写光源索引表（b2）+ 逐
// cluster 计数（b3，CPU 可见，BRP cluster_census 逐 tile 对照时直读）。
//
// 接口（光源分簇集，布局见 src/render/passes/lighting/cluster.rs）：
// - b0 ClusterParams（160B，每帧槽直写）。
// - b1 LightRecord 数组（光源常驻表本帧槽；槽号就是 cluster 列表里存的索引）。
// - b2 cluster 光源索引表（DEVICE_LOCAL，count + 16 索引/cluster）。
// - b3 逐 cluster 计数（HOST_VISIBLE，本 compute 全量重写，无需清零）。
//
// AABB 构造：深度片界（视深指数分片）换成 NDC z（与 CPU 参照 slice_ndc_z 同式），
// 4 个 tile 角 × 2 个片深共 8 角点经 inv_view_proj 反投影成世界 AABB。像素→NDC 的
// y 翻转与 gbuffer_composite 一致（顶行 ↔ NDC +1）。
//
// 每个 invocation 独占写 lists[ci]/counts[ci]，普通 store 即可，无需原子；
// 索引顺序 = 光源遍历顺序（对照按集合比较，不承诺顺序）。
// #import cluster

@group(0) @binding(0) var<storage, read> params: ClusterParams;
@group(0) @binding(1) var<storage, read> lights: array<LightRecord>;
@group(0) @binding(2) var<storage, read_write> lists: array<ClusterRecord>;
@group(0) @binding(3) var<storage, read_write> counts: array<u32>;

// 视深 → NDC z（与 CPU 参照 slice_ndc_z 同式）：透视下 clip.w = 视深 d，
// NDC z = a + b/d；在中心射线上取两个已知 NDC z 反投影量出 d，解出 a、b。
// 坑：不能把视空间点 (0,0,−z) 直接乘 view_proj（那是把视空间当世界空间），相机
// 不在原点时深度片整体错位，cluster 列表按 tile 阶梯状漏灯。
fn depth_at_ndc(ndc_z: f32) -> f32 {
    let p = params.inv_view_proj * vec4f(0.0, 0.0, ndc_z, 1.0);
    return (params.view_proj * vec4f(p.xyz / p.w, 1.0)).w;
}

fn slice_ndc_z(z_view: f32) -> f32 {
    let n0 = 0.5;
    let n1 = 0.25;
    let d0 = depth_at_ndc(n0);
    let d1 = depth_at_ndc(n1);
    let b = (n0 - n1) / (1.0 / d0 - 1.0 / d1);
    let a = n0 - b / d0;
    return a + b / z_view;
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
    // tile 像素矩形 → NDC（y 翻转：顶行 ↔ +1，与 CPU 参照同式）
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
            continue;  // 已回收的空槽半径为 0，跳过（CPU 参照同）
        }
        let closest = clamp(lp, aabb_min, aabb_max);
        if (distance(closest, lp) <= r) {
            if (count < MAX_PER_CLUSTER) {
                record.indices[count] = l;
            }
            count = count + 1u;  // 超容量照样计数，最后截断（与 CPU 参照结果一致）
        }
    }
    record.count = min(count, MAX_PER_CLUSTER);
    lists[ci] = record;
    counts[ci] = record.count;
}
