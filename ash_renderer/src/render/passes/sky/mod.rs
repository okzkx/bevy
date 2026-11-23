//! 程序化天空：天空参数资源（[`SkySettings`]），天空函数本体在
//! `shaders/common/sky.wgsl`。本模块不持有 GPU 资源——合成 pass 的背景分支每帧
//! 从帧 UBO 取参求值，环境 cubemap 捕获（9.2）复用同一份参数。
//!
//! 参数是每帧常量，走帧 UBO 的成熟纪律（每帧槽一份、整块重写）；宿主侧唯一真源
//! 是本资源，天空面板与 BRP `sky_set` 写同一份。

use bevy::prelude::Resource;

/// 程序化天空参数。颜色是线性域辐亮度系数，乘 [`Self::intensity`] 后落到
/// pre-exposure 域（曝光乘算在合成 pass 末尾，与直接光同域）。
#[derive(Resource)]
pub struct SkySettings {
    /// 天空开关：关 = 背景原样回清屏色（与无天空基线逐位一致）。
    pub enabled: bool,
    /// 天顶色。
    pub zenith: [f32; 3],
    /// 地平线色（`dir.y = 0` 两侧的共同色，保证地平线无缝）。
    pub horizon: [f32; 3],
    /// 地面色（`dir.y < 0`）。
    pub ground: [f32; 3],
    /// 整体辐亮度缩放。
    pub intensity: f32,
    /// 地平线→天顶过渡幂：越小越快切到天顶色，越大越贴着地平线。
    pub horizon_exponent: f32,
    /// 地平线→地面过渡幂。
    pub ground_exponent: f32,
    /// 太阳圆盘角半径（弧度）。
    pub sun_radius: f32,
    /// 太阳圆盘增益（乘方向光颜色，与直接光照强度解耦）。
    pub sun_gain: f32,
    /// 太阳光晕强度（乘方向光颜色，0 = 关）。
    pub glow_intensity: f32,
    /// 太阳光晕角衰减幂：越大光晕越紧。
    pub glow_falloff: f32,
}

impl Default for SkySettings {
    fn default() -> Self {
        // 默认梯度按 pre-exposure 域校准：地平线辐亮度 ≈ 0.4×2000，乘 EXPOSURE 与
        // tonemap 的肩部压缩后仍是淡蓝，天顶明显更深更蓝。坑：光晕强度以方向光颜色
        // （≈ illuminance，量级 1e4）为单位，0.1 级就会把整片天空刷白。
        Self {
            enabled: true,
            zenith: [0.06, 0.14, 0.42],
            horizon: [0.40, 0.50, 0.72],
            ground: [0.08, 0.075, 0.07],
            intensity: 2000.0,
            horizon_exponent: 0.5,
            ground_exponent: 1.5,
            sun_radius: 0.01,
            sun_gain: 1.5,
            glow_intensity: 0.02,
            glow_falloff: 6.0,
        }
    }
}
