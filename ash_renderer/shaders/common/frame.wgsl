// 每帧 UBO（272B）：字段序与 src/render/resident/tables/frame_uniform_table.rs 布局
// 镜像的 offset_of! 断言一致（0/64/80/96/112/116/120/128/192/208/224/240/256），
// 描述符 range = FRAME_UBO_SIZE。
// 只读前缀字段的 pass 也导入完整定义——绑定的 UBO 恒为 272B，range 足够。
struct FrameUniforms {
    view_proj: mat4x4f,
    // 表面到光方向（dir_to_light = transform.back()；w 未用），
    // 归一化与零向量兜底见 brdf.wgsl 的 safe_light_dir
    dir_to_light: vec4f,
    ambient_color: vec4f,
    // 方向光 rgb = 线性色 × illuminance（强度已折进 rgb）
    light_color: vec4f,
    mode: u32,          // FRAME_MODE_*（frame_uniform_table.rs 同值）
    sky_enabled: u32,   // 0 关 / 1 开（关 = 背景回清屏色）
    sky_intensity: f32, // 天空辐亮度缩放
    _pad: u32,
    // clip → world 反投影矩阵（深度重建世界位置）
    inv_view_proj: mat4x4f,
    // 相机世界位置（BRDF 的 V 向量）
    camera_pos: vec4f,
    // 天空参数（见 sky.wgsl）。vec3 各占一个 16B 槽，w 塞配套标量：
    sky_zenith: vec4f,  // xyz = 天顶色；w = 地平线→天顶过渡幂
    sky_horizon: vec4f, // xyz = 地平线色；w = 地平线→地面过渡幂
    sky_ground: vec4f,  // xyz = 地面色；w = 太阳角半径（弧度）
    // x = 太阳圆盘增益、y = 光晕强度、z = 光晕衰减幂、w 未用
    sky_sun: vec4f,
}
