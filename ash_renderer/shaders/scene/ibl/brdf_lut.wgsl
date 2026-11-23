// DFG/BRDF LUT（compute 单入口，一次性生成）：split-sum 的第二项 A/B（Karis 的
// 重要性采样积分）。V 固定在切空间 (√(1−NoV²), 0, NoV)，N = +z，逐像素扫
// (NoV, perceptualRoughness) 网格。
//
// 布局：x 轴 = NoV、y 轴 = perceptualRoughness；128²、RGBA16F（xy = A/B，zw 未用，
// 与环境 cubemap 同格式，采样器/格式能力只需查一次）。
// 接口：b0 = 目标 2D storage（单级）。dispatch = (16, 16, 1)。
// #import brdf
// #import ibl

const DFG_SIZE: f32 = 128.0;
const DFG_SAMPLES: u32 = 256u;

@group(0) @binding(0) var dst: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = vec2<u32>(textureDimensions(dst));
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let no_v = (f32(gid.x) + 0.5) / DFG_SIZE;
    let p = (f32(gid.y) + 0.5) / DFG_SIZE;
    let a = p * p;
    let v = vec3f(sqrt(max(1.0 - no_v * no_v, 0.0)), 0.0, no_v);
    let n = vec3f(0.0, 0.0, 1.0);
    var scale = 0.0;
    var bias = 0.0;
    for (var i = 0u; i < DFG_SAMPLES; i = i + 1u) {
        let h = importance_sample_ggx(hammersley(i, DFG_SAMPLES), n, a);
        let l = reflect(-v, h);
        let no_l = l.z; // dot(n, l)
        if (no_l > 0.0) {
            let no_h = max(h.z, 0.0);
            let vo_h = max(dot(v, h), 0.0);
            // G_Vis = G · VoH / (NoH · NoV)：把几何项折进可见性，F 项分离后就是 A/B
            let g = V_SmithGGXCorrelated(a, no_v, no_l);
            let g_vis = g * vo_h / max(no_h * no_v, 1e-6);
            let fc = pow5(1.0 - vo_h);
            scale += (1.0 - fc) * g_vis;
            bias += fc * g_vis;
        }
    }
    let inv = 1.0 / f32(DFG_SAMPLES);
    // 坑：NoV→0 且粗糙度→0 时 G_Vis = G·VoH/(NoH·NoV) 按 1/NoV³ 发散（Denominator 的
    // 1e-6 护栏挡不住），积分值可达 1e6 量级；RGBA16F 存成 Inf，随后高光 Inf → HDR
    // Inf → Bloom 的 soft-knee 出现 Inf/Inf = NaN，整块 tile 变黑（实测 53 像素拖黑
    // 大半个画面）。物理上 A+B 是反射率（≤ 1），超 1 的值全是数值产物；钳到 1 顺带
    // 压掉掠射角的塑料感爆点（叶片/路面正常贴图的高频法线在 A 大的区域会闪白）。
    let dfg_a = min(scale * inv, 1.0);
    let dfg_b = min(bias * inv, 1.0);
    textureStore(dst, vec2<i32>(gid.xy), vec4f(dfg_a, dfg_b, 0.0, 1.0));
}
