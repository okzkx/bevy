// 9.7.4 L1 压缩/解压（编解码一对，只在 1/4 屏幕目标上用）。
//
// 为什么压：L1 三项（l1x/l1y/l1z）的**方向**比颜色重要得多，而 `RGBA8` 只有 3 个可用通道，
// 装不下"三轴 × 三色"的 9 个标量。本段取"方向保真、颜色跟随 L0"的口径：
//   - 每轴先取 RGB 均值得到标量 `mean(l1_axis)`，再对它做归一化编码；
//   - 解码时把比值乘回 **L0 的颜色**（`l1_axis ≈ l0 · ratio`），于是 `E(n) = l0·(1 + Σ ratio·n)`
//     是一条颜色锁在 L0 上的方向项。
// 代价写在 9.7.4 施工记录里：间接光的**色向性**（例如红墙在绿面上的色渗方向差异）会被抹平；
// 若 9.7.5 读图看出不够，换成 RGBA16F 存三轴 RGB 即可（多 ≈2 MiB，计划 §4 的预算容得下）。
//
// 本工程已折基常数的系数满足 |L1| <= 2 L0（非负辐亮度投影后 A1·Y1²/(A0·Y0²)=2）。
// 每轴的合法比值因此在 [-2,2]，编码 e = ratio/4 + 0.5；不能照搬未折基常数的比例。
//
// L0 均值 → 0 时按 0.5（= 零向量）兜底防除零：该像素本就没有间接光，L0 那张的 rgb 也是 0。

const L1_ENCODE_SCALE: f32 = 0.25;

// 第四通道保存参考样本的 4×4 子像素编号；去噪保留中心编号，上采样据此读真实几何。
fn scgi_guide_offset(encoded: f32) -> vec2i {
    let sub = u32(round(clamp(encoded, 0.0, 1.0) * 15.0));
    return vec2i(i32(sub % 4u), i32(sub / 4u));
}

fn scgi_face_normal(n: vec3f, p: vec3f, camera: vec3f) -> vec3f {
    let unit = normalize(n);
    return select(unit, -unit, dot(unit, camera - p) < 0.0);
}

// 几何引导使用深度差分平面法线，不能拿 normal map 的微法线构造表面的真实切平面。
// 调用入口已导入 gi_cell；纹理/帧参数显式传入，不依赖公共片段内的 binding。
fn scgi_stable_guide_normal(
    depths: texture_2d<f32>, pixel: vec2i, full: vec2u, p: vec3f, depth: f32,
    fallback: vec3f, inv_view_proj: mat4x4f, camera: vec3f,
) -> vec3f {
    let last = vec2i(full) - vec2i(1);
    let uv = (vec2f(pixel) + vec2f(0.5)) / vec2f(full);
    let texel = vec2f(1.0) / vec2f(full);
    let n = flat_normal(
        p, uv, texel, depth,
        textureLoad(depths, clamp(pixel + vec2i(-1, 0), vec2i(0), last), 0).r,
        textureLoad(depths, clamp(pixel + vec2i(1, 0), vec2i(0), last), 0).r,
        textureLoad(depths, clamp(pixel + vec2i(0, 1), vec2i(0), last), 0).r,
        textureLoad(depths, clamp(pixel + vec2i(0, -1), vec2i(0), last), 0).r,
        inv_view_proj, camera,
    );
    if (dot(n, n) < 1e-8) {
        return scgi_face_normal(fallback, p, camera);
    }
    return n;
}

// 按真实世界位置到两侧切平面的距离拒绝跨层样本，而非拿窗口 reverse-Z 差冒充平面距离。
fn scgi_geometry_weight(p: vec3f, n: vec3f, q: vec3f, m: vec3f, camera: vec3f) -> f32 {
    let normal_weight = pow(clamp(dot(n, m), 0.0, 1.0), 8.0);
    let delta = q - p;
    let plane_distance = max(abs(dot(delta, n)), abs(dot(delta, m)));
    let scale = max(length(camera - p) * 0.01, 0.01);
    return normal_weight * exp(-plane_distance / scale);
}

// L0 判零门限：比它小的通道按"没有方向性"编码（避免除零放大噪声）。
const L1_ZERO_EPSILON: f32 = 1e-6;

// 三通道均值（方向的标量化）。
fn l1_mean_rgb(v: vec3f) -> f32 {
    return (v.x + v.y + v.z) / 3.0;
}

// 编码一轴：e = mean(l1)/mean(l0) / 4 + 0.5，clamp 到 [0,1]。
fn l1_encode_axis(l1_axis: vec3f, l0: vec3f) -> f32 {
    let l0_mean = l1_mean_rgb(l0);
    if (l0_mean <= L1_ZERO_EPSILON) {
        return 0.5;
    }
    let ratio = l1_mean_rgb(l1_axis) / l0_mean;
    return clamp(ratio * L1_ENCODE_SCALE + 0.5, 0.0, 1.0);
}

// 解码一轴的比值（相对 L0 的方向系数）。
fn l1_decode_ratio(encoded: f32) -> f32 {
    return (encoded - 0.5) / L1_ENCODE_SCALE;
}

// 解码一轴的颜色：比值乘回 L0 的颜色（颜色跟随 L0 的那条近似）。
fn l1_decode_axis(encoded: f32, l0: vec3f) -> vec3f {
    return l0 * l1_decode_ratio(encoded);
}

// SH 求值已折基常数和余弦卷积；反射 BRDF 的 albedo/π 仍由最终着色端另乘。
// 低阶 SH 在偏离采样半球时可产生负瓣，物理辐照度必须非负；保留有符号系数，求值后裁负。
fn scgi_evaluate(l0: vec3f, l1x: vec3f, l1y: vec3f, l1z: vec3f, n: vec3f) -> vec3f {
    return max(l0 + l1x * n.x + l1y * n.y + l1z * n.z, vec3f(0.0));
}
