// CSM shadow depth pass 写入：把 cascade 光空间矩阵乘到世界位置直出，颜色输出无；深度与
// 主链同向 reverse-Z（清 0 + GREATER，近=1/远=0）。MASK 材质在片段端按贴图 alpha 裁切，
// 镂空物体的阴影形状与可见面一致。
//
// 接口：
// - set0：实例表（b0 对象表 / b1 模型环 / b2 材质环；与 gbuffer_write 同一描述符集对象）。
// - set1 b0：shadow UBO（四 cascade_view_proj）。
// - set2 b0/b1：纹理数组 + sampler 数组（MASK 裁切采样 base_color）。
// - push（var<immediate>，4B）：cascade 索引。每 cascade 一个 render pass 一次 indirect
//   draw，矩阵经 UBO，push 只选下标。
//
// firstInstance = 实例槽号，VS 按 @builtin(instance_index) 直址读模型环；材质索引经 flat
// varying 带到 FS（fragment 阶段没有 instance_index）。
enable wgpu_binding_array;
// #import instance
// #import shadow_ubo

@group(0) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(0) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(0) @binding(2) var<storage, read> materials: array<MaterialRecord>;
@group(1) @binding(0) var<uniform> shadow: ShadowUniforms;
@group(2) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(2) @binding(1) var samplers: binding_array<sampler>;

var<immediate> push: PushShadow;
struct PushShadow {
    cascade: u32,
}

struct ShadowOut {
    @builtin(position) clip: vec4f,
    @location(0) uv: vec2f,
    @location(1) @interpolate(flat) material: u32,
}

@vertex
fn vs_main(
    @location(0) position: vec3f,
    @location(2) uv: vec2f,
    @builtin(instance_index) instance_index: u32,
) -> ShadowOut {
    let world = models[instance_index] * vec4f(position, 1.0);
    var out: ShadowOut;
    out.clip = shadow.cascade_view_proj[push.cascade] * world;
    out.uv = uv;
    out.material = objects[instance_index].material_index;
    return out;
}

@fragment
fn fs_main(in: ShadowOut) {
    let mat = materials[in.material];
    if (mat.alpha_mode == ALPHA_MODE_MASK) {
        let alpha = textureSampleLevel(
            textures[mat.tex_base_color], samplers[mat.sampler_index], in.uv, 0.0).a
            * mat.base_color.a;
        if (alpha < mat.alpha_cutoff) {
            discard;
        }
    }
}
