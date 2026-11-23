// 物理天空（单次散射）：沿视线积分大气散射，返回辐亮度（线性、pre-exposure 域）。
// 口径照 Unity HDRP Physically Based Sky：三介质（瑞利/米氏/臭氧）+ Chapman 解析
// 透射率 + 受光的地面反照率。本实现不做多次散射（无 LUT），太阳背侧的暗部与
// 暮光会比参考实现偏暗——见《9.1.6 物理天空》施工记录。
//
// 不声明绑定：导入方须声明全局 `frame: FrameUniforms`（参数全在帧 UBO）。
// include_sun：可见天空传 true；9.2 环境 cubemap 捕获传 false——只排除太阳圆盘，
// 太阳附近的米氏前向散射仍留在捕获里（HDRP 的 cubemap 烘焙同口径）。
// #import frame
// #import brdf

// 主视线积分步数（HDRP 的 SkyView LUT 也用 16 步）。逐步成本见施工记录。
const SKY_STEPS: u32 = 16u;
// 臭氧光学深度的沿线样本数（HDRP 的 2 样本近似）
const SKY_OZONE_SAMPLES: u32 = 2u;

// 某高度处三种介质的系数（全按"纯散射"处理：散射 = 消光；臭氧只吸收）。
struct MediumSample {
    rayleigh: vec3f,
    mie: vec3f,
    ozone: vec3f,
    extinction: vec3f,
}

// 三角臭氧剖面：中心最厚，两侧 width/2 处为 0（与 HDRP 的 OzoneDensity 同形状）。
fn sky_ozone_density(height: f32) -> f32 {
    let center = frame.sky_geo.z;
    let half_width = max(frame.sky_geo.w * 0.5, 1.0);
    return max(0.0, 1.0 - abs(height - center) / half_width);
}

fn sky_sample_medium(height: f32) -> MediumSample {
    let r = frame.sky_rayleigh.rgb * exp(-height / max(frame.sky_rayleigh.w, 1.0));
    let m = vec3f(frame.sky_mie.x * exp(-height / max(frame.sky_mie.y, 1.0)));
    let o = frame.sky_ozone.rgb * sky_ozone_density(height);
    return MediumSample(r, m, o, r + m + o);
}

fn sky_rayleigh_phase(cos_theta: f32) -> f32 {
    return (3.0 / (16.0 * PI)) * (1.0 + cos_theta * cos_theta);
}

// Cornette-Shanks 米氏相函数（正本：render-pipelines.core 的 VolumeRendering.hlsl）。
// g>0 时在 cos_theta=1（正对太阳）出前向峰——这是太阳周围亮区的来源。
fn sky_mie_phase(g: f32, cos_theta: f32) -> f32 {
    let gg = g * g;
    let k = (3.0 / (8.0 * PI)) * (1.0 - gg) / (2.0 + gg);
    let x = max(1.0 + gg - 2.0 * g * cos_theta, 1e-4);
    return k * (1.0 + cos_theta * cos_theta) * pow(x, -1.5);
}

// 球求交的稳健形式：先除半径再开方，避免两个大数相减损失精度。
// 返回 (近交点, 远交点)；无交返回负值。
fn sky_intersect_sphere(sphere_radius: f32, cos_chi: f32, r: f32) -> vec2f {
    let ratio = sphere_radius / max(r, 1.0);
    let d = ratio * ratio - saturate(1.0 - cos_chi * cos_chi);
    if (d < 0.0) {
        return vec2f(-1.0, -1.0);
    }
    let s = sqrt(d);
    return r * vec2f(-cos_chi - s, -cos_chi + s);
}

// Chapman 近似（HDRP 同式）：z = r / 标高。
fn sky_chapman_upper(z: f32, cos_theta: f32) -> f32 {
    let c = cos_theta;
    let n = 0.761643 * ((1.0 + 2.0 * z) - (c * c * z));
    let d = c * z + sqrt(z * (1.47721 + 0.273828 * (c * c * z)));
    return 0.5 * c + n / max(d, 1e-6);
}

fn sky_chapman_horizontal(z: f32) -> f32 {
    let r = inverseSqrt(max(z, 1e-6));
    let s = z * r;
    return 0.626657 * (r + 2.0 * s);
}

