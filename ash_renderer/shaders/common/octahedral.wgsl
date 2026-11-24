// 八面体球↔方映射与 4×4 方向桶索引。GlobalProbe（9.3）的投影与着色求值共用；
// 9.5 起 cell/patch 的 16 桶直接 import 同一份，不要复制改改变量名。
//
// 与 Unity `Common.hlsl:161-182` / `GlobalProbe.hlsl` 同式；分量序统一 x/y/z
//（Unity 的 `RGBL1` 是 y/z/x，别照抄）。本片段自带 clamp，不依赖 `brdf.wgsl` 的
// `saturate`——被不引 brdf 的 entry 导入时也能单独编译。

fn oct_wrap(v: vec2f) -> vec2f {
    return (1.0 - abs(v.yx)) * select(vec2f(-1.0), vec2f(1.0), v >= vec2f(0.0));
}

// 方向（单位向量）→ 方图坐标 [0,1]²。零向量按极小分母兜底，避免 NaN 扩散。
fn octahedral_sphere_to_square(n_in: vec3f) -> vec2f {
    let denominator = max(abs(n_in.x) + abs(n_in.y) + abs(n_in.z), 1e-20);
    let n = n_in / denominator;
    let xy = select(n.xy, oct_wrap(n.xy), n.z < 0.0);
    return xy * 0.5 + 0.5;
}

fn octahedral_square_to_sphere(f: vec2f) -> vec3f {
    let g = f * 2.0 - 1.0;
    var n = vec3f(g.x, g.y, 1.0 - abs(g.x) - abs(g.y));
    let t = clamp(-n.z, 0.0, 1.0);
    n.x += select(t, -t, n.x >= 0.0);
    n.y += select(t, -t, n.y >= 0.0);
    return normalize(n);
}

// 方向 → 桶索引（行主序 y*res + x）。与 Unity 同式：方图坐标先乘 res、截断，
// 再钳 res−1；乘 res−1 会把桶边界压歪，方向与桶不再互逆。
fn direction_to_bucket_index(n: vec3f, res: u32) -> u32 {
    let square = octahedral_sphere_to_square(n);
    let p = min(vec2u(square * f32(res)), vec2u(res - 1u));
    return p.y * res + p.x;
}

// 桶索引 → 桶中心方向（(x+0.5, y+0.5)/res 反投影）。
fn bucket_index_to_direction(index: u32, res: u32) -> vec3f {
    let p = vec2f(f32(index % res), f32(index / res));
    return octahedral_square_to_sphere((p + vec2f(0.5)) / f32(res));
}
