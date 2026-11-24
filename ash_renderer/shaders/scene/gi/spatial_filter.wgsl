// 9.7.2 patch 级空间滤波（compute）：每活跃 patch 在本帧估计出的辐照度上，沿切平面按
// `radius_cells × 体素边长` 投若干抖动样本，用 9.7.1 的只读查表取邻居 patch 的辐照度，
// 按 `1 − |jitter|/√2` 归一化混合后写 `filtered_irradiance0`（b14）。
//
// 三条硬口径：
// 1. **不原地写**：读 `patch_irradiance{0,1}`（estimation 的本帧输出），写独立的 filtered 表。
//    同一 dispatch 里边写边读邻居，读到的值取决于工作组调度顺序——那是不可复现的噪声源。
// 2. **不把 rank 1 与刚分配的 patch 抹开**：它们身上只有 IBL 种子，铺开会污染整片表面；
//    自身与邻居都按同一条门槛（rank 0 且 `update_count ≥ 1`）筛。
// 3. **查不到的样本跳过**：累加器以自身值 × 权重 1 起头，因此一个邻居都没查到时输出与输入
//    逐位相同——不需要额外分支，也不会出现"归一化除零"。
//
// 线程 = 池容量（每线程一条 patch，空槽直接返回）。
//
// 接口：
// - set0 b0：帧 UBO（只保证与其它 compute 的集序一致，本文件不读它的字段）。
// - set1：GI 表（b0 cell→patch / b2 几何 / b3+b4 辐照度双份 / b5 统计 / b6 patch→cell /
//   b9 GI UBO / b14 输出）。
// - push（`var<immediate>`，24B）：开关 / 样本数 / 帧号 / 池容量 / 半径倍数。
// #import frame
// #import gi_cell
// #import gi_lookup
// #import gi_sampling

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var<storage, read> cell_patch: array<u32>;
@group(1) @binding(2) var<storage, read> patch_geometry: array<PatchGeometry>;
@group(1) @binding(3) var<storage, read> patch_irradiance0: array<PatchIrradiance>;
@group(1) @binding(4) var<storage, read> patch_irradiance1: array<PatchIrradiance>;
@group(1) @binding(5) var<storage, read> patch_statistics: array<PatchStatistics>;
@group(1) @binding(6) var<storage, read> patch_cell: array<u32>;
@group(1) @binding(9) var<uniform> gi: GiUniforms;
@group(1) @binding(14) var<storage, read_write> filtered_irradiance0: array<PatchIrradiance>;

// push 布局与 `src/render/passes/gi/filtering.rs` 的 `pack_spatial_filter_push` 一一对应。
struct PushSpatialFilter {
    // 0 = 直拷（A 与源逐位一致）；1 = 滤波。
    enabled: u32,
    // 每 patch 的抖动样本数（1..8）。
    taps: u32,
    // 帧号：抖动序列的样本起点偏移（同帧同 patch 确定）。
    frame_index: u32,
    // 池容量（`gi.volume.w` 的冗余副本，防越界读写）。
    capacity: u32,
    // 切平面采样半径（体素边长的倍数）。
    radius_cells: f32,
    // 对齐填充（保持 24B）。
    reserved: f32,
}
var<immediate> push: PushSpatialFilter;

// 抖动幅度上限：jitter 在 [-1,1]² 内，|jitter| ∈ [0, √2]。
const SPATIAL_JITTER_MAX: f32 = 1.41421356;

// 邻居样本被接受的门槛：与自己同一条（rank 0 且已估计过）。
fn neighbor_usable(statistics: PatchStatistics) -> bool {
    return (statistics.state & 0xFFu) == 0u && statistics.update_count >= 1u;
}

// 读本帧 estimation 写的那份（奇偶由 GI UBO 的 frame.x 给出，与 estimation 同源）。
fn patch_irradiance_at(slot: u32) -> PatchIrradiance {
    if ((gi.frame.x & 1u) != 0u) {
        return patch_irradiance1[slot];
    }
    return patch_irradiance0[slot];
}