// 沿射线的臭氧光学深度：把射线裁到臭氧层的两个球壳之间取 2 个样本，
// 乘 0.6 修正（HDRP 的 ComputeOzoneOpticalDepth 同式）。
fn sky_ozone_optical_depth(r: f32, cos_theta: f32, dist_along_ray: f32) -> f32 {
    let planet_r = frame.sky_geo.x;
    let center = frame.sky_geo.z;
    let half = max(frame.sky_geo.w * 0.5, 1.0);
    let t_inner = sky_intersect_sphere(planet_r + max(center - half, 0.0), cos_theta, r);
    let t_outer = sky_intersect_sphere(planet_r + center + half, cos_theta, r);

    var t_entry = 0.0;
    var t_exit = 0.0;
    var t_entry2 = 0.0;
    var t_exit2 = 0.0;
    if (t_inner.x < 0.0 && t_inner.y >= 0.0) {
        // 起点在臭氧层下方：从下层交点入射，到外层出射
        t_entry = t_inner.y;
        t_exit2 = t_outer.y;
        t_entry2 = (t_exit2 - t_entry) * 0.5;
        t_exit = t_entry2;
    } else {
        // 起点在层内或层上：先到下层（打中）或外层出射
        t_entry = max(t_outer.x, 0.0);
        t_exit = select(t_outer.y, t_inner.x, t_inner.x >= 0.0);
        if (t_inner.x >= 0.0 && dist_along_ray > t_inner.y) {
            // 穿过下层再回来，第二段
            t_entry2 = t_inner.y;
            t_exit2 = t_outer.y;
        } else {
            t_exit2 = t_exit;
            t_entry2 = (t_exit2 - t_entry) * 0.5;
            t_exit = t_entry2;
        }
    }
    t_exit = min(t_exit, dist_along_ray);
    t_exit2 = min(t_exit2, dist_along_ray);

    var optical_depth = 0.0;
    let dt = max(t_exit - t_entry, 0.0) * 0.5;
    let dt2 = max(t_exit2 - t_entry2, 0.0) * 0.5;
    for (var i = 0u; i < SKY_OZONE_SAMPLES; i = i + 1u) {
        let f = (f32(i) + 0.5) * 0.5;
        let t = mix(t_entry, t_exit, f);
        let t2 = mix(t_entry2, t_exit2, f);
        // 高度 = sqrt(r² + t(2r·cosθ + t)) − R（余弦定理）
        let h = sqrt(max(r * r + t * (2.0 * r * cos_theta + t), 0.0)) - planet_r;
        let h2 = sqrt(max(r * r + t2 * (2.0 * r * cos_theta + t2), 0.0)) - planet_r;
        optical_depth += sky_ozone_density(h) * dt + sky_ozone_density(h2) * dt2;
    }
    return optical_depth * 0.6;
}

// 到大气顶的光学深度（RGB）：瑞利/米氏走 Chapman 解析，臭氧走样本积分。
// above_horizon=false 表示射线被地面截断（只积到地面遮挡点）。
fn sky_optical_depth(r: f32, cos_theta: f32, above_horizon: bool) -> vec3f {
    let planet_r = frame.sky_geo.x;
    let h_r = max(frame.sky_rayleigh.w, 1.0);
    let h_m = max(frame.sky_mie.y, 1.0);
    let z = vec2f(r / h_r, r / h_m);
    let z0 = vec2f(planet_r / h_r, planet_r / h_m);
    let sin_theta = sqrt(saturate(1.0 - cos_theta * cos_theta));
    var ch = vec2f(
        sky_chapman_upper(z.x, abs(cos_theta)) * exp(z0.x - z.x),
        sky_chapman_upper(z.y, abs(cos_theta)) * exp(z0.y - z.y),
    );
    if (!above_horizon) {
        // 视线落地：从地面遮挡点起算
        let sin_gamma = (r / planet_r) * sin_theta;
        let cos_gamma = sqrt(saturate(1.0 - sin_gamma * sin_gamma));
        let ch2 = vec2f(
            sky_chapman_upper(z0.x, cos_gamma),
            sky_chapman_upper(z0.y, cos_gamma),
        );
        ch = ch2 - ch;
    } else if (cos_theta < 0.0) {
        // 太阳在该点地平线附近/以下：走下半球公式（HDRP 同式）
        let zc = z * sin_theta;
        let b = exp(z0 - zc);
        let a = 2.0 * vec2f(sky_chapman_horizontal(zc.x), sky_chapman_horizontal(zc.y));
        ch = a * b - ch;
    }
    let ozone = sky_ozone_optical_depth(r, cos_theta, 1e9);
    return ch.x * h_r * frame.sky_rayleigh.rgb
        + ch.y * h_m * vec3f(frame.sky_mie.x)
        + ozone * frame.sky_ozone.rgb;
}

