// 屏幕空间反射步进（10.3，半分辨率 compute）。
//
// 口径（与施工计划 §1 一致）：
// - **世界空间步进**：`P(t) = origin + dir·t`，每步用 `frame.view_proj` 投影回屏幕取 uv 与
//   视深（`clip.w`，与 SSAO/调试图 MODE_DEPTH 同口径），不引入视图矩阵（UBO 无 view 矩阵，
//   352B 契约不动）。
// - 命中判定：射线点的视深 > 该像素场景视深（射线已在该像素后面），且差值小于
//   `max(thickness, 本步步长)`（步长作为容差，避免大步长跳过薄物体）。命中后 4 次二分细化。
// - 天空（reverse-Z 深度 <= 1e-6）与屏幕外的步直接跳过；起点沿法线抬 2 cm 防自相交。
// - 输出半分辨率 RGBA16F：rgb = 命中色（从 HDR 拷贝里取，已含曝光），a = 权重 w：
//   `w = gate_roughness · gate_distance · edge_fade · intensity`，clamp 到 0..1。
//   w = 0 表示「这次不接管」，合成 pass 靠它做加 0 的 IEEE 恒等。
// - 粗糙度门控：`perceptualRoughness > roughness_cutoff` 的像素直接写 0，反射交给 IBL。
// - 不做时域累积、不做 Hi-Z（首版线性步进，主计划 §5 D4）。

// #import frame
// #import ibl

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var albedo_tex: texture_2d<f32>;
@group(1) @binding(1) var normal_tex: texture_2d<f32>;
@group(1) @binding(2) var hdr_tex: texture_2d<f32>;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(1) @binding(4) var gbuf_sampler: sampler;
@group(1) @binding(5) var matparam_tex: texture_2d<f32>;
@group(2) @binding(0) var reflection: texture_storage_2d<rgba16float, write>;

struct PushSsr {
    full_w: u32,
    full_h: u32,
    half_w: u32,
    half_h: u32,
    steps: u32,
    _pad0: u32,
    max_t: f32,
    thickness: f32,
    roughness_cutoff: f32,
    edge_fade: f32,
    intensity: f32,
    _pad1: u32,
}

var<immediate> push: PushSsr;

// 深度反投影：uv → shader-NDC → clip → world（与 gbuffer_composite / ssao 同式）。
fn world_from_depth(uv: vec2f, depth: f32) -> vec3f {
    let ndc = vec3f(uv.x * 2.0 - 1.0, 1.0 - 2.0 * uv.y, depth);
    let clip = vec4f(ndc, 1.0);
    let world = frame.inv_view_proj * clip;
    return world.xyz / world.w;
}

// 视深：透视下 clip.w 就是沿视线方向的正距离（取投影矩阵第 4 行点积）。
fn clip_w(clip: vec4f) -> f32 {
    let row = vec4f(
        frame.view_proj[0].w,
        frame.view_proj[1].w,
        frame.view_proj[2].w,
        frame.view_proj[3].w,
    );
    return dot(row, clip);
}

// 世界点 → 屏幕 uv；返回 false 表示在相机背后或屏幕外。
fn project_uv(p: vec3f) -> vec2f {
    let clip = frame.view_proj * vec4f(p, 1.0);
    let ndc = clip.xy / clip.w;
    return vec2f(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);
}

fn inside(uv: vec2f) -> bool {
    return uv.x >= 0.0 && uv.x < 1.0 && uv.y >= 0.0 && uv.y < 1.0;
}

