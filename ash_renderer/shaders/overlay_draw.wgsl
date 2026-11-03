// overlay 绘制着色器（vertex + fragment 双入口）：egui 镶嵌顶点直出调试 UI。
//
// 接口 = 常驻表 set0 + overlay 专用 16B push（UI 无 UBO，没有 set1）：
// - set0 b0/b1：纹理数组 + sampler 数组，runtime array（build.rs 补丁器补
//   capability），与 gbuffer_write.wgsl 同一布局。
// - push constant：整帧 UI 一次的 16B，偏移 0/8/12，须与
//   src/render/passes/overlay/overlay_pipeline.rs 的 UiPushLayout（offset_of! 断言）
//   和管线的 push 范围一致。
//
// 色彩（与 egui 官方 wgpu 后端一致，目标是视觉相同而非理论最优）：
// - 顶点色与图集采样值都在 gamma 域且已预乘，直接相乘。
// - rgb 做 sRGB 解码、alpha 原样输出：写 SRGB view 时编码一次，混合落在线性域。
//   图集是 R8G8B8A8_UNORM，采样拿到的就是 gamma 值。
// - 不做 dithering；mip 固定 level 0。
//
// 坐标：pos 与 screen_size 都是 points（左上原点、y 向下），按 Y-up NDC 写 clip；
// Vulkan 侧的 Y 翻转由 naga ADJUST_COORDINATE_SPACE 注入（与场景管线同一路径，
// 只翻一次）。
enable wgpu_binding_array;

struct PushParams {
    screen_size: vec2f,
    tex_index: u32,
    sampler_index: u32,
}

@group(0) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(0) @binding(1) var samplers: binding_array<sampler>;

var<immediate> push: PushParams;

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) uv: vec2f,
    // gamma 域预乘 sRGBA（Color32 字节经 UNORM 属性解成 0..1）
    @location(1) color: vec4f,
}

@vertex
fn vs_main(
    @location(0) pos: vec2f,
    @location(1) uv: vec2f,
    @location(2) color: vec4f,
) -> VsOut {
    var out: VsOut;
    // 左上原点的 points → Y-up NDC（Vulkan 翻转见文件头）
    out.clip = vec4f(
        pos.x * 2.0 / push.screen_size.x - 1.0,
        1.0 - pos.y * 2.0 / push.screen_size.y,
        0.0,
        1.0,
    );
    out.uv = uv;
    out.color = color;
    return out;
}

// sRGB 解码：≤0.04045 走 /12.92 线性段，否则 ((x+0.055)/1.055)^2.4
fn srgb_to_linear(rgb: vec3f) -> vec3f {
    return select(rgb / 12.92, pow((rgb + 0.055) / 1.055, vec3f(2.4)), rgb > vec3f(0.04045));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    // 两侧都在 gamma 域且已预乘，直接相乘
    let rgba = in.color * textureSampleLevel(
        textures[push.tex_index],
        samplers[push.sampler_index],
        in.uv,
        0.0,
    );
    return vec4f(srgb_to_linear(rgba.rgb), rgba.a);
}
