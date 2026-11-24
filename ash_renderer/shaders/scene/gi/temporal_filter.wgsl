// 9.7.3 patch 级时域后滤波（compute）：把 9.7.2 的空间滤波输出（b14）与**上一帧**的时域
// 输出（b15）做指数混合，结果仍写 b15。公式与 Unity 的 `TemporalFiltering` 同形：
//   w = lerp(0.9, 0.95, temporal_smoothing)，B = lerp(A, B_prev, w)
// 即 B 主要保留上一帧（权重 0.9～0.95），只吸收 5%～10% 的本帧新值。
//
// 与 estimation 的 ping-pong 的关系：**两套**。estimation 的帧奇偶决定它写 b3 还是 b4
// （辐照度双份），本文件的 b14/b15 有固定角色（空间输出 / 时域输出），不参与那个奇偶——
// 复用同一索引会读到"上一帧的上一帧"。
//
// 首次出现的 patch 直取 A：`update_count <= 1` 说明它本帧才刚拿到第一个估计值，b15 里那个
// 槽还是上一条 patch（或全零）的残留，混合会把别人的历史抹进来。
//
// 线程 = 池容量（每线程一条 patch，空槽直接返回）。
//
// 接口：set0 帧 UBO（只用集序占位）；set1 GI 表（b5 统计 / b6 patch→cell / b9 GI UBO /
// b14 空间输出 / b15 时域输出）；push 24B。
// #import frame
// #import gi_cell

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(5) var<storage, read> patch_statistics: array<PatchStatistics>;
@group(1) @binding(6) var<storage, read> patch_cell: array<u32>;
@group(1) @binding(9) var<uniform> gi: GiUniforms;
@group(1) @binding(14) var<storage, read> filtered_irradiance0: array<PatchIrradiance>;
@group(1) @binding(15) var<storage, read_write> filtered_irradiance1: array<PatchIrradiance>;

// push 布局与 `src/render/passes/gi/filtering.rs` 的 `pack_temporal_filter_push` 一一对应。
struct PushTemporalFilter {
    // 0 = 直拷（B = A）；1 = 时域混合。
    enabled: u32,
    // 池容量（防越界）。
    capacity: u32,
    // 帧号（读数与调试用，公式本身不依赖它）。
    frame_index: u32,
    // 0 = 直取 A 的更新次数门槛之上的 patch 直取 A 例外；保留位。
    reserved: u32,
    // 平滑度 [0,1]：w = lerp(0.9, 0.95, smoothing)。
    smoothing: f32,
    // 对齐填充（保持 24B）。
    pad0: f32,
}
var<immediate> push: PushTemporalFilter;

// 上一帧权重的下限/上限（Unity 同值）。
const TEMPORAL_WEIGHT_MIN: f32 = 0.9;
const TEMPORAL_WEIGHT_MAX: f32 = 0.95;

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let slot = gid.x;
    if (slot >= push.capacity) {
        return;
    }
    if (patch_cell[slot] == INVALID_PATCH) {
        return;
    }
    let source = filtered_irradiance0[slot];
    var out: PatchIrradiance = source;
    let statistics = patch_statistics[slot];
    // 刚拿到第一个估计值的 patch 直取 A（b15 里是别人的历史）
    if (push.enabled != 0u && statistics.update_count > 1u) {
        let w = clamp(
            mix(TEMPORAL_WEIGHT_MIN, TEMPORAL_WEIGHT_MAX, push.smoothing),
            TEMPORAL_WEIGHT_MIN,
            TEMPORAL_WEIGHT_MAX,
        );
        let previous = filtered_irradiance1[slot];
        out.l0 = mix(source.l0, previous.l0, vec4f(w));
        out.l1x = mix(source.l1x, previous.l1x, vec4f(w));
        out.l1y = mix(source.l1y, previous.l1y, vec4f(w));
        out.l1z = mix(source.l1z, previous.l1z, vec4f(w));
    }
    filtered_irradiance1[slot] = out;
}
