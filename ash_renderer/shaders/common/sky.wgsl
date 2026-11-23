// 程序化天空：天顶—地平线—地面解析渐变 + 太阳圆盘 + 光晕。返回辐亮度（线性、
// pre-exposure 域），曝光乘算由调用方负责。不声明绑定：导入方须声明全局
// `frame: FrameUniforms`（参数全在帧 UBO，见 frame.wgsl）。
//
// include_sun：可见天空传 true；IBL 捕获传 false——太阳圆盘是高频亮斑，进 SH/
// 预滤波会污染低频项（Unity 的 _SUNDISK_NONE 同目的）。
// #import frame
// #import brdf

fn sky_radiance(dir: vec3f, include_sun: bool) -> vec3f {
    // 两个 pow 都在 dir.y = 0 处退到 horizon 色：地平线两侧连续，不出亮线
    let up = saturate(dir.y);
    let down = saturate(-dir.y);
    var c = mix(frame.sky_horizon.rgb, frame.sky_zenith.rgb, pow(up, frame.sky_zenith.w));
    c = mix(c, frame.sky_ground.rgb, pow(down, frame.sky_horizon.w));
    c = c * frame.sky_intensity;
    if (!include_sun) {
        return c;
    }
    let sun_dir = safe_light_dir(frame.dir_to_light.xyz);
    let cos_theta = dot(dir, sun_dir);
    // 圆盘按角度软化边缘（低分辨率下硬边会闪）：0.8r 内全亮，r 外全灭
    let radius = frame.sky_ground.w;
    let theta = acos(clamp(cos_theta, -1.0, 1.0));
    let disc = 1.0 - smoothstep(radius * 0.8, radius, theta);
    // 光晕：太阳附近一个低频项，给地平线附近方向感
    let glow = pow(saturate(cos_theta), frame.sky_sun.z);
    // 太阳色复用方向光（rgb 已乘 illuminance），圆盘增益/光晕强度单独调，与直接光
    // 照强度解耦；无方向光时 light_color 全零，圆盘与光晕自然消失
    return c + frame.light_color.rgb * (frame.sky_sun.x * disc + frame.sky_sun.y * glow);
}
