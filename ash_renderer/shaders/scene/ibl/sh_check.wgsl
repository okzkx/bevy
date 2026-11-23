// SH-L2 自检（compute 单入口，第三段）：两条**不依赖均匀天空**的自检，结果写 stats
// 缓冲（由 BRP `ibl_stats` 回显，GPU 侧不再归约——最大值由读数端取）。
//   1. l=0 归一化：C00·Y00 应等于 π·mean(L)。均匀天空（`sky_uniform`）时严格相等，
//      误差就是投影归一化错（4π/N 或 A_l 折叠写错的直接证据）。
//   2. 求值对积分：6 轴向 + 8 对角共 14 个法线，E_sh(n) 与同一批样本上的暴力余弦积分
//      E_ref(n) = (4π/N)·Σ L·max(n·ω, 0) 的逐通道最大相对误差（L2 截断误差的量级）。
//
// 接口：b0 = radiance 缓存、b1 = SH 缓冲（已折叠 A_l）、b2 = stats（16 个 f32：
// [0]=l=0 相对误差、[1..15]=14 个法线的相对误差）。
// dispatch = 1（线程 0 做 l=0，线程 0..13 各做一个法线；无共享内存、无屏障）。
// #import ibl

const SH_SAMPLES: u32 = 1024u;

@group(0) @binding(0) var<storage, read> radiance: array<vec4f>;
@group(0) @binding(1) var<storage, read> sh_out: array<vec4f>;
@group(0) @binding(2) var<storage, read_write> stats: array<f32>;

// 14 个自检法线：0..5 = ±X/±Y/±Z，6..13 = 8 个对角方向。
fn check_normal(i: u32) -> vec3f {
    switch i {
        case 0u: { return vec3f(1.0, 0.0, 0.0); }
        case 1u: { return vec3f(-1.0, 0.0, 0.0); }
        case 2u: { return vec3f(0.0, 1.0, 0.0); }
        case 3u: { return vec3f(0.0, -1.0, 0.0); }
        case 4u: { return vec3f(0.0, 0.0, 1.0); }
        case 5u: { return vec3f(0.0, 0.0, -1.0); }
        default: {
            let k = i - 6u;
            let x = select(-1.0, 1.0, (k & 1u) != 0u);
            let y = select(-1.0, 1.0, (k & 2u) != 0u);
            let z = select(-1.0, 1.0, (k & 4u) != 0u);
            return normalize(vec3f(x, y, z));
        }
    }
}

// 相对误差（分母下限防 0/0）。
fn rel_err(got: f32, expect: f32) -> f32 {
    return abs(got - expect) / max(abs(expect), 1e-6);
}

fn rel_err_rgb(got: vec3f, expect: vec3f) -> f32 {
    return max(max(rel_err(got.r, expect.r), rel_err(got.g, expect.g)), rel_err(got.b, expect.b));
}

@compute @workgroup_size(16)
fn cs_main(@builtin(local_invocation_id) lid: vec3<u32>) {
    let t = lid.x;
    let uniform_weight = 4.0 * PI / f32(SH_SAMPLES);
    if (t == 0u) {
        var mean = vec3f(0.0);
        for (var i = 0u; i < SH_SAMPLES; i = i + 1u) {
            mean += radiance[i].rgb;
        }
        mean = mean / f32(SH_SAMPLES);
        // Y00 常量：C00·Y00 对 π·mean
        stats[0] = rel_err_rgb(sh_out[0].rgb * 0.28209479, mean * PI);
    }
    if (t < 14u) {
        let n = check_normal(t);
        let basis = sh_basis(n);
        var e_sh = vec3f(0.0);
        for (var c = 0u; c < 9u; c = c + 1u) {
            e_sh += sh_out[c].rgb * basis[c];
        }
        var e_ref = vec3f(0.0);
        for (var i = 0u; i < SH_SAMPLES; i = i + 1u) {
            // 样本方向与 sh_project 同序同值（同一 hammersley/uniform_sphere）
            let dir = uniform_sphere(hammersley(i, SH_SAMPLES));
            e_ref += radiance[i].rgb * max(dot(n, dir), 0.0);
        }
        e_ref = e_ref * uniform_weight;
        stats[1u + t] = rel_err_rgb(e_sh, e_ref);
    }
}
