//! 物理天空：大气参数资源（[`SkySettings`]），散射积分本体在
//! `shaders/common/sky.wgsl`。本模块不持有 GPU 资源——合成 pass 的背景分支每帧
//! 从帧 UBO 取参求值，环境 cubemap 捕获（9.2）复用同一份参数。
//!
//! 默认值照 Unity HDRP Physically Based Sky 的 Earth 默认（见字段注释），
//! 单位一律 SI（米、1/米、弧度）。参数是每帧常量，走帧 UBO 的成熟纪律；
//! 宿主侧唯一真源是本资源，天空面板与 BRP `sky_set` 写同一份。

use bevy::prelude::Resource;

/// 物理天空参数。海平面约定为世界 `y = 0`，相机海拔取自 `camera_pos.y`。
#[derive(Resource)]
pub struct SkySettings {
    /// 天空开关：关 = 背景原样回清屏色（与无天空基线逐位一致）。
    pub enabled: bool,
    /// 天空整体亮度倍数。**标定旋钮**：2 万 lux 输入的物理天空比旧渐变压暗数倍，
    /// 默认值按现有后链（EXPOSURE + ACES）标到同一显示区间，不是物理量。
    pub intensity: f32,
    /// 行星半径（米）。
    pub planet_radius: f32,
    /// 大气壳厚度（米）：HDRP 取 `8000 / 0.144765 ≈ 55_266`（密度衰减到千分之一）。
    pub atmosphere_height: f32,
    /// 海平面瑞利散射系数（RGB，1/米；纯散射）。
    pub rayleigh_scattering: [f32; 3],
    /// 瑞利标高（米）。
    pub rayleigh_scale_height: f32,
    /// 海平面米氏散射系数（1/米；纯散射，灰）。
    pub mie_scattering: f32,
    /// 米氏标高（米）。
    pub mie_scale_height: f32,
    /// 米氏各向异性 g（0 = 各向同性，越接近 1 前向峰越窄）。
    pub mie_anisotropy: f32,
    /// 臭氧吸收系数（RGB，1/米）。
    pub ozone_absorption: [f32; 3],
    /// 臭氧层中心高度（米）。
    pub ozone_center: f32,
    /// 臭氧层全宽（米，三角剖面：中心最厚、两侧为零）。
    pub ozone_width: f32,
    /// 行星地面反照率（RGB，Lambert）。
    pub ground_albedo: [f32; 3],
    /// 太阳圆盘增益（乘方向光颜色）。
    pub sun_gain: f32,
    /// 太阳圆盘角半径（弧度）。
    pub sun_radius: f32,
}

impl Default for SkySettings {
    fn default() -> Self {
        // HDRP Earth 默认：瑞利 5.8/13.5/33.1e-6 @680/550/440nm、标高 8000；
        // 米氏 10e-6、标高 1200、g 0.8；臭氧 0.65/1.88/0.08e-6，20–40 km。
        // 两处偏离 HDRP 默认：intensity 见字段注释；地面反照率取中性灰（HDRP 的
        // 0.12/0.10/0.09 是地球土色，在本工程的灰色场景里像一条褐色带）。
        Self {
            enabled: true,
            intensity: 2.0,
            planet_radius: 6.3781e6,
            atmosphere_height: 55_266.0,
            rayleigh_scattering: [5.8e-6, 13.5e-6, 33.1e-6],
            rayleigh_scale_height: 8000.0,
            mie_scattering: 10e-6,
            mie_scale_height: 1200.0,
            mie_anisotropy: 0.8,
            ozone_absorption: [0.65e-6, 1.88e-6, 0.08e-6],
            ozone_center: 30_000.0,
            ozone_width: 20_000.0,
            ground_albedo: [0.25, 0.25, 0.25],
            sun_gain: 1.5,
            sun_radius: 0.01,
        }
    }
}
