// 8.3 CSM shadow depth pass 写入（depth-only）：把 cascade 光空间矩阵
// 乘到世界位置直出——无颜色输出、无材质（深度只写，light_depth 与主链同向
// reverse-Z：清 0 + GREATER，近=1/远=0）。
//
// 接口：
// - set0：实例表（b0 对象表 / b1 模型环，与 gbuffer_write 同一 set2 冻结表；
//   compute 布局里它排组 0——组号随管线布局走，5.2 首钉同款纪律）。
// - set1 b0：shadow UBO（四 cascade_view_proj + 平面族 + 分割带）。
// - push（var<immediate>，4B）：cascade 索引——每 cascade 一个 render pass
//   一次 indirect draw，矩阵经 UBO、本 push 只给选择面（cull 112B 同款
//   "帧级全局量走 push"的合规理由；逐对象 push 仍退役）。
//
// 并发语义：firstInstance = 实例槽号（5.3 退役钉的沿用），VS 按
// @builtin(instance_index) 直址读 set0——instance_index == 槽号。

struct ObjectRecord {
    first_index: u32,
    index_count: u32,
    vertex_base: u32,
    material_index: u32,
    aabb_min: vec4f,
    aabb_max: vec4f,
}

struct ShadowUniforms {
    cascade_view_proj: array<mat4x4f, 4>,
    planes: array<vec4f, 24>,
    splits: vec4f,
    camera_forward: vec4f,
    camera_pos: vec4f,
    map_size: vec4f,
}

@group(0) @binding(0) var<storage, read> objects: array<ObjectRecord>;
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
