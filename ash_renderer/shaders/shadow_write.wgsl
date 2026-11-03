// CSM shadow depth pass 写入（depth-only）：把 cascade 光空间矩阵乘到世界位置直出，无
// 颜色输出、无材质（深度与主链同向 reverse-Z：清 0 + GREATER，近=1/远=0）。
//
// 接口：
// - set0 b1：实例表的模型环（与 gbuffer_write 同一描述符集对象，组号见 cull.wgsl）。
// - set1 b0：shadow UBO（四 cascade_view_proj）。
// - push（var<immediate>，4B）：cascade 索引。每 cascade 一个 render pass 一次 indirect
//   draw，矩阵经 UBO，push 只选下标。
//
// firstInstance = 实例槽号，VS 按 @builtin(instance_index) 直址读模型环。
// #import shadow_ubo

@group(0) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(1) @binding(0) var<uniform> shadow: ShadowUniforms;

var<immediate> push: PushShadow;
struct PushShadow {
    cascade: u32,
}

@vertex
fn vs_main(
    @location(0) position: vec3f,
    @builtin(instance_index) instance_index: u32,
) -> @builtin(position) vec4f {
    let world = models[instance_index] * vec4f(position, 1.0);
    return shadow.cascade_view_proj[push.cascade] * world;
}

@fragment
fn fs_main() {}
