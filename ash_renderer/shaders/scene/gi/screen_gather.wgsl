// 9.7.4 1/4 屏幕查表（compute）：每线程一个 1/4 像素（= 全分辨率 4×4 块），按 9.7.1 的网格
// 口径在该块内落 3 个样本点，各自反投影成世界位置、查 Volume 内 rank 0 的 patch，加权合成后
// 压成 L0（物理单位）+ L1（归一化方向）写两张 1/4 目标。
//
// 口径（9.7 施工计划 §0.1/§0.2/§9.7.4）：
// - 查表只在 1/4 分辨率做；全分辨率那一步（9.7.5）只做双边上采样与 SH 求值，不再查缓存；
// - 样本规则照 Unity：**中心样本权重 0.01 且不查 rank**（只回答"这里有没有缓存"），其余样本
//   只接受 `rank == 0` 且已估计过的 patch；
// - 本段**不做** Chebyshev 软可见性或任何软阴影式权重——Unity 无此实现，加了就是没有出处的滤波面；
// - Volume 外直接写 `a = 0`（合成端据此回 IBL 基线），不在这里求 9.3 的 probe。
//
// 读的是**时域输出**（`b15`，9.7.3 的 B），不是空间滤波的中间产物 A——A 只是滤波链的中间态。
//
// 接口：
// - set0 b0：帧 UBO（inv_view_proj / camera_pos）。
// - set1：GI 表（b0 cell→patch / b5 统计 / b9 GI UBO / b15 时域辐照度）。
// - set2：G-buffer 采样集（b1 normal / b3 depth）。
// - set3：本段目标（b0 `scgi_l0` A / b1 `scgi_l1` A；b2/b3 是 B 组，9.7.5 去噪写它）。
// - push 32B：低/全分辨率尺寸 + 帧号 + 法线来源 + 样本数。
// #import frame
// #import gi_cell
// #import gi_lookup
// #import gi_compression

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var<storage, read> cell_patch: array<u32>;
@group(1) @binding(5) var<storage, read> patch_statistics: array<PatchStatistics>;
@group(1) @binding(9) var<uniform> gi: GiUniforms;
@group(1) @binding(15) var<storage, read> filtered_irradiance1: array<PatchIrradiance>;
@group(2) @binding(1) var normal_tex: texture_2d<f32>;
@group(2) @binding(3) var depth_tex: texture_2d<f32>;
@group(3) @binding(0) var scgi_l0_out: texture_storage_2d<rgba16float, write>;
@group(3) @binding(1) var scgi_l1_out: texture_storage_2d<rgba8unorm, write>;

// push 布局与 `src/render/passes/gi/screen_gather.rs` 的 `pack_screen_gather_push` 一一对应。
struct PushScreenGather {
    low_w: u32,
    low_h: u32,
    full_w: u32,
    full_h: u32,
    frame_index: u32,
    // 0 = 深度差分平面法线；1 = G-buffer 着色法线（与播种同一个开关）。
    normal_source: u32,
    // 每 1/4 像素的样本数（1..3；默认 3）。
    samples: u32,
    // 保留。
    reserved: u32,
}
var<immediate> push: PushScreenGather;

// 中心样本的权重：只用来回答"这里有没有缓存"，几乎不影响颜色，但不查 rank。
const CENTER_SAMPLE_WEIGHT: f32 = 0.01;

// 一个样本的查表结果（RGBL1 四系数，各 vec3）。
struct GatherSample {
    // 命中且可用（depth 有效、在 Volume 内、cell 有 patch）。
    valid: bool,
    // 命中的 patch 是 rank 0 且已估计过（中心样本不要求这一条）。
    rank_zero: bool,
    l0: vec3f,
    l1x: vec3f,
    l1y: vec3f,
    l1z: vec3f,
}

