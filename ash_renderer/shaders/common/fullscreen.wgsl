// 全屏三角形顶点入口（无顶点 buffer）：三个越界角点覆盖视口。
// uv.y 取 0.5 - p.y/2：naga 对 @builtin(position) 注入 Y 翻转（正高度 viewport
// 朝向的同一机制），翻转发生在 clip 写出、uv 不随之翻——图像坐标 y 向下，
// 顶行 ↔ shader-NDC y=+1 ↔ uv.y=0。

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) uv: vec2f,
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var p = array<vec2f, 3>(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
    var out: VsOut;
    out.clip = vec4f(p[vi], 0.0, 1.0);
    out.uv = vec2f(p[vi].x * 0.5 + 0.5, 0.5 - p[vi].y * 0.5);
    return out;
}
