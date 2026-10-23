//! 着色模式页：场景着色模式单选（光照/直出/G-buffer 单通道调试视图）。

use bevy::prelude::*;

use crate::render::{
    FRAME_MODE_AO, FRAME_MODE_DEPTH, FRAME_MODE_LAMBERT, FRAME_MODE_METALLIC, FRAME_MODE_NORMAL,
    FRAME_MODE_POSITION, FRAME_MODE_REFLECTANCE, FRAME_MODE_ROUGHNESS, FRAME_MODE_UNLIT,
};

/// 场景着色模式（UBO `mode` 的宿主侧形态）。UI 单选钮写、`draw_frame` 读；
/// 初值来自 env `ASH_RENDER_MODE`（拼写错误 warn 后按 filament 继续）。
#[derive(Clone, Copy, PartialEq, Eq, Resource)]
pub enum RenderMode {
    /// Filament BRDF 光照累积（默认）：照抄 Filament surfaceShading
    ///（UI/BRP/env 拼写为 filament，"lambert" 保留兼容别名，UBO 值不变）。
    Lambert,
    /// albedo 直出：base color/UV/颜色空间对照（官方侧同款置 unlit）。
    Unlit,
    /// 世界法线可视化：法线方向验收仪器（非均匀缩放案例的判定仪器）。
    Normal,
    /// 世界位置可视化：G-buffer 深度反投影重建——世界位置取法的验收仪器。
    Position,
    /// matparam 单通道灰度直出（G-buffer 调试仪器）：metallic/roughness/
    /// AO/reflectance 逐通道判读——mutate 材质热改的写后读验收口。
    Metallic,
    Roughness,
    AmbientOcclusion,
    Reflectance,
    /// reverse-Z 深度线性化灰度。
    Depth,
}

impl RenderMode {
    pub(crate) fn from_env() -> Self {
        match std::env::var("ASH_RENDER_MODE").ok().as_deref() {
            None | Some("") | Some("filament") | Some("lambert") => Self::Lambert,
            Some("unlit") => Self::Unlit,
            Some("normal") => Self::Normal,
            Some("position") => Self::Position,
            Some("metallic") => Self::Metallic,
            Some("roughness") => Self::Roughness,
            Some("ao") => Self::AmbientOcclusion,
            Some("reflectance") => Self::Reflectance,
            Some("depth") => Self::Depth,
            Some(other) => {
                // 拼写错误是配置问题不是运行故障：warn 后按默认继续，不中断帧循环
                bevy::log::warn!(
                    "ASH_RENDER_MODE={other:?} 无法识别（可选 filament(lambert 兼容)/unlit/normal/position/metallic/roughness/ao/reflectance/depth），按 filament 继续"
                );
                Self::Lambert
            }
        }
    }

    /// UBO `mode` 值（与 `gbuffer_composite.wgsl` 的 `MODE_*` 同值，
    /// `FRAME_MODE_*`；0..=2 兼容退役 A/B 参照 `debug_draw.wgsl`）。
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

/// 着色模式页的一次构建输入。
pub(in crate::overlay) struct ShadingPage<'a> {
    /// 单选钮直接写它（借用自 pass 侧 ResMut，就地落资源）。
    pub(in crate::overlay) mode: &'a mut RenderMode,
}

impl ShadingPage<'_> {
    pub(in crate::overlay) fn show(self, ui: &mut egui::Ui) {
        let Self { mode } = self;
        ui.strong("光照");
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(mode, RenderMode::Lambert, "filament");
            ui.selectable_value(mode, RenderMode::Unlit, "unlit");
        });
        ui.separator();
        ui.strong("几何");
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(mode, RenderMode::Normal, "normal");
            ui.selectable_value(mode, RenderMode::Position, "position");
            ui.selectable_value(mode, RenderMode::Depth, "depth");
        });
        ui.separator();
        ui.strong("G-buffer 材质通道");
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(mode, RenderMode::Metallic, "metallic");
            ui.selectable_value(mode, RenderMode::Roughness, "roughness");
            ui.selectable_value(mode, RenderMode::AmbientOcclusion, "ao");
            ui.selectable_value(mode, RenderMode::Reflectance, "reflectance");
        });
    }
}
