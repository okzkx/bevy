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
// 归一化分母取 `l0 · 4/√3`：RGBl1 里 `|l1·n| ≤ l0`，三轴均分时 `|l1|` 最大到 `l0·4/√3 ≈
// 2.31·l0`——取这个倍数才能保证编码落在 [0,1] 而不被截断。编码 `e = ratio·(√3/4) + 0.5`。
//
// L0 均值 → 0 时按 0.5（= 零向量）兜底防除零：该像素本就没有间接光，L0 那张的 rgb 也是 0。

const L1_ENCODE_SCALE: f32 = 0.43301270189;

// L0 判零门限：比它小的通道按"没有方向性"编码（避免除零放大噪声）。
const L1_ZERO_EPSILON: f32 = 1e-6;

// 三通道均值（方向的标量化）。
fn l1_mean_rgb(v: vec3f) -> f32 {
    return (v.x + v.y + v.z) / 3.0;
}

// 编码一轴：`e = mean(l1)/mean(l0) · √3/4 + 0.5`，clamp 到 [0,1]。
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

// 像素辐照度求值：E(n) = l0 + (l1x·n.x + l1y·n.y + l1z·n.z)。
// 基常数与余弦卷积都已折在存储值里——消费端不要再乘 1/π。
fn scgi_evaluate(l0: vec3f, l1x: vec3f, l1y: vec3f, l1z: vec3f, n: vec3f) -> vec3f {
    return l0 + l1x * n.x + l1y * n.y + l1z * n.z;
}
