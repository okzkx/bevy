// 校色链的"曲线输入"公共片段：Bloom 叠加 + 自动曝光倍数。
//
// present_tonemap（真正出图）与 color_scope（9.9 颜色校验面板的采样）必须用同一条
// 式子，否则面板里的 B 域就不是 tonemap 真正吃到的颜色。本文件只放结构/常量/纯函数，
// @group/@binding 留在各入口——`build.rs` 的 `// #import` 在入口处的指令位置文本展开。
//
// 口径（与 src/render/passes/post/color_scope.rs 的 bins 说明一致）：
//   B 域 = (hdr + bloom * strength) * exposure_scale
// 乘法顺序刻意与 9.9 抽取前的 present fs_main 相同（先乘 bloom 强度、再加 hdr、
// 最后乘曝光倍数），保证抽取前后同一帧逐位一致。不含曲线，也不含 `frame.mode`
// 的调试直出分支——那些由调用方决定。

// Bloom 参数 UBO（32B，前 6 个 f32 有效；字段序见 bloom.rs 的 BloomParamsLayout）。
struct BloomParams {
    strength: f32,
    enabled: f32,
    tonemap: f32,
    curve: f32,
    gain: f32,
    // 校色强度（0…=1）：曲线末端 mix，1 = 完整曲线（默认），0 = 曝光后线性钳位。
    tonemap_strength: f32,
}

// 曝光状态 buffer 的第 0 个 vec4f：(adapted_log2, scale, avg_log2, 1)。
// scale <= 0 表示该槽还没被曝光段写过（或调试显示模式的中性状态），按 1 处理。
fn exposure_scale_of(state: vec4f) -> f32 {
    return select(1.0, state.y, state.y > 0.0);
}

// 曲线输出与"曝光后线性钳位"按校色强度混合。
// `strength >= 1` 时直接返回曲线结果——`mix(x, y, 1) = y` 在浮点里并不逐位成立，
// 而默认 1.0 必须与立案基线逐位一致（判定线⑧）。
fn apply_tonemap_strength(linear: vec3f, mapped: vec3f, params: BloomParams) -> vec3f {
    let t = clamp(params.tonemap_strength, 0.0, 1.0);
    if (t >= 1.0) {
        return mapped;
    }
    return mix(clamp(linear, vec3f(0.0), vec3f(1.0)), mapped, t);
}

// 曲线输入色。`bloom` 是 Bloom 采样值（未乘强度），`params` 提供叠加系数。
fn tonemap_input_color(
    hdr: vec3f,
    bloom: vec3f,
    params: BloomParams,
    exposure_scale: f32,
) -> vec3f {
    return (hdr + bloom * params.strength) * exposure_scale;
}
