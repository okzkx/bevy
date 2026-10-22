// 3.3.3 描述符链路探针着色器：形状 = 前置闸门冻结的正式 set/binding 表。
//
// - set0 b0/b1：纹理数组 + sampler 数组（WGSL texture/sampler 分离），本版不带
//   长度 = runtime array（SPIR-V OpTypeRuntimeArray；build.rs 补丁器补
//   RuntimeDescriptorArray capability，见前置闸门施工记录 §4 定案）。
// - set1 b0：每帧 UBO（FrameUniforms = 64B mat4）。
// - push constant：每 draw 参数（model/tex_index/sampler_index/base_color，
//   96B，偏移 0/64/68/80，与 Rust 镜像 offset_of! 互证）。
// - set2：探针专用 I/O（每调用 storage 索引 + 输出）——**不属于生产表**，仅给
//   compute 验证提供非一致下标来源与读回出口；生产管线 layout 只含 set0/set1+push。
//
// 两条采样路径：push 索引（一致）与每调用 storage 索引（非一致，naga 自动发
// OpDecorate NonUniform）。build.rs 以本文件 → OUT_DIR/descriptor_probe.spv。
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

struct DrawIndices {
    tex: array<u32, 2>,
    samp: array<u32, 2>,
}

@group(0) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(0) @binding(1) var samplers: binding_array<sampler>;
@group(1) @binding(0) var<uniform> frame: FrameUniforms;
@group(2) @binding(0) var<storage, read> draw_indices: DrawIndices;
@group(2) @binding(1) var<storage, read_write> out: array<vec4f>;

var<immediate> push: PushParams;

@compute @workgroup_size(2, 1, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    // 引用 frame 防"声明即裁剪"歧义：乘 0 不改变采样坐标（uniform 内容运行时值，不会被折叠）
    let uv = vec2f(0.3, 0.5) + vec2f(frame.view_proj[0][0] * 0.0, 0.0);
    // 路径一：push 索引（一致路径）→ out[0]
    out[0] = textureSampleLevel(
        textures[push.tex_index],
        samplers[push.sampler_index],
        uv,
        0.0,
    );
    // 路径二：每调用 storage 索引（非一致路径）→ out[1+gid.x]
    let i = gid.x;
    out[1u + i] = textureSampleLevel(
        textures[draw_indices.tex[i]],
        samplers[draw_indices.samp[i]],
        uv,
        0.0,
    );
}
