//! 环境光遮蔽（10.2）：半分辨率 SSAO + 深度感知上采样，AO 只乘环境项。
//!
//! 关档（[`SsaoSettings::enabled`] 为 `false`）时不录 SSAO 段，合成 pass 绑 1×1 白
//! dummy，AO 乘 1.0 逐字节恒等；管线与集序始终不变。
//!
//! | 文件 | 内容 |
//! |---|---|
//! | `mod.rs` | 设置与模块出口 |
//! | `ssao_pass.rs` | 两条 compute 管线与录制束 |
//! | `ssao_targets.rs` | 半/全分辨率 AO 图、关档 dummy、描述符集 |

use bevy::prelude::Resource;

pub mod ssao_pass;
pub mod ssao_targets;

/// SSAO 设置（BRP `ssao_set` 与 overlay 面板共用同一份）。
#[derive(Resource)]
pub struct SsaoSettings {
    /// 总开关。默认关：关档时画面与没接 SSAO 逐字节一致。
    pub enabled: bool,
    /// 采样半球半径（米）。采样在世界空间做，所以是场景尺度而非屏幕像素。
    pub radius_m: f32,
    /// 深度偏置（米）：比这更近的遮挡算自遮挡，忽略。
    pub bias: f32,
    /// 强度：`ao = saturate(1.0 - intensity * occlusion)`。
    pub intensity: f32,
    /// 半球内的步数（4 个方向 × steps 个样本）。
    pub steps: u32,
    /// 全分辨率诊断模式：AO 直接按全分辨率算、跳过上采样。
    ///
    /// 只给判定线 4 的「半 vs 全分辨率」对照与读图取证用；默认关，正常渲染走
    /// 半分辨率 + 双边上采样。关档时该项无意义。
    pub full_res: bool,
}

impl Default for SsaoSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            radius_m: 0.5,
            bias: 0.02,
            intensity: 1.0,
            steps: 4,
            full_res: false,
        }
    }
}
