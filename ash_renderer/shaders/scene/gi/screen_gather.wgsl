// 9.7.4 1/4 屏幕查表（compute）：每线程一个 1/4 像素（= 全分辨率 4×4 块），按 9.7.1 的网格
// 口径在该块内落 3 个样本点，各自反投影成世界位置、查 Volume 内 rank 0 的 patch，加权合成后
// 压成 L0（物理单位）+ L1（归一化方向）写两张 1/4 目标。
//
// 口径（9.7 施工计划 §0.1/§0.2/§9.7.4）：
// - 查表只在 1/4 分辨率做；全分辨率那一步（9.7.5）只做双边上采样与 SH 求值，不再查缓存；
// - 播种样本权重 0.01，参考几何独立取固定真实子像素；颜色只接受"已有真实估计"
//   （update_count ≥ 1）的缓存条目，不看 rank——rank 1 的辐照度仍在每帧估计；
// - **覆盖率只由参考几何决定**（有缓存 = 1、无 = 0），不进抖动样本的加权。抖动样本随帧旋转，
//   落到无 patch 的格子时会把覆盖率压到 0.005，合成端于是在遮挡后的 SCGI 与未遮挡的 IBL 之间
//   整块来回跳——那就是暗区的持续闪烁；
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

// 本帧播种点的弱颜色权重；参考几何独立选择，颜色仍须满足 rank 0 门槛。
const SEED_SAMPLE_WEIGHT: f32 = 0.01;

// 一个样本的查表结果（RGBL1 四系数，各 vec3）。
struct GatherSample {
    // 命中且可用（depth 有效、在 Volume 内、cell 有 patch）。
    valid: bool,
    // 可用于颜色和覆盖率：槽里已有真实估计（update_count ≥ 1）。**不看 rank**——rank 1 只是
    // "不再被播种刷新"，estimation 仍每帧更新它，辐照度是活的；拿 rank 当门槛会让覆盖率随
    // 采样相位整块翻转，合成端于是在遮挡后的 SCGI 与未遮挡的 IBL 之间来回跳。
    usable: bool,
    surface_valid: bool,
    position: vec3f,
    normal: vec3f,
    l0: vec3f,
    l1x: vec3f,
    l1y: vec3f,
    l1z: vec3f,
}

