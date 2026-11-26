// 颜色校验的采样侧（9.9）：把 tonemap 链的三个位置打成直方图，写进 bins SSBO。
//
//   A 域 = present 采样到的原始 HDR（未加 Bloom、未乘曝光）
//   B 域 = 曲线输入色（tonemap_shared 的 tonemap_input_color，与 present 同式）
//   C 域 = tonemap 写进 LDR 的值（线性 UNORM；本 shader 读回后做 sRGB 编码）
//
// 录制点在 tonemap 出场屏障之后、SMAA edge 之前；`frame.mode != 0` 或
// `BloomSettings::tonemap == false` 时宿主整段跳过（present 直出 HDR，没有 C 域）。
//
// bins 布局（u32，宿主侧常量镜像在 src/render/passes/post/color_scope.rs）：
//   [0..16)      头：magic / stride / 宽 / 高 / 帧号 / 曝光状态 4×f32 / 保留 7（本 shader 写）
//   [16..1045)   A：luma 256 + RGB 3×256 + 计数 5（样本 / 低于轴 / 高于轴 / ≤0 / ≥1）
//   [1045..2074) B：同 A
//   [2074..3103) C：luma 256 + RGB parade 3×256 + 计数 5
//   [3103..7199) C 色度：CbCr 64×64（行 = Cr，列 = Cb，均映射到 0..1）
//
// 轴：A/B 是绝对 log2（−16…+8 EV，每 bin 0.09375 EV）；C 是 sRGB 编码后的 0…1
// （每 bin 1/256）。A/B 的 RGB 通道共用同一 log2 轴；越轴通道不落 bin，只靠
// luma 的三个计数（低于/高于/零）暴露。
// #import tonemap_shared

@group(0) @binding(0) var hdr_tex: texture_2d<f32>;
@group(0) @binding(1) var blit_sampler: sampler;
@group(0) @binding(2) var bloom_tex: texture_2d<f32>;
@group(0) @binding(3) var bloom_sampler: sampler;
@group(0) @binding(4) var<uniform> bloom_params: BloomParams;
@group(0) @binding(5) var<storage, read> exposure_state: array<vec4f>;
@group(0) @binding(6) var ldr_tex: texture_2d<f32>;
@group(0) @binding(7) var<storage, read_write> bins: array<atomic<u32>>;

// 宿主每帧压入的采样帧号（写进 bins 头，供 frame_age 对账）。WGSL 新语法用
// `immediate` 地址空间（同一管线布局里的 push constant）。
var<immediate> frame_index: u32;

// 与宿主侧 COLOR_SCOPE_* 常量一一对应（layout_contract 测试会核对总长）
const STRIDE: u32 = 4u;
const HEADER_U32: u32 = 16u;
const DOMAIN_U32: u32 = 1029u;
const BINS_U32: u32 = 7199u;
const COUNT_OFFSET: u32 = 1024u;
// 域内布局：luma 256 bin，随后 R/G/B 各 256 bin（parade 与 luma 不共用 256 窗口）
const CHANNEL_STRIDE: u32 = 256u;

// 每工作组的在界样本数只在本组共享内存里累加，组末由 0 号线程一次性写进三域
// CNT_SAMPLES。直接对同一个全局地址做 36 万次原子写会被串行化——这是 9.9 成本
// 判定线唯一超标的热点（p95 40 ms → 目标内）。
var<workgroup> wg_samples: atomic<u32>;

// bins 头 magic：宿主侧 COLOR_SCOPE_MAGIC 的镜像（字节 "CSCP" 的小端 u32）
const MAGIC: u32 = 0x50435343u;

const BASE_A: u32 = HEADER_U32;
const BASE_B: u32 = BASE_A + DOMAIN_U32;
const BASE_C: u32 = BASE_B + DOMAIN_U32;
const BASE_CBCR: u32 = BASE_C + DOMAIN_U32;

// 计数槽（相对域起点）
const CNT_SAMPLES: u32 = 0u;
const CNT_BELOW: u32 = 1u;
const CNT_ABOVE: u32 = 2u;
const CNT_ZERO: u32 = 3u;
const CNT_CLIP: u32 = 4u;

// A/B 轴：−16…+8 EV 共 24 EV 铺 256 bin
const AXIS_MIN_EV: f32 = -16.0;
const AXIS_EV_SPAN: f32 = 24.0;
const BIN_PER_EV: f32 = 256.0 / AXIS_EV_SPAN;

// Rec.709 亮度权重（与 present 的 ACES 输入一致：sRGB 原色域）
const LUMA_W: vec3f = vec3f(0.2126, 0.7152, 0.0722);

/// 单通道 log2 直方图：越轴/零值按计数记账，不落 bin。
fn add_log2_axis(domain: u32, rgb: vec3f) {
    // RGB parade：三个通道共用绝对 log2 轴
    for (var ch = 0u; ch < 3u; ch = ch + 1u) {
        let v = rgb[ch];
        if (v > 0.0) {
            let ev = log2(v);
            if (ev >= AXIS_MIN_EV && ev < AXIS_MIN_EV + AXIS_EV_SPAN) {
                let bin = u32((ev - AXIS_MIN_EV) * BIN_PER_EV);
                atomicAdd(&bins[domain + (ch + 1u) * CHANNEL_STRIDE + min(bin, 255u)], 1u);
            }
        }
    }
    // luma：轴、低于、高于、零
    let luma = dot(rgb, LUMA_W);
    if (luma <= 0.0) {
        atomicAdd(&bins[domain + COUNT_OFFSET + CNT_ZERO], 1u);
        return;
    }
    let ev = log2(luma);
    if (ev < AXIS_MIN_EV) {
        atomicAdd(&bins[domain + COUNT_OFFSET + CNT_BELOW], 1u);
        return;
    }
    if (ev >= AXIS_MIN_EV + AXIS_EV_SPAN) {
        atomicAdd(&bins[domain + COUNT_OFFSET + CNT_ABOVE], 1u);
        return;
    }
    let bin = u32((ev - AXIS_MIN_EV) * BIN_PER_EV);
    atomicAdd(&bins[domain + min(bin, 255u)], 1u);
}

