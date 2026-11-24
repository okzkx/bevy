// 9.5 屏幕播种与分配（compute）：1/4 网格逐样本反投影 G-buffer 深度 → 平面法线 →
// 级联/桶/cell → 解析 patch（刷新或新分配）。只建池与生命周期，不出辐照度（9.6）、
// 不接屏幕（9.7）。
//
// 接口：
// - set0 b0：每帧 UBO（inv_view_proj 反投影、ibl_enabled/ibl_mode/ibl_intensity 种子）。
// - set1 b1/b3：G-buffer normal（A/B 着色法线）与深度（只 textureLoad，绝不 filter——
//   D32 线性过滤不可依赖，平均后的深度反投影是错的几何）。
// - set2 b3/b4：IBL SH-L2 系数 / GlobalProbe 16 桶（种子基线；与 patch 的 RGBL1 同格式）。
// - set3 b0..b9：cell/patch 表、ring 配置、计数、GI UBO（布局见 gi_cell.wgsl 与
//   resident/tables/surface_cache.rs）。
// - push（var<immediate>，32B）：低/全分辨率尺寸 + 帧号 + 法线来源。
//
// 分配两态（照 Unity PatchAllocation 语义）：
// - cell 已有 patch：刷几何/心跳、rank 强制 0，辐照度历史保留（本段种子只在新建时写）。
// - 新 cell：CellAllocationMarks 纪元标签仲裁同帧同 cell（标签 = 帧号，免每帧清 8 MiB）；
//   抢到者 ring 追加一条，池满只计失败（不增长），失败归还标签允许后续帧重试。
// 同帧多线程写同一新 patch 的几何是"同值竞写"（与 Unity 同级的良性竞态）：冲突只发生在
// 同 cell 的不同样本像素之间，差值是同一体素内的采样抖动。
// #import frame
// #import gi_cell
// #import gi_ring

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(1) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(2) @binding(3) var<storage, read> sh_coeffs: array<vec4f, 9>;
@group(2) @binding(4) var<storage, read> probe_buckets: array<vec4f, 64>;
@group(3) @binding(0) var<storage, read_write> cell_patch: array<u32>;
@group(3) @binding(1) var<storage, read_write> cell_marks: array<atomic<u32>>;
@group(3) @binding(2) var<storage, read_write> patch_geometry: array<PatchGeometry>;
@group(3) @binding(3) var<storage, read_write> patch_irradiance0: array<PatchIrradiance>;
@group(3) @binding(4) var<storage, read_write> patch_irradiance1: array<PatchIrradiance>;
@group(3) @binding(5) var<storage, read_write> patch_statistics: array<PatchStatistics>;
@group(3) @binding(6) var<storage, read_write> patch_cell: array<u32>;
@group(3) @binding(7) var<storage, read_write> ring: array<atomic<u32>, 12>;
@group(3) @binding(8) var<storage, read_write> counters: array<atomic<u32>, 16>;
@group(3) @binding(9) var<uniform> gi: GiUniforms;

var<immediate> push: PushAlloc;
struct PushAlloc {
    low_w: u32,
    low_h: u32,
    full_w: u32,
    full_h: u32,
    frame_index: u32,
    // 0 = 深度差分平面法线（默认）；1 = G-buffer 着色法线（A/B 对照）。
    normal_source: u32,
    pad0: u32,
    pad1: u32,
}

// 新 patch 的种子：IBL 漫反射基线（9.5 还没有低分辨率辐照度生产者，先验 = 环境光）。
// 两种编码都给出 RGBL1 四系数，与 patch 存储同格式；IBL 关 = 全 0。
fn seed_irradiance(n: vec3f) -> PatchIrradiance {
    var out: PatchIrradiance;
    out.l0 = vec4f(0.0);
    out.l1x = vec4f(0.0);
    out.l1y = vec4f(0.0);
    out.l1z = vec4f(0.0);
    if (frame.ibl_enabled == 0u || gi.ring.z == 0u) {
        return out;
    }
    let intensity = frame.ibl_intensity;
    if (frame.ibl_mode == 1u) {
        // 16 桶：直接取该方向桶的 RGBL1（求值式 l0 + l1·n，见 ibl.wgsl 的 probe16_eval）
        let base = direction_to_bucket_index(n, BUCKET_RESOLUTION) * 4u;
        out.l0 = probe_buckets[base] * intensity;
        out.l1x = probe_buckets[base + 1u] * intensity;
        out.l1y = probe_buckets[base + 2u] * intensity;
        out.l1z = probe_buckets[base + 3u] * intensity;
    } else {
        // SH-L2 只取 l=0/l=1 带（高阶折进 l0 会改变近场各向异性，故只丢不折）；
        // 系数已折余弦卷积 A_l 与 Y 基常数，l1 的 0.48860251 是 Y1 基函数常数。
        out.l0 = vec4f(sh_coeffs[0].rgb * 0.28209479 * intensity, 0.0);
        out.l1x = vec4f(sh_coeffs[3].rgb * 0.48860251 * intensity, 0.0);
        out.l1y = vec4f(sh_coeffs[1].rgb * 0.48860251 * intensity, 0.0);
        out.l1z = vec4f(sh_coeffs[2].rgb * 0.48860251 * intensity, 0.0);
    }
    return out;
}