// 单点查表：反投影 → 法线 → 定位 → 读 patch。查不到就返回 valid = false。
fn gather_one(pixel: vec2u, n_source: u32) -> GatherSample {
    var out: GatherSample;
    out.valid = false;
    out.usable = false;
    out.surface_valid = false;
    out.position = vec3f(0.0);
    out.normal = vec3f(0.0);
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
    n = scgi_face_normal(n, p, frame.camera_pos.xyz);
    out.surface_valid = true;
    out.position = p;
    out.normal = scgi_stable_guide_normal(
        depth_tex, coord, vec2u(push.full_w, push.full_h), p, depth,
        n, frame.inv_view_proj, frame.camera_pos.xyz,
    );

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
    out.usable = statistics.update_count >= 1u;
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
    let last_pixel = vec2u(push.full_w - 1u, push.full_h - 1u);
    var guide_pixel = min(low_to_full(low, vec2u(1u, 1u)), last_pixel);
    var guide_offset = guide_pixel - low * 4u;
    var reference = gather_one(guide_pixel, push.normal_source);
    // 几何身份不随采样抖动；中心无效时按固定次序选一个真实表面。
    if (!reference.surface_valid) {
        for (var phase = 0u; phase < 16u; phase = phase + 1u) {
            let pixel = min(low_to_full(low, vec2u(phase % 4u, phase / 4u)), last_pixel);
            let candidate = gather_one(pixel, push.normal_source);
            if (candidate.surface_valid) {
                reference = candidate;
                guide_offset = pixel - low * 4u;
                break;
            }
        }
    }
    // 覆盖率只看确定性参考几何：中心表面有缓存条目就算覆盖。跟着抖动样本走会让覆盖率
    // 随采样相位在 0.005 与 1.0 之间翻（样本落到无 patch 的格子时），合成端于是在遮挡后的
    // SCGI 与未遮挡的 IBL 之间来回跳——暗区就是用户看到的持续闪烁。
    if (reference.surface_valid && !reference.usable) {
        for (var phase = 0u; phase < 16u; phase = phase + 1u) {
            let pixel = min(low_to_full(low, vec2u(phase % 4u, phase / 4u)), last_pixel);
            let candidate = gather_one(pixel, push.normal_source);
            if (candidate.surface_valid && candidate.usable) {
                reference = candidate;
                guide_offset = pixel - low * 4u;
                break;
            }
        }
    }
    let guide_phase = f32(guide_offset.y * 4u + guide_offset.x) / 15.0;
    if (!reference.surface_valid) {
        let coord = vec2i(low);
        textureStore(scgi_l0_out, coord, vec4f(0.0));
        textureStore(scgi_l1_out, coord, vec4f(0.5, 0.5, 0.5, guide_phase));
        return;
    }

    // 参考几何本身就是一次确定性采样：先把它放进累加，抖动样本只作细化。
    // 若只累加抖动样本，样本全落在未估计/刚回收的格子上时 weight_sum = 0，
    // 块会被写成黑色（覆盖率却仍是 1），合成端于是在"正常 E"与"纯黑"之间来回跳
    // （实测同一块在 0 与 600 之间翻转）。
    var acc_l0 = reference.l0;
    var acc_l1x = reference.l1x;
    var acc_l1y = reference.l1y;
    var acc_l1z = reference.l1z;
    var weight_sum = 1.0;

    for (var i = 0u; i < sample_count; i = i + 1u) {
        let pixel = min(low_to_full(low, offsets[i]), vec2u(push.full_w - 1u, push.full_h - 1u));
        // 首样本跟随播种相位（权重 0.01，只作弱引导，不主导颜色）。
        let base_weight = select(1.0, SEED_SAMPLE_WEIGHT, i == 0u);
        let sample = gather_one(pixel, push.normal_source);
        if (!sample.surface_valid) {
            continue;
        }
        let weight = base_weight * scgi_geometry_weight(
            reference.position, reference.normal, sample.position, sample.normal, frame.camera_pos.xyz,
        );
        if (!sample.usable || weight <= 0.0) {
            continue;
        }
        acc_l0 = acc_l0 + sample.l0 * weight;
        acc_l1x = acc_l1x + sample.l1x * weight;
        acc_l1y = acc_l1y + sample.l1y * weight;
        acc_l1z = acc_l1z + sample.l1z * weight;
        weight_sum = weight_sum + weight;
    }

    var out_l0 = vec3f(0.0);
    var ratio_x = 0.5;
    var ratio_y = 0.5;
    var ratio_z = 0.5;
    // 覆盖率只由参考几何的缓存状态决定（0 或 1），不跟抖动样本走：那会随采样相位在
    // 0.005 与 1.0 之间跳，合成端于是在遮挡后的 SCGI 与未遮挡的 IBL 之间来回闪。
    // 参考还没估计过（刚分配、update_count = 0）时给 0，让合成端退回 IBL，而不是注入黑洞。
    let coverage = select(0.0, 1.0, reference.usable);
    if (weight_sum > 0.0) {
        out_l0 = acc_l0 / weight_sum;
        ratio_x = l1_encode_axis(acc_l1x / weight_sum, out_l0);
        ratio_y = l1_encode_axis(acc_l1y / weight_sum, out_l0);
        ratio_z = l1_encode_axis(acc_l1z / weight_sum, out_l0);
    }

    let coord = vec2i(i32(low.x), i32(low.y));
    textureStore(scgi_l0_out, coord, vec4f(out_l0, coverage));
    textureStore(scgi_l1_out, coord, vec4f(ratio_x, ratio_y, ratio_z, guide_phase));
}