/// sRGB 传递函数（LDR 存线性值，面板/直方图用编码后的感知轴）。
fn srgb_encode(c: vec3f) -> vec3f {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3f(0.0)), vec3f(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3f(0.0031308));
}

/// 单通道 0…1 直方图（C 域：sRGB 编码后的 luma 与 RGB parade）。
fn add_unit_axis(domain: u32, encoded: vec3f) {
    for (var ch = 0u; ch < 3u; ch = ch + 1u) {
        let bin = u32(clamp(encoded[ch], 0.0, 1.0) * 255.999);
        atomicAdd(&bins[domain + (ch + 1u) * CHANNEL_STRIDE + min(bin, 255u)], 1u);
    }
    let luma = dot(encoded, LUMA_W);
    let bin = u32(clamp(luma, 0.0, 1.0) * 255.999);
    atomicAdd(&bins[domain + min(bin, 255u)], 1u);
}

/// C 域色度：编码后 RGB 的 CbCr（BT.709），铺成 64×64（行 Cr、列 Cb）。
fn add_chroma(encoded: vec3f) {
    let y = dot(encoded, LUMA_W);
    let cb = (encoded.z - y) / 1.8556;
    let cr = (encoded.x - y) / 1.5748;
    let ix = u32(clamp(cb + 0.5, 0.0, 0.999999) * 64.0);
    let iy = u32(clamp(cr + 0.5, 0.0, 0.999999) * 64.0);
    atomicAdd(&bins[BASE_CBCR + iy * 64u + ix], 1u);
}

@compute @workgroup_size(8, 8)
fn cs_main(
    @builtin(global_invocation_id) gid: vec3u,
    @builtin(local_invocation_index) local_invocation_index: u32,
) {
    let dims = textureDimensions(hdr_tex);
    // 头（magic / stride / 尺寸 / 帧号 / 曝光状态快照）：只让 (0,0) 线程写。
    // bins 在显存里（DEVICE_LOCAL），头也由这里写，宿主不再每帧往缓冲里拷 28.8 KB。
    if (all(gid == vec3u(0u, 0u, 0u))) {
        atomicStore(&bins[0], MAGIC);
        atomicStore(&bins[1], STRIDE);
        atomicStore(&bins[2], dims.x);
        atomicStore(&bins[3], dims.y);
        atomicStore(&bins[4], frame_index);
        let state = exposure_state[0];
        atomicStore(&bins[5], bitcast<u32>(state.x));
        atomicStore(&bins[6], bitcast<u32>(state.y));
        atomicStore(&bins[7], bitcast<u32>(state.z));
        atomicStore(&bins[8], bitcast<u32>(state.w));
    }
    let coord = gid.xy * STRIDE;
    // 越界线程不能提前 return：组末的 workgroupBarrier 要求整组都到达，
    // 这里改成单边 if，越界线程只跳过采样。
    if (coord.x < dims.x && coord.y < dims.y) {
        let uv = (vec2f(coord) + vec2f(0.5)) / vec2f(dims);
        // 三域的样本数相同（同一批在界线程），本组先累加，组末一次写回
        atomicAdd(&wg_samples, 1u);

        // A 域：present 采样到的原始 HDR
        let hdr = textureSampleLevel(hdr_tex, blit_sampler, uv, 0.0).rgb;
        add_log2_axis(BASE_A, hdr);

        // B 域：曲线输入色（与 present 同式，含 Bloom 与曝光倍数）
        var bloom = vec3f(0.0);
        if (bloom_params.enabled > 0.5) {
            bloom = textureSampleLevel(bloom_tex, bloom_sampler, uv, 0.0).rgb;
        }
        let b = tonemap_input_color(hdr, bloom, bloom_params, exposure_scale_of(exposure_state[0]));
        add_log2_axis(BASE_B, b);

        // C 域：tonemap 写进 LDR 的线性 UNORM 值，面板轴用 sRGB 编码后的感知值
        let c_linear = textureSampleLevel(ldr_tex, blit_sampler, uv, 0.0).rgb;
        if (all(c_linear <= vec3f(0.0))) {
            // 与 A/B 同口径：全零样本只计数、不落 bin，直方图和 + 零 = 样本数才成立
            atomicAdd(&bins[BASE_C + COUNT_OFFSET + CNT_ZERO], 1u);
        } else {
            let c = srgb_encode(c_linear);
            add_unit_axis(BASE_C, c);
            add_chroma(c);
        }
        // 触顶比例：线性值任一通道 ≥ 1 的样本（"零"已在上面单列，不在这里重复记）
        if (any(c_linear >= vec3f(1.0))) {
            atomicAdd(&bins[BASE_C + COUNT_OFFSET + CNT_CLIP], 1u);
        }
    }

    // 组末把本组样本数合并回三域（每组 3 次全局原子写，此前是每样本 3 次）。
    workgroupBarrier();
    let lid = local_invocation_index;
    if (lid == 0u) {
        let v = atomicLoad(&wg_samples);
        if (v > 0u) {
            atomicAdd(&bins[BASE_A + COUNT_OFFSET + CNT_SAMPLES], v);
            atomicAdd(&bins[BASE_B + COUNT_OFFSET + CNT_SAMPLES], v);
            atomicAdd(&bins[BASE_C + COUNT_OFFSET + CNT_SAMPLES], v);
        }
    }
}