// 单点查表：反投影 → 法线 → 定位 → 读 patch。查不到就返回 valid = false。
fn gather_one(pixel: vec2u, n_source: u32) -> GatherSample {
    var out: GatherSample;
    out.valid = false;
    out.rank_zero = false;
    out.l0 = vec3f(0.0);
    out.l1x = vec3f(0.0);
    out.l1y = vec3f(0.0);
    out.l1z = vec3f(0.0);

    let coord = vec2i(i32(pixel.x), i32(pixel.y));
    let depth = textureLoad(depth_tex, coord, 0).r;
    if (depth <= 0.0) {
        return out;
    }
    let full = vec2f(f32(push.full_w), f32(push.full_h));
    // +0.5 不能省：直接拿整数坐标当 uv 会偏半像素（9.5 播种同款钉子）
    let uv = (vec2f(f32(pixel.x), f32(pixel.y)) + vec2f(0.5)) / full;
    let p = world_from_depth(uv, depth, frame.inv_view_proj);
    let texel = vec2f(1.0) / full;

    var n = vec3f(0.0);
    if (n_source == 1u) {
        n = textureLoad(normal_tex, coord, 0).xyz;
    } else {
        let d_left = textureLoad(depth_tex, coord + vec2i(-1, 0), 0).r;
        let d_right = textureLoad(depth_tex, coord + vec2i(1, 0), 0).r;
        let d_up = textureLoad(depth_tex, coord + vec2i(0, -1), 0).r;
        let d_down = textureLoad(depth_tex, coord + vec2i(0, 1), 0).r;
        n = flat_normal(
            p,
            uv,
            texel,
            depth,
            d_left,
            d_right,
            d_down,
            d_up,
            frame.inv_view_proj,
            frame.camera_pos.xyz,
        );
    }
    if (dot(n, n) < 1e-8) {
        return out;
    }
    n = normalize(n);

    let key = lookup_cell_key(
        p,
        n,
        gi.target_pos.xyz,
        gi.volume.x,
        u32(gi.volume.y),
        u32(gi.volume.z),
        gi.cascade_offsets,
    );
    if (key.cascade == INVALID_PATCH) {
        return out;
    }
    let patch_slot = cell_patch[key.cell];
    if (patch_slot == INVALID_PATCH) {
        return out;
    }
    let statistics = patch_statistics[patch_slot];
    let irr = filtered_irradiance1[patch_slot];
    out.valid = true;
    out.rank_zero = (statistics.state & 0xFFu) == 0u && statistics.update_count >= 1u;
    out.l0 = irr.l0.xyz;
    out.l1x = irr.l1x.xyz;
    out.l1y = irr.l1y.xyz;
    out.l1z = irr.l1z.xyz;
    return out;
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    if (gid.x >= push.low_w * push.low_h) {
        return;
    }
    let low = vec2u(gid.x % push.low_w, gid.x / push.low_w);
    let offsets = gather_sample_offsets(push.frame_index);
    let sample_count = clamp(push.samples, 1u, GATHER_SAMPLE_COUNT);

    var acc_l0 = vec3f(0.0);
    var acc_l1x = vec3f(0.0);
    var acc_l1y = vec3f(0.0);
    var acc_l1z = vec3f(0.0);
    var weight_sum = 0.0;
    var rank_zero_weight = 0.0;
    var attempted_weight = 0.0;

    for (var i = 0u; i < sample_count; i = i + 1u) {
        let pixel = low_to_full(low, offsets[i]);
        if (pixel.x >= push.full_w || pixel.y >= push.full_h) {
            continue;
        }
        // 中心样本（i == 0）权重 0.01 且不查 rank；其余只收 rank 0
        let is_center = i == 0u;
        let weight = select(1.0, CENTER_SAMPLE_WEIGHT, is_center);
        let sample = gather_one(pixel, push.normal_source);
        if (!sample.valid) {
            continue;
        }
        attempted_weight = attempted_weight + weight;
        if (!is_center && !sample.rank_zero) {
            // 几何上命中了缓存，但那条 patch 只有 IBL 种子（rank 1 或还没估计过）
            continue;
        }
        acc_l0 = acc_l0 + sample.l0 * weight;
        acc_l1x = acc_l1x + sample.l1x * weight;
        acc_l1y = acc_l1y + sample.l1y * weight;
        acc_l1z = acc_l1z + sample.l1z * weight;
        weight_sum = weight_sum + weight;
        if (sample.rank_zero) {
            rank_zero_weight = rank_zero_weight + weight;
        }
    }

    var out_l0 = vec3f(0.0);
    var ratio_x = 0.5;
    var ratio_y = 0.5;
    var ratio_z = 0.5;
    // a = 命中 rank 0 的样本权重占比（Volume 外 / 无命中 = 0）。分母取"尝试过的样本权重"，
    // Volume 边界处于是得到 0..1 的软过渡，合成端看到的就是一圈软边。
    var coverage = 0.0;
    if (weight_sum > 0.0) {
        out_l0 = acc_l0 / weight_sum;
        ratio_x = l1_encode_axis(acc_l1x / weight_sum, out_l0);
        ratio_y = l1_encode_axis(acc_l1y / weight_sum, out_l0);
        ratio_z = l1_encode_axis(acc_l1z / weight_sum, out_l0);
    }
    if (attempted_weight > 0.0) {
        coverage = clamp(rank_zero_weight / attempted_weight, 0.0, 1.0);
    }

    let coord = vec2i(i32(low.x), i32(low.y));
    textureStore(scgi_l0_out, coord, vec4f(out_l0, coverage));
    textureStore(scgi_l1_out, coord, vec4f(ratio_x, ratio_y, ratio_z, 0.5));
}
