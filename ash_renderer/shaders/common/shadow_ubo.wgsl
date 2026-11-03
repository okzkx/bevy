// shadow UBO（736B）：字段序与 src/render/passes/shadow/cascades.rs 的
// ShadowUniformsLayout 对照，尺寸以 SHADOW_UBO_SIZE 为准。
struct ShadowUniforms {
    cascade_view_proj: array<mat4x4f, 4>,
    // 4 cascade × 6 视锥平面（384B 超 push 上限，故走 UBO）
    planes: array<vec4f, 24>,
    splits: vec4f,
    camera_forward: vec4f,
    camera_pos: vec4f,
    // x = map_size（PCSS texel 归一）、y = enabled 旗标（0/1）
    map_size: vec4f,
    // 逐级联 texel 世界边长（米）/ 1÷光向盒深（米 → NDC 深度）——bias、
    // 法线抬升、半影都按米算再换算，换级联不失真
    texel_world: vec4f,
    inv_span: vec4f,
}
