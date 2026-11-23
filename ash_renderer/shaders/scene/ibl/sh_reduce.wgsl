// SH-L2 归约（compute 单入口，第二段）：把 partial 表按 SH_THREADS 行求和，乘 4π/N
// 并把余弦卷积的 A_l 折叠进系数，写 9×vec4 的 sh 缓冲。两条自检在 sh_check.wgsl
// （同一次录制里紧随本 pass；这里不跨线程交换，因此不碰 workgroup 共享内存）。
//
// 接口：b0 = partial 表、b1 = SH 缓冲（9×vec4，xyz = RGB，w=0）。
// dispatch = 1（前 9 个线程各认领一个系数；线程数取 32）。
// #import ibl

const SH_THREADS: u32 = 256u;

@group(0) @binding(0) var<storage, read> partial: array<f32>;
@group(0) @binding(1) var<storage, read_write> sh_out: array<vec4f>;

@compute @workgroup_size(32)
fn cs_main(@builtin(local_invocation_id) lid: vec3<u32>) {
    let c = lid.x;
    if (c >= 9u) {
        return;
    }
    // partial 行布局：[系数 × 3 + 通道]，与 sh_project 同序
    var sum = vec3f(0.0);
    for (var i = 0u; i < SH_THREADS; i = i + 1u) {
        let base = i * 27u + c * 3u;
        sum += vec3f(partial[base], partial[base + 1u], partial[base + 2u]);
    }
    let scale = sh_band_factor(c) * (4.0 * PI / 1024.0);
    sh_out[c] = vec4f(sum * scale, 0.0);
}
