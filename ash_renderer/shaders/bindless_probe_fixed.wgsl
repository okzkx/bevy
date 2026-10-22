// 3.3 前置闸门小样例（定长版）：与 runtime 版同形状，唯一差异是数组带定长 4。
//
// 定长 binding_array → SPIR-V OpTypeArray（非 OpTypeRuntimeArray），不需要
// runtimeDescriptorArray 特性——它是 naga 原生 runtime 输出若被 spirv-val 拒绝时
// 的备选路线证据（选型记录的切换评估素材），也是"更窄特性集即可跑"的对照组。
// 采样语义、NonUniform 预期、字段偏移与 runtime 版完全一致。
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

@group(0) @binding(0) var textures: binding_array<texture_2d<f32>, 4>;
@group(0) @binding(1) var samplers: binding_array<sampler, 4>;
@group(1) @binding(0) var<uniform> frame: FrameUniforms;
@group(2) @binding(0) var<storage, read> draw_indices: DrawIndices;
@group(2) @binding(1) var<storage, read_write> out: array<vec4f>;

var<immediate> push: PushParams;

@compute @workgroup_size(2, 1, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let uv = vec2f(0.3, 0.5) + vec2f(frame.view_proj[0][0] * 0.0, 0.0);
    out[0] = textureSampleLevel(
        textures[push.tex_index],
        samplers[push.sampler_index],
        uv,
        0.0,
    );
    let i = gid.x;
    out[1u + i] = textureSampleLevel(
        textures[draw_indices.tex[i]],
        samplers[draw_indices.samp[i]],
        uv,
        0.0,
    );
}