// ring 追加在 common/gi_cell.wgsl 的 ring_append（9.6.3 起两个分配入口共用）。

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let total = push.low_w * push.low_h;
    if (gid.x >= total) {
        return;
    }
    // 处理序轮转：低分辨率像素序按帧号平移（双射），池将满时"哪些 cell 先拿预算"
    // 不固定偏向屏幕某角。子像素轮转 = 4×4 块内 16 帧走遍。
    let rotated = (gid.x + push.frame_index * 2654435761u) % total;
    let low_x = rotated % push.low_w;
    let low_y = rotated / push.low_w;
    let sub = (push.frame_index * 7u) % 16u;
    let full_x = low_x * 4u + sub % 4u;
    let full_y = low_y * 4u + sub / 4u;
    if (full_x >= push.full_w || full_y >= push.full_h) {
        return;
    }
    let pixel = vec2i(i32(full_x), i32(full_y));
    let depth = textureLoad(depth_tex, pixel, 0).r;
    if (depth <= 0.0) {
        return;
    }
    let full = vec2f(f32(push.full_w), f32(push.full_h));
    // 1/4 像素中心 → 全分辨率像素中心 → uv（+0.5 不能省：直接拿整数坐标当 uv 会偏半像素）
    let uv = (vec2f(f32(full_x), f32(full_y)) + vec2f(0.5)) / full;
    let p = world_from_depth(uv, depth, frame.inv_view_proj);
    let texel = vec2f(1.0) / full;

    var n = vec3f(0.0);
    if (push.normal_source == 1u) {
        n = textureLoad(normal_tex, pixel, 0).xyz;
    } else {
        let d_left = textureLoad(depth_tex, pixel + vec2i(-1, 0), 0).r;
        let d_right = textureLoad(depth_tex, pixel + vec2i(1, 0), 0).r;
        let d_up = textureLoad(depth_tex, pixel + vec2i(0, -1), 0).r;
        let d_down = textureLoad(depth_tex, pixel + vec2i(0, 1), 0).r;
        n = flat_normal(
            p, uv, texel, depth, d_left, d_right, d_down, d_up,
            frame.inv_view_proj, frame.camera_pos.xyz,
        );
    }
    if (dot(n, n) < 1e-8) {
        return;
    }
    n = normalize(n);

    let center = gi.target_pos.xyz;
    let res = u32(gi.volume.y);
    let cascades = u32(gi.volume.z);
    let voxel_min = gi.volume.x;
    let cascade = resolve_cascade(center, p, voxel_min, res, cascades);
    if (cascade == INVALID_PATCH) {
        return;
    }
    let voxel = voxel_size(voxel_min, cascade);
    let offset = gi.cascade_offsets[cascade].xyz;
    let pv = volume_position(p, voxel, offset, i32(res));
    let storage = volume_to_storage(pv, offset, i32(res));
    let bucket = direction_to_bucket_index(n, BUCKET_RESOLUTION);
    let cell = cell_index(cascade, storage, bucket, res);

    // —— 已有 patch：刷新几何/心跳、rank 0、辐照度历史保留 ——
    var existing = cell_patch[cell];
    // 陈旧指针兜底：槽已归别人（整理搬走后又被新分配复用）就清掉重来，别刷新别人的 patch
    if (existing != INVALID_PATCH && patch_cell[existing] != cell) {
        cell_patch[cell] = INVALID_PATCH;
        existing = INVALID_PATCH;
    }
    if (existing != INVALID_PATCH) {
        patch_geometry[existing].position = vec4f(p, 1.0);
        patch_geometry[existing].normal = vec4f(n, 0.0);
        patch_statistics[existing].heartbeat = push.frame_index & 0xFFFFu;
        patch_statistics[existing].state = patch_statistics[existing].state & ~0xFFu;
        return;
    }

    // —— 新 cell：同帧同 cell 只有一个线程拿到分配权（纪元标签仲裁）——
    let epoch = push.frame_index | 1u;
    let previous = atomicExchange(&cell_marks[cell], epoch);
    if (previous == epoch) {
        return;
    }
    let capacity = u32(gi.volume.w);
    let slot = ring_append(capacity);
    if (slot == INVALID_PATCH) {
        atomicStore(&cell_marks[cell], 0u);
        atomicAdd(&counters[1], 1u);
        return;
    }
    let seed = seed_irradiance(n);
    let heartbeat = push.frame_index & 0xFFFFu;
    patch_geometry[slot].position = vec4f(p, 1.0);
    patch_geometry[slot].normal = vec4f(n, 0.0);
    patch_irradiance0[slot] = seed;
    patch_irradiance1[slot] = seed;
    patch_statistics[slot].mean = vec4f(seed.l0.xyz, 0.0);
    patch_statistics[slot].variance = vec4f(0.0);
    patch_statistics[slot].state = 0u;
    patch_statistics[slot].request = 0u;
    patch_statistics[slot].update_count = 0u;
    patch_statistics[slot].heartbeat = heartbeat;
    patch_cell[slot] = cell;
    cell_patch[cell] = slot;
    atomicAdd(&counters[0], 1u);
    let active_count = atomicAdd(&ring[1], 1u) + 1u;
    atomicMax(&ring[2], active_count);
}
