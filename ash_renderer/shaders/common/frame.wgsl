// 每帧 UBO（336B）：字段序与 src/render/resident/tables/frame_uniform_table.rs 布局
// 镜像的 offset_of! 断言一致（0/64/80/96/112/116/120/128/192/208/224/240/256/272/288/304/308/312/320），
// 描述符 range = FRAME_UBO_SIZE。
// 只读前缀字段的 pass 也导入完整定义——绑定的 UBO 恒为 336B，range 足够。
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
    sky_intensity: f32, // 天空整体亮度倍数（标定旋钮）
    _pad: u32,
    // clip → world 反投影矩阵（深度重建世界位置）
    inv_view_proj: mat4x4f,
    // 相机世界位置（BRDF 的 V 向量；天空的相机海拔取 y）
    camera_pos: vec4f,
    // 物理天空参数（见 sky.wgsl）。vec3 各占一个 16B 槽，w 塞配套标量：
    sky_rayleigh: vec4f, // xyz = 海平面瑞利散射系数(1/m)；w = 瑞利标高(m)
    sky_mie: vec4f,      // x = 海平面米氏散射系数(1/m)；y = 米氏标高(m)；z = 各向异性 g；w 未用
    sky_ozone: vec4f,    // xyz = 臭氧吸收系数(1/m)；w 未用
    sky_geo: vec4f,      // x = 行星半径(m)；y = 大气顶半径(m)；z = 臭氧层中心高(m)；w = 臭氧层全宽(m)
    sky_ground: vec4f,   // xyz = 地面反照率；w 未用
    sky_sun: vec4f,      // x = 太阳圆盘增益；y = 太阳角半径(rad)；zw 未用
    ibl_enabled: u32,    // 0 关 / 1 开（关 = albedo · ambient_color，步骤 8 原文）
    ibl_mode: u32,       // 漫反射编码：0 = SH-L2（9.3 的 1 = 16 桶）
    ibl_intensity: f32,  // IBL 强度倍数
    _pad2: u32,
    // 常量天空（9.2.0 自检开关）：xyz = 辐亮度；w = 1 开 / 0 关。
    // 开时 sky_radiance 直接返回 rgb（不受 sky_intensity 影响，自检锚点须精确）。
    sky_uniform: vec4f,
    // 9.7.6 SCGI 注入：x = 开关（0 关 / 1 开）；y = 预留质量档；zw 保留。
    scgi: vec4u,
}
