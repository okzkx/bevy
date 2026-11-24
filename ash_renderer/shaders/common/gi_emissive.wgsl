// 9.6.2 自发光三角形表的记录定义（跨端契约）：生产者在
// `scene/gi/emissive_build.wgsl`（每帧重建），消费者是 `scene/gi/estimation.wgsl`
// 的面积采样。Rust 镜像见 `resident/tables/surface_cache.rs` 的 GI_EMISSIVE_TRIANGLE_SIZE。
// 只放结构定义与常量，不声明任何 @group/@binding。

// 一条自发光三角形（64B）。
struct EmissiveTriangle {
    // xyz = 世界空间第一个顶点；w = 三角形面积（平方米）。
    p0: vec4f,
    // xyz = v1 - v0。
    e1: vec4f,
    // xyz = v2 - v0。
    e2: vec4f,
    // rgb = 自发光辐亮度（因子 × 贴图，线性域）。
    emissive: vec4f,
}

// 表容量（超限截断；`counters[14]` 仍记到超限值）。
const GI_EMISSIVE_CAPACITY: u32 = 32768u;
