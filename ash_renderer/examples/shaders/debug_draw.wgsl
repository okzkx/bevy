// 3.4 正式绘制着色器（vertex + fragment 双入口）：FlightHelmet 调试几何的首条管线。
// 3.5.1/3.5.2 扩：FrameUniforms 64B→128B 灯光版，fragment 从 push 固定光换 UBO
// 数据驱动光照，并按 mode 三分支（材质策略 3.5 定案）。
//
// 接口 = 3.3 前置闸门冻结的 set/binding 表（与 descriptor_probe.wgsl 逐字段同源）：
// - set0 b0/b1：纹理数组 + sampler 数组，runtime array（build.rs 补丁器补 capability）。
// - set1 b0：每帧 UBO（FrameUniforms = 128B，偏移 0/64/80/96/112——与 Rust 镜像
//   `vulkan/descriptors.rs::FrameUniformsLayout`（offset_of! 静态互证）和描述符
//   range（FRAME_UBO_SIZE）三方一致）。
// - push constant：每 draw 参数，96B，偏移 0/64/68/80——与 Rust 镜像
//   `vulkan/pipeline.rs::PushLayout`（offset_of! 静态互证）和 pipeline layout 的
//   push constant range 三方一致。
//
// fragment 着色 = 材质三模式（UBO mode，与 Rust `FRAME_MODE_*` 同值）：
// - 0 Lambert（默认）：albedo × (direct + ambient) × exposure。物理链与 bevy GPU
//   侧同构：UBO 里的 light_color = linear × illuminance、ambient = linear ×
//   brightness（prepare_lights 同款），此处只补 Lambert 的 1/π 与曝光常量——
//   曝光 = exp2(-EV100)/1.2，EV100=9.7 为 bevy Exposure::BLENDER 默认。Burley
//   漫反射/镜面/IBL/tonemap 不属于 M2 判定（3.5 收官记录列明差异）。
// - 1 Unlit：albedo 直出（base color/UV/颜色空间对照；bevy 侧同款置 unlit）。
// - 2 Normal：世界法线可视化（rgb = n×0.5+0.5，法线方向验收仪器）。
// - vertex 做坐标变换（view_proj × model）；法线用 cofactor 矩阵——它与逆转置
//   只差行列式倒数这一个标量，normalize 后消失，免 WGSL 无 inverse() 内建的限制，
//   非均匀缩放正确（见 normal_cofactor 注释）。
// - sRGB 解码在采样端（贴图格式是 SRGB），编码在输出端（swapchain 的 SRGB view，
//   ROP 写出自动做）——着色器全程线性，编码恰好一次（纹理格式与Gamma篇 §0）。
//
// push 索引在一个 draw 内对全部调用一致（push 恒定），不触发 NonUniform；跨 draw
// 的差异由"每次 draw 前重推"表达。mip 采样钉 level 0（静态 mip0，与采样器
// max_lod=0 双保险）。
enable wgpu_binding_array;

struct FrameUniforms {
    view_proj: mat4x4f,
    // 表面到光方向（bevy GPU 同款：dir_to_light = light transform.back()）。
    // w 未用。模式 0 专用；归一化在 shader 端防御性再做一次。
    dir_to_light: vec4f,
    ambient_color: vec4f,
    light_color: vec4f,
    mode: u32,
}

struct PushParams {
    model: mat4x4f,
    tex_index: u32,
    sampler_index: u32,
    base_color: vec4f,
}

// 与 Rust 侧 FRAME_MODE_* 同值（descriptors.rs）
const MODE_LAMBERT: u32 = 0u;
const MODE_UNLIT: u32 = 1u;
const MODE_NORMAL: u32 = 2u;

// Lambert 漫反射的 1/π（能量归一；bevy Fd_Burley 的 Lambert 部分同款）
const INV_PI: f32 = 0.3183099;
// bevy 默认曝光（Exposure::BLENDER）：EV100=9.7，exposure = exp2(-9.7)/1.2 = 0.0010019。
// 20000 lx + 80 cd/m² 的物理量收敛到可见亮度，与官方视亮度同源。
const EXPOSURE: f32 = 0.0010019;

@group(0) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(0) @binding(1) var samplers: binding_array<sampler>;
@group(1) @binding(0) var<uniform> frame: FrameUniforms;

var<immediate> push: PushParams;

// 模型的逆转置法线矩阵（cofactor 矩阵形态）：设 mat3 列为 a,b,c，则
// (M⁻¹)ᵀ = cofactor(M)/det(M)，而 cofactor 列 = [b×c, c×a, a×b]。
// det 标量在 normalize 中消失——不用除法、不引入 inverse()，非均匀缩放正确；
// det=0（退化缩放）不在资产假设内（3.4 面板验证条款只要求非恒等/非均匀）。
fn normal_cofactor(m: mat4x4f) -> mat3x3f {
    let a = m[0].xyz;
    let b = m[1].xyz;
    let c = m[2].xyz;
    return mat3x3f(cross(b, c), cross(c, a), cross(a, b));
}

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) world_normal: vec3f,
    @location(1) uv: vec2f,
}

@vertex
fn vs_main(
    @location(0) position: vec3f,
    @location(1) normal: vec3f,
    @location(2) uv: vec2f,
) -> VsOut {
    var out: VsOut;
    // 列主序全链：model(local→world) 后 view_proj(world→clip)，与 Rust 侧
    // Mat4::to_cols_array / get_clip_from_view × view 的乘序同一约定。
    // 朝向定案（3.4 施工记录 §viewport）：bevy 投影（glam directx 变体，Y-up NDC、
    // Z[0,1]）在本机 Vulkan 上用**正高度** viewport 直出即与官方 wgpu 渲染同向；
    // 负高度翻转反而颠倒（E5 光栅级实验 + 官方示例对照截图实证）。
    out.clip = frame.view_proj * (push.model * vec4f(position, 1.0));
    out.world_normal = normal_cofactor(push.model) * normal;
    out.uv = uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let albedo =
        textureSampleLevel(textures[push.tex_index], samplers[push.sampler_index], in.uv, 0.0)
            * push.base_color;
    let n = normalize(in.world_normal);
    // 材质三模式（3.5 定案）：unlit / normal / Lambert（默认，数据驱动）。
    if frame.mode == MODE_UNLIT {
        return vec4f(albedo.rgb, albedo.a);
    }
    if frame.mode == MODE_NORMAL {
        return vec4f(n * 0.5 + vec3f(0.5), 1.0);
    }
    // Lambert（mode == MODE_LAMBERT 及一切未知值的兜底）：UBO 方向光 + 环境。
    // dir_to_light 防御性归一化：零向量 normalize 会出 NaN 且 NaN×0≠0，先择回退向。
    let l_raw = frame.dir_to_light.xyz;
    let l = select(vec3f(0.0, 1.0, 0.0), normalize(l_raw), dot(l_raw, l_raw) > 1e-8);
    let lambert = max(dot(n, l), 0.0);
    let direct = frame.light_color.rgb * lambert * INV_PI;
    let ambient = frame.ambient_color.rgb;
    return vec4f(albedo.rgb * (direct + ambient) * EXPOSURE, albedo.a);
}
