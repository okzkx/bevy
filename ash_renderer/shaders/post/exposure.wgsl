// 自动曝光（眼适应）——独立的显示段模块，**不属于 SCGI**：它只读 HDR 中间目标，
// 输出一个全帧统一的曝光倍数，由 present tonemap 在色调映射前乘上去。GI / IBL 的
// 物理辐照度、`common/brdf.wgsl` 的 EXPOSURE 锚点、`BloomSettings::gain` 都不因为
// 这个模块而改变；关闭时（mode = 0）倍数恒 1，画面与没有本模块逐像素相同。
//
// 两段 dispatch（同一描述符集，管线不同 entry）：
//   ① cs_measure：把 HDR 按 EXPOSURE_TILES 网格切成 tile，每 tile 采 4×4 个点，
//      写入 log2(luma) 的 tile 均值到 measures[]（长度 = tile 数）。
//      取对数是为了让"少数很亮的窗/灯"不会把整帧曝光压死（几何平均的口径）。
//   ② cs_adapt：单线程读满 measures[]，得到本帧 avg_log2；与上一帧的适应值做
//      指数逼近（亮适应快、暗适应慢，与人眼一致），再由 key 反解曝光倍数：
//          scale = clamp(key / 2^adapted, min_scale, max_scale)
//      结果写 state[0] = (adapted_log2, scale, avg_log2, 1.0)。w = 1 表示已初始化。
//
// 时域：state 按帧槽各一份（与 UBO 同一 slot），所以相邻两帧不会同时读改写同一份；
// 代价是同一 slot 的值两帧才更新一次，`rate_*` 已按这个节奏取值，不需要外部补偿。
//
// 采样数固定为 64×36 tile × 4×4 = 36864 个点（1600×900 的 2.6%），与分辨率无关；
// 平均量本来就不需要全像素，且固定采样数让 cost 与窗口大小脱钩。

const LUMINANCE: vec3f = vec3f(0.2126, 0.7152, 0.0722);
// 线性亮度下限：HDR 里大片真零（未受光的室内）取 log 会发散，钳到这里相当于 -20 EV，
// 已经远低于 8-bit 显示下限，不会影响可观察画面。
const LUMINANCE_FLOOR: f32 = 1.0e-6;
const TILE_SAMPLES: f32 = 16.0;

struct ExposureParams {
    // 0 = 手动（scale 恒 1，本模块只旁路不改画面）/ 1 = 自动。
    mode: u32,
    tiles_x: u32,
    tiles_y: u32,
    _pad0: u32,
    // 目标中灰（线性亮度口径）：2^adapted 应等于它时 scale = 1。
    key: f32,
    min_scale: f32,
    max_scale: f32,
    // 适应速率（1/s）：画面变暗（需要提高曝光）用 rate_brighten，
    // 画面变亮（需要压低曝光）用 rate_darken。
    rate_brighten: f32,
    rate_darken: f32,
    dt: f32,
    _pad1: vec2f,
};

@group(0) @binding(0) var<uniform> params: ExposureParams;
@group(0) @binding(1) var hdr_tex: texture_2d<f32>;
@group(0) @binding(2) var<storage, read_write> measures: array<f32>;
@group(0) @binding(3) var<storage, read_write> state: array<vec4f>;

/// 逐 tile 的 log2 亮度均值。线程数 = tile 数（不足一个 workgroup 也照发）。
@compute @workgroup_size(64)
fn cs_measure(@builtin(global_invocation_id) gid: vec3u) {
    let tiles = params.tiles_x * params.tiles_y;
    let index = gid.x;
    if (index >= tiles) {
        return;
    }
    let dim = vec2i(textureDimensions(hdr_tex));
    let max_coord = max(dim - vec2i(1), vec2i(0));
    let tile_x = index % params.tiles_x;
    let tile_y = index / params.tiles_x;
    var acc = 0.0;
    for (var j = 0u; j < 4u; j = j + 1u) {
        for (var i = 0u; i < 4u; i = i + 1u) {
            // tile 内 4×4 的格心 → 归一化 uv → 全分辨率像素
            let u = (f32(tile_x) + (f32(i) + 0.5) * 0.25) / f32(params.tiles_x);
            let v = (f32(tile_y) + (f32(j) + 0.5) * 0.25) / f32(params.tiles_y);
            let coord = clamp(
                vec2i(i32(u * f32(dim.x)), i32(v * f32(dim.y))),
                vec2i(0),
                max_coord,
            );
            let luma = dot(textureLoad(hdr_tex, coord, 0).rgb, LUMINANCE);
            // NaN / 负数（不该出现，但别让它污染整帧曝光）落到下限
            let safe = select(LUMINANCE_FLOOR, luma, luma > 0.0);
            acc = acc + log2(max(safe, LUMINANCE_FLOOR));
        }
    }
    measures[index] = acc / TILE_SAMPLES;
}

/// 全帧适应 + 求曝光倍数。单线程：tile 数固定 2304，一次循环的开销可以忽略。
@compute @workgroup_size(1)
fn cs_adapt() {
    let tiles = params.tiles_x * params.tiles_y;
    var sum = 0.0;
    for (var i = 0u; i < tiles; i = i + 1u) {
        sum = sum + measures[i];
    }
    let avg_log = sum / f32(tiles);

    let previous = state[0];
    var adapted = avg_log;
    // w > 0.5 = 上一帧写过（首帧直接吸附，避免从 0 慢慢爬）
    if (previous.w > 0.5) {
        let brightening = avg_log > previous.x;
        let rate = select(params.rate_darken, params.rate_brighten, brightening);
        let blend = clamp(1.0 - exp(-rate * max(params.dt, 0.0)), 0.0, 1.0);
        adapted = mix(previous.x, avg_log, blend);
    }

    let scale = clamp(
        params.key / exp2(adapted),
        params.min_scale,
        params.max_scale,
    );
    state[0] = vec4f(adapted, select(1.0, scale, params.mode != 0u), avg_log, 1.0);
}
