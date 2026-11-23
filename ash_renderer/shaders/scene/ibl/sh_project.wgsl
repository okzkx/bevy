// SH-L2 投影（compute 单入口，第一段）：N=1024 个球面均匀方向逐样本求环境辐亮度，
// 累积 9 系数 × RGB 的未缩放和，并把每个方向的 L 存进 radiance 缓存（自检用）。
// 每线程 SH_PER_THREAD 个方向，写 27 个浮点的 partial 行；归约与两条自检在
// sh_reduce.wgsl（同一次录制里紧接本 pass）。
//
// 接口：b0 = mip0 cube 采样（只用 mip0；显式级 0，锐利源）、b1 = 采样器、
// b2 = radiance 缓存（vec4/方向，w 未用）、b3 = partial 表（27×线程数）。
// dispatch = 1（单 workgroup；线程数也是 PARTIAL 行数，sh_reduce 按同值归约）。
// #import ibl

const SH_SAMPLES: u32 = 1024u;
const SH_THREADS: u32 = 256u;
const SH_PER_THREAD: u32 = SH_SAMPLES / SH_THREADS;

@group(0) @binding(0) var env_src: texture_cube<f32>;
@group(0) @binding(1) var env_samp: sampler;
@group(0) @binding(2) var<storage, read_write> radiance: array<vec4f>;
@group(0) @binding(3) var<storage, read_write> partial: array<f32>;

@compute @workgroup_size(256)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let tid = gid.x;
    if (tid >= SH_THREADS) {
        return;
    }
    // 函数作用域的 var 无初始化时按类型零值起步（WGSL 规范），27 个累加器直接开累
    var acc: array<f32, 27>;
    for (var k = 0u; k < SH_PER_THREAD; k = k + 1u) {
        let i = tid * SH_PER_THREAD + k;
        let dir = uniform_sphere(hammersley(i, SH_SAMPLES));
        let l = textureSampleLevel(env_src, env_samp, dir, 0.0).rgb;
        radiance[i] = vec4f(l, 0.0);
        let basis = sh_basis(dir);
        for (var c = 0u; c < 9u; c = c + 1u) {
            acc[c * 3u + 0u] += l.x * basis[c];
            acc[c * 3u + 1u] += l.y * basis[c];
            acc[c * 3u + 2u] += l.z * basis[c];
        }
    }
    for (var c = 0u; c < 27u; c = c + 1u) {
        partial[tid * 27u + c] = acc[c];
    }
}
