//! 场景着色模式：帧 UBO `mode` 字段的宿主侧形态与唯一名称表。
//!
//! 资源由 `AshHostPlugin` 插入（初值取 env `ASH_RENDER_MODE`），overlay 单选钮与 BRP
//! `set_render_mode` 写、`draw_frame` 读；UI 文本、BRP 参数、env 拼写、日志共用
//! [`RenderMode::label`] 这一份名称表。

use bevy::prelude::*;

use crate::render::{
    FRAME_MODE_AO, FRAME_MODE_DEPTH, FRAME_MODE_LAMBERT, FRAME_MODE_METALLIC, FRAME_MODE_NORMAL,
    FRAME_MODE_POSITION, FRAME_MODE_REFLECTANCE, FRAME_MODE_ROUGHNESS, FRAME_MODE_UNLIT,
};

/// 场景着色模式（UBO `mode` 的宿主侧形态）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Resource)]
pub enum RenderMode {
    /// Filament BRDF 光照（默认）。名称是 filament，"lambert" 保留为兼容别名。
    Lambert,
    /// 直出 albedo，用来检查 base color/UV/颜色空间。
    Unlit,
    /// 世界法线可视化。
    Normal,
    /// 世界位置可视化（G-buffer 深度反投影重建）。
    Position,
    /// matparam 单通道灰度直出（metallic/roughness/AO/reflectance 各一档），用来检查材质热改。
    Metallic,
    Roughness,
    AmbientOcclusion,
    Reflectance,
    /// reverse-Z 深度线性化灰度。
    Depth,
}

impl RenderMode {
    /// 全部模式（名称解析遍历用）。
    pub const ALL: [Self; 9] = [
        Self::Lambert,
        Self::Unlit,
        Self::Normal,
        Self::Position,
        Self::Metallic,
        Self::Roughness,
        Self::AmbientOcclusion,
        Self::Reflectance,
        Self::Depth,
    ];

    /// 可选名称提示（env 拼写错误告警与 BRP 未知模式报错共用）。
    pub const NAME_HINT: &'static str =
        "filament(lambert 兼容)/unlit/normal/position/metallic/roughness/ao/reflectance/depth";

    /// 模式名（唯一名称表）。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Lambert => "filament",
            Self::Unlit => "unlit",
            Self::Normal => "normal",
            Self::Position => "position",
            Self::Metallic => "metallic",
            Self::Roughness => "roughness",
            Self::AmbientOcclusion => "ao",
            Self::Reflectance => "reflectance",
            Self::Depth => "depth",
        }
    }

    /// [`Self::label`] 的逆，另认 "lambert" 别名；未知名返回 None。
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        if name == "lambert" {
            return Some(Self::Lambert);
        }
        Self::ALL.into_iter().find(|mode| mode.label() == name)
    }

    /// 初值：env `ASH_RENDER_MODE`（缺省或空 = filament）。
    pub(crate) fn from_env() -> Self {
        match std::env::var("ASH_RENDER_MODE").ok().as_deref() {
            None | Some("") => Self::Lambert,
            Some(name) => Self::from_name(name).unwrap_or_else(|| {
                // 拼错是配置问题，warn 后按默认继续
                warn!(
                    "ASH_RENDER_MODE={name:?} 无法识别（可选 {}），按 filament 继续",
                    Self::NAME_HINT,
                );
                Self::Lambert
            }),
        }
    }

    /// UBO `mode` 值（`FRAME_MODE_*`，与 `gbuffer_composite.wgsl` 的 `MODE_*` 同值）。
    pub(crate) fn as_u32(self) -> u32 {
        match self {
            Self::Lambert => FRAME_MODE_LAMBERT,
            Self::Unlit => FRAME_MODE_UNLIT,
            Self::Normal => FRAME_MODE_NORMAL,
            Self::Position => FRAME_MODE_POSITION,
            Self::Metallic => FRAME_MODE_METALLIC,
            Self::Roughness => FRAME_MODE_ROUGHNESS,
            Self::AmbientOcclusion => FRAME_MODE_AO,
            Self::Reflectance => FRAME_MODE_REFLECTANCE,
            Self::Depth => FRAME_MODE_DEPTH,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 名称表往返：每个模式按 label 解析回自身，lambert 别名落 Lambert。
    #[test]
    fn label_round_trips() {
        for mode in RenderMode::ALL {
            assert_eq!(RenderMode::from_name(mode.label()), Some(mode));
        }
        assert_eq!(RenderMode::from_name("lambert"), Some(RenderMode::Lambert));
        assert_eq!(RenderMode::from_name("bogus"), None);
    }
}
