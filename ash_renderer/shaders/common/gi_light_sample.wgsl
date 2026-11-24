// 9.6.2 光源侧采样清单的记录定义（跨端契约）：生产者在
// `scene/gi/light_sampling.wgsl`，消费者是 `scene/gi/estimation.wgsl` 的重连接取样。
// Rust 镜像见 `resident/tables/surface_cache.rs` 的 `GI_LIGHT_SAMPLE_SIZE`（64B）。
// 只放结构定义与常量，不声明任何 @group/@binding。

// 一条光源侧采样（64B）。
struct LightSample {
    // xyz = 命中点世界位置；w = 灯到命中点的距离（米）。
    hit_pos: vec4f,
    // xyz = 命中点法线（已翻到与射线同侧）；w = 1/pdf。
    hit_normal: vec4f,
    // 命中点反照率（base color × 贴图）。
    albedo: vec4f,
    // x = 灯号（光源紧凑表下标）；y = 1 有效 / 0 无效；zw 保留。
    // 名字不用 meta：WGSL 把 meta 列为保留关键字。
    info: vec4u,
}

// 全向点光的采样密度：1/(4π · 灯数)，记录里存的是它的倒数。
const GI_SPHERE_PDF_4PI: f32 = 12.566370614359172;