@compute @workgroup_size(8, 8)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let half = vec2u(max(push.half_w, 1u), max(push.half_h, 1u));
    if (gid.x >= half.x || gid.y >= half.y) {
        return;
    }
    let full = vec2f(f32(max(push.full_w, 1u)), f32(max(push.full_h, 1u)));
    let uv = (vec2f(gid.xy) + vec2f(0.5)) / vec2f(half);
    let pix = vec2i(clamp(uv * full, vec2f(0.0), full - vec2f(1.0)));
    let depth = textureLoad(depth_tex, pix, 0).r;
    // 天空：没有可反射的东西，写 0（合成 pass 加 0 恒等）
    if (depth <= 1e-6) {
        textureStore(reflection, vec2i(gid.xy), vec4f(0.0));
        return;
    }
    let matparam = textureLoad(matparam_tex, pix, 0);
    let perceptual_roughness = matparam.b;
    // 粗糙度门控：模糊面交给 IBL，屏幕空间反射只服务镜面
    if (perceptual_roughness > push.roughness_cutoff) {
        textureStore(reflection, vec2i(gid.xy), vec4f(0.0));
        return;
    }
    let world = world_from_depth(uv, depth);
    let v = normalize(frame.camera_pos.xyz - world);
    let n_raw = normalize(textureLoad(normal_tex, pix, 0).xyz);
    // 法线翻到视向侧（双面几何），与合成端 `ibl_environmental` 同口径
    let n = ibl_normal(n_raw, v);
    let dir = reflect(-v, n);
    let origin = world + n * 0.02;
    let t_min = 0.25;
    let steps = max(push.steps, 1u);
    // 对数步进：近处步密（薄物体/近场反射不发虚），远处步疏（一根 16 m 的射线不必每 25 cm 采一次）。
    // 均匀步进在同样的步数下要么漏薄物体、要么把 max_t 吃掉。
    let max_t = max(push.max_t, t_min * 1.01);
    let ratio = exp(log(max_t / t_min) / f32(steps));

    var prev_t = t_min;
    let prev_clip = frame.view_proj * vec4f(origin + dir * t_min, 1.0);
    var prev_w = clip_w(prev_clip);
    // 上一个「明确在几何之前」的采样点（delta < 0）。命中细化必须从这对前后夹逼开始，
    // 否则 bracket 落在几何内部，二分收敛到错误的交点，表现为掠射处一圈圈台阶带。
    var front_t = t_min;
    var has_front = false;
    var hit_t = t_min;
    var hit_uv = uv;
    var found = false;
    var i = 0u;
    loop {
        if (i >= steps) {
            break;
        }
        let t = t_min * pow(ratio, f32(i + 1u));
        let p = origin + dir * t;
        let clip = frame.view_proj * vec4f(p, 1.0);
        if (clip.w <= 0.0) {
            break;
        }
        let suv = project_uv(p);
        if (inside(suv)) {
            let spix = vec2i(clamp(suv * full, vec2f(0.0), full - vec2f(1.0)));
            let sd = textureLoad(depth_tex, spix, 0).r;
            if (sd > 1e-6) {
                let sndc = vec3f(
                    (f32(spix.x) + 0.5) / f32(max(push.full_w, 1u)) * 2.0 - 1.0,
                    1.0 - (f32(spix.y) + 0.5) / f32(max(push.full_h, 1u)) * 2.0,
                    sd,
                );
                let scene_w = clip_w(vec4f(sndc, 1.0));
                let delta = clip.w - scene_w;
                // 命中三条件：①射线点落在该像素场景之后 ②深度差在厚度内（绝对容差，
                // 不能拿步长当容差——掠射地面时会产生整片假命中）③命中面朝向来向
                // （掠射同一平面时法线与射线同向，dot >= 0，直接排除自反射假命中）
                let hit_n = normalize(textureLoad(normal_tex, spix, 0).xyz);
                // 容差 = 材质厚度 + 本步的视深增量。掠射时射线横向走很远而视深几乎不变，
                // 固定容差会把「擦过表面」的射线判成没碰到——正是偏轴机位整片为 0 的原因。
                let tol = push.thickness + min(abs(clip.w - prev_w), push.thickness);
                if (delta < 0.0) {
                    front_t = t;
                    has_front = true;
                }
                if (delta > 0.0 && delta < tol && dot(hit_n, dir) < -0.05) {
                    hit_t = t;
                    hit_uv = suv;
                    found = true;
                    break;
                }
            }
        }
        prev_w = clip.w;
        prev_t = t;
        i = i + 1u;
    }
    if (!found) {
        textureStore(reflection, vec2i(gid.xy), vec4f(0.0));
        return;
    }

    // 二分细化：t0 在几何前面、t1 在后面，4 次把命中点收敛到像素级。
    // t0 取「最后一个 delta < 0 的采样点」而不是上一个采样点——掠射时上一个采样点往往
    // 已经在几何内部，那样二分区间不含真实交点。
    var t0 = select(max(prev_t, t_min), front_t, has_front);
    var t1 = hit_t;
    for (var k = 0u; k < 4u; k = k + 1u) {
        let tm = 0.5 * (t0 + t1);
        let p = origin + dir * tm;
        let clip = frame.view_proj * vec4f(p, 1.0);
        if (clip.w <= 0.0) {
            break;
        }
        let suv = project_uv(p);
        if (!inside(suv)) {
            break;
        }
        let spix = vec2i(clamp(suv * full, vec2f(0.0), full - vec2f(1.0)));
        let sd = textureLoad(depth_tex, spix, 0).r;
        if (sd <= 1e-6) {
            t0 = tm;
            continue;
        }
        let sndc = vec3f(
            (f32(spix.x) + 0.5) / f32(max(push.full_w, 1u)) * 2.0 - 1.0,
            1.0 - (f32(spix.y) + 0.5) / f32(max(push.full_h, 1u)) * 2.0,
            sd,
        );
        if (clip.w > clip_w(vec4f(sndc, 1.0))) {
            t1 = tm;
        } else {
            t0 = tm;
        }
    }

    let final_uv = project_uv(origin + dir * t1);
    if (!inside(final_uv)) {
        textureStore(reflection, vec2i(gid.xy), vec4f(0.0));
        return;
    }
    let color = textureSampleLevel(hdr_tex, gbuf_sampler, final_uv, 0.0).rgb;
    // 权重三项：粗糙度、距离、屏幕边缘（边缘的命中会被裁掉，不可信）
    let gate_roughness = saturate(1.0 - perceptual_roughness / max(push.roughness_cutoff, 1e-3));
    let gate_distance = saturate(1.0 - t1 / max(push.max_t, 1e-3));
    let border = min(min(final_uv.x, 1.0 - final_uv.x), min(final_uv.y, 1.0 - final_uv.y));
    let edge = saturate(border / max(push.edge_fade, 1e-4));
    let w = clamp(gate_roughness * gate_distance * edge * push.intensity, 0.0, 1.0);
    textureStore(reflection, vec2i(gid.xy), vec4f(color, w));
}
