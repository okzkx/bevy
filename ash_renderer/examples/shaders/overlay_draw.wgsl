// 3.7.3 overlay 绘制着色器（vertex + fragment 双入口）：egui 镶嵌顶点直出调试 UI。
//
// 接口 = 3.3 冻结的 set0 表 + overlay 专用 16B push（无 set1——UI 无 UBO）：
// - set0 b0/b1：纹理数组 + sampler 数组，runtime array（build.rs 补丁器补
//   capability），与 debug_draw.wgsl 同一契约；overlay 管线 layout 只声明 set0
//   （"表跨管线共享"的兑现位，布局=未来管线公共 ABI）。
// - push constant：整帧 UI 一次的 16B，偏移 0/8/12——与 Rust 镜像
//   `vulkan/overlay_pipeline.rs::UiPushLayout`（offset_of! 静态互证）和 pipeline
//   layout 的 push constant range 三方一致。
//
// 色彩约定（判据 = egui 官方 wgpu 后端 0.36.2 源码，施工计划 §2.2 钉死）：
// - 顶点色与图集采样值都在 gamma 域且已预乘，相乘不拆预乘（官方同款非理想
//   数学；保真目标 = 与官方 egui 视觉一致，不是理论最优线性预乘）。
// - rgb 做 gamma→linear（sRGB 解码曲线），alpha 原样带过 → ROP（SRGB view）
//   编码恰好一次，blend 落在线性域——与场景管线"着色器全程线性、编码只在
//   输出端"同构。图集是 R8G8B8A8_UNORM（非 sRGB 采样解码），gamma 值原样
//   采进来。
// - 官方 shader 的 dithering 与可预测滤波分支不搬（项目全管线无 dithering，
//   显式边界）。
// - mip 钉 level 0（图集静态 mip0，与采样器 max_lod=0 双保险，debug_draw 同款）。
//
// NDC：pos 是物理像素（左上原点、y 向下；tessellate 已按 ppp 把 points 换算
// 成像素），照 egui-wgpu 的 webgpu 约定（Y-up）写 pos→clip，screen_size 同为
// 物理像素；naga ADJUST_COORDINATE_SPACE 注入的 gl_Position.y 翻转照旧
// 生效（与 debug_draw 同一编译路径，翻转恰好一次）。
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
    // gamma 预乘 sRGBA（Color32 字节经 UNORM 属性解包 /255）
    @location(1) color: vec4f,
}

@vertex
fn vs_main(
    @location(0) pos: vec2f,
    @location(1) uv: vec2f,
    @location(2) color: vec4f,
) -> VsOut {
    var out: VsOut;
    // egui-wgpu 同款公式：左上原点（y 向下）的逻辑点 → webgpu Y-up NDC；
    // Vulkan 侧翻转由 naga 注入承担（与场景管线同一路径，恰好一次）
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

// sRGB 解码（egui-wgpu fs_main_linear_framebuffer 同款曲线）：
// ≤0.04045 走 12.92 线性段，否则 (x+0.055)/1.055 的 2.4 幂
fn srgb_to_linear(rgb: vec3f) -> vec3f {
    return select(rgb / 12.92, pow((rgb + 0.055) / 1.055, vec3f(2.4)), rgb > vec3f(0.04045));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    // 两侧都在 gamma 域且已预乘：乘法不拆预乘（官方保真，计划 §2.2）
    let rgba = in.color * textureSampleLevel(
        textures[push.tex_index],
        samplers[push.sampler_index],
        in.uv,
        0.0,
    );
    // rgb gamma→linear、alpha 直过：ROP 编码恰好一次，blend 在线性域
    return vec4f(srgb_to_linear(rgba.rgb), rgba.a);
}