// 切平面抖动 taps：自身权重 1，邻居按 `1 − |jitter|/√2` 加权；一个邻居都没查到就原样返回。
fn spatial_filter(slot: u32, source: PatchIrradiance) -> PatchIrradiance {
    let geometry = patch_geometry[slot];
    let normal_raw = geometry.normal.xyz;
    let len_normal = length(normal_raw);
    if (len_normal <= 0.0) {
        return source;
    }
    // 滤波按**着色法线**建切平面（世界缓存也是按它分桶），不拿 G-buffer 平面法线
    let normal = normal_raw / len_normal;
    let position = geometry.position.xyz;
    // 半径以该 patch 所在级联的体素边长为单位：粗级联上的采样半径自然放大
    let cascade = cell_cascade(patch_cell[slot], u32(gi.volume.y));
    let voxel = voxel_size(gi.volume.x, cascade);
    let radius = push.radius_cells * voxel;
    let basis = gi_tangent_basis(normal);
    let center = gi.target_pos.xyz;
    let res = u32(gi.volume.y);
    let cascades = u32(gi.volume.z);

    // 自身先按权重 1 计入：查不到邻居时输出与输入逐位一致，也避免整块能量漂移
    var acc_l0 = source.l0.xyz;
    var acc_l1x = source.l1x.xyz;
    var acc_l1y = source.l1y.xyz;
    var acc_l1z = source.l1z.xyz;
    var weight_sum = 1.0;

    var rng = gi_r2_init(low_bias_hash_32(slot), push.frame_index * 64u);
    let taps = clamp(push.taps, 1u, 8u);
    for (var i = 0u; i < taps; i = i + 1u) {
        let jitter = gi_r2_sample2(rng, i) * 2.0 - vec2f(1.0);
        gi_r2_next(&rng);
        // 切平面内偏移：basis 的列是 (t, b, n)，local z = 0 即落在切平面里
        let offset = basis * vec3f(jitter.x, jitter.y, 0.0);
        let tap_position = position + offset * radius;
        let key = lookup_cell_key(
            tap_position,
            normal,
            center,
            gi.volume.x,
            res,
            cascades,
            gi.cascade_offsets,
        );
        if (key.cascade == INVALID_PATCH) {
            continue;
        }
        let neighbor_slot = cell_patch[key.cell];
        if (neighbor_slot == INVALID_PATCH || neighbor_slot == slot) {
            continue;
        }
        if (!neighbor_usable(patch_statistics[neighbor_slot])) {
            continue;
        }
        let weight = 1.0 - length(jitter) / SPATIAL_JITTER_MAX;
        if (weight <= 0.0) {
            continue;
        }
        let neighbor = patch_irradiance_at(neighbor_slot);
        acc_l0 = acc_l0 + neighbor.l0.xyz * weight;
        acc_l1x = acc_l1x + neighbor.l1x.xyz * weight;
        acc_l1y = acc_l1y + neighbor.l1y.xyz * weight;
        acc_l1z = acc_l1z + neighbor.l1z.xyz * weight;
        weight_sum = weight_sum + weight;
    }

    var out: PatchIrradiance;
    out.l0 = vec4f(acc_l0 / weight_sum, source.l0.w);
    out.l1x = vec4f(acc_l1x / weight_sum, source.l1x.w);
    out.l1y = vec4f(acc_l1y / weight_sum, source.l1y.w);
    out.l1z = vec4f(acc_l1z / weight_sum, source.l1z.w);
    return out;
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let slot = gid.x;
    if (slot >= push.capacity) {
        return;
    }
    // 活跃判据与 estimation 一致：patch→cell 反向指针为 INVALID = 空槽
    if (patch_cell[slot] == INVALID_PATCH) {
        return;
    }
    let source = patch_irradiance_at(slot);
    var out: PatchIrradiance = source;
    let statistics = patch_statistics[slot];
    if (push.enabled != 0u && neighbor_usable(statistics)) {
        out = spatial_filter(slot, source);
    }
    filtered_irradiance0[slot] = out;
}