fn sky_radiance(dir: vec3f, include_sun: bool) -> vec3f {
    let planet_r = max(frame.sky_geo.x, 1.0);
    let atmos_r = max(frame.sky_geo.y, planet_r + 1.0);
    // 海拔约定：海平面 = 世界 y = 0（相机在山谷/地下时按 0 兜底）
    let r0 = planet_r + max(frame.camera_pos.y, 0.0);
    let up = vec3f(0.0, 1.0, 0.0);
    let cos_chi = dir.y;

    let t_atmos = sky_intersect_sphere(atmos_r, cos_chi, r0);
    if (t_atmos.y < 0.0) {
        return vec3f(0.0);
    }
    var t_end = t_atmos.y;
    let t_ground = sky_intersect_sphere(planet_r, cos_chi, r0);
    var hit_ground = false;
    if (t_ground.x > 1e-3 && t_ground.x < t_end) {
        t_end = t_ground.x;
        hit_ground = true;
    }

    let sun_dir = safe_light_dir(frame.dir_to_light.xyz);
    var radiance = vec3f(0.0);

    if (hit_ground) {
        // 地平线以下：行星表面，Lambert 反射太阳（不含天空照地的二次项）
        let p = up * r0 + dir * t_end;
        let n = normalize(p);
        let cos_sun = dot(n, sun_dir);
        if (cos_sun > 0.0) {
            let t_sun = exp(-sky_optical_depth(planet_r, cos_sun, true));
            radiance = frame.light_color.rgb * frame.sky_ground.rgb
                * (cos_sun / PI) * t_sun;
        }
    } else {
        // 视线在大气内：单次散射积分（段内按均匀介质解析积分）
        var transmittance = vec3f(1.0);
        let cos_phase = dot(dir, sun_dir);
        for (var s = 0u; s < SKY_STEPS; s = s + 1u) {
            // 二次分布采样：近端密、远端疏（HDRP 的 GetSample 同式）
            let f0 = f32(s) / f32(SKY_STEPS);
            let f1 = f32(s + 1u) / f32(SKY_STEPS);
            let t0 = f0 * f0 * t_end;
            let t1 = f1 * f1 * t_end;
            let t = (t0 + t1) * 0.5;
            let dt = t1 - t0;

            let p = up * r0 + dir * t;
            let r = max(length(p), planet_r);
            let height = max(r - planet_r, 0.0);
            let n = p / r;
            let medium = sky_sample_medium(height);
            let seg_t = exp(-medium.extinction * dt);
            let cos_sun = dot(n, sun_dir);
            let t_sun = exp(-sky_optical_depth(r, cos_sun, true));
            let phase = sky_rayleigh_phase(cos_phase) * medium.rayleigh
                + sky_mie_phase(frame.sky_mie.z, cos_phase) * medium.mie;
            let source = frame.light_color.rgb * t_sun * phase;
            let sigma = max(medium.extinction, vec3f(1e-12));
            radiance += transmittance * (source - source * seg_t) / sigma;
            transmittance *= seg_t;
        }
    }

    if (include_sun) {
        // 太阳圆盘：解析项，不进散射积分。按角度软化边缘（低分辨率下硬边会闪）
        let cos_theta = clamp(dot(dir, sun_dir), -1.0, 1.0);
        let theta = acos(cos_theta);
        let radius = max(frame.sky_sun.y, 1e-4);
        let disc = 1.0 - smoothstep(radius * 0.8, radius, theta);
        radiance += frame.light_color.rgb * (frame.sky_sun.x * disc);
    }
    return radiance * frame.sky_intensity;
}
