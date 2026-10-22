// 3.3 前置闸门小样例（runtime 版）：两张纹理 + 索引，naga 30.0.1 wgsl-in/spv-out。
//
// 形状即冻结草案（3.3 README 前置闸门③）：
// - set0 b0/b1：纹理数组 + sampler 数组（WGSL texture/sampler 分离，非 combined）。
//   本版不带长度 = runtime array（SPIR-V OpTypeRuntimeArray，须 runtimeDescriptorArray）。
// - set1 b0：每帧 UBO。
// - push constant：每 draw 参数（model/tex_index/sampler_index/base_color，96B）。
// - set2：每调用各自索引的 storage 输入（逼出 NonUniform）+ 采样输出。
//
// 两条采样路径：push 索引（全 dispatch 一致，不该发 NonUniform）与每调用 storage
// 索引（非一致，naga 须自动发 OpDecorate NonUniform——VUID-RuntimeSpirv-NonUniform-06274）。
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
