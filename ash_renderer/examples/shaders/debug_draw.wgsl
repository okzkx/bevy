// 3.4 正式绘制着色器（vertex + fragment 双入口）：FlightHelmet 调试几何的首条管线。
//
// 接口 = 3.3 前置闸门冻结的 set/binding 表（与 descriptor_probe.wgsl 逐字段同源）：
// - set0 b0/b1：纹理数组 + sampler 数组，runtime array（build.rs 补丁器补 capability）。
// - set1 b0：每帧 UBO（FrameUniforms = 64B mat4 view_proj；3.5 扩灯光字段）。
// - push constant：每 draw 参数，96B，偏移 0/64/68/80——与 Rust 镜像
//   `vulkan/pipeline.rs::PushLayout`（offset_of! 静态互证）和 pipeline layout 的
//   push constant range 三方一致。
//
// 着色内容 = M2 不透明调试策略（3.4 面板口径）：
// - vertex 做坐标变换（view_proj × model）；法线用 cofactor 矩阵——它与逆转置
//   只差行列式倒数这一个标量，normalize 后消失，免 WGSL 无 inverse() 内建的限制，
//   非均匀缩放正确（见 normal_cofactor 注释）。
// - fragment 按 push 索引采样 base color 贴图 × 材质基色（线性域），调试方向光
//   Lambert + 环境（光照对照归 3.5，此处只为让法线/深度可判读）。
// - sRGB 解码在采样端（贴图格式是 SRGB），编码在输出端（swapchain 的 SRGB view，
//   ROP 写出自动做）——着色器全程线性，编码恰好一次（纹理格式与Gamma篇 §0）。
//
// push 索引在一个 draw 内对全部调用一致（push 恒定），不触发 NonUniform；跨 draw
// 的差异由"每次 draw 前重推"表达。mip 采样钉 level 0（静态 mip0，与采样器
// max_lod=0 双保险）。
enable wgpu_binding_array;

struct FrameUniforms {
    view_proj: mat4x4f,
}

struct PushParams {
    model: mat4x4f,
    tex_index: u32,
    sampler_index: u32,
    base_color: vec4f,
}

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
    // 调试光照：固定世界空间方向光 + 环境（3.5 换成 UBO 数据并分项对照）
    let n = normalize(in.world_normal);
    let l = normalize(vec3f(0.5, 1.0, 0.3));
    let lambert = max(dot(n, l), 0.0);
    return vec4f(albedo.rgb * (0.25 + 0.75 * lambert), albedo.a);
}
