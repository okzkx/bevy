// 每帧 UBO（208B）：与 src/render/assets/descriptors.rs 的 FrameUniformsLayout
// offset_of! 静态断言同序（0/64/80/96/112/128/192），描述符 range = FRAME_UBO_SIZE。
// 只读前缀字段的 pass 也导入完整定义——绑定的 UBO 恒为 208B，range 足够。
struct FrameUniforms {
    view_proj: mat4x4f,
    // 表面到光方向（bevy 同款 dir_to_light = transform.back()；w 未用），
    // 归一化与零向量兜底见 brdf.wgsl 的 safe_light_dir
    dir_to_light: vec4f,
    ambient_color: vec4f,
    // 方向光 rgb = 线性色 × illuminance（强度已折进 rgb）
    light_color: vec4f,
    mode: u32,  // FRAME_MODE_*（descriptors.rs 同值）
    // clip → world 反投影矩阵（深度重建世界位置）
    inv_view_proj: mat4x4f,
    // 相机世界位置（BRDF V 向量取数）
    camera_pos: vec4f,
}
