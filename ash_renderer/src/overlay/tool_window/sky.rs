//! 程序化天空盒页：天空参数（渐变三色/强度/过渡幂/太阳圆盘/光晕）就地写
//! [`SkySettings`]。`draw_frame` 每帧照资源写帧 UBO，改参下一帧生效；BRP
//! `sky_set`/`sky_stats` 写读同一份，AI 调参与面板手动调互不冲突。

use crate::render::SkySettings;

/// 天空页的一次构建输入（控件就地写资源）。
pub(in crate::overlay) struct SkyPage<'a> {
    pub(in crate::overlay) sky: &'a mut SkySettings,
}

impl SkyPage<'_> {
    pub(in crate::overlay) fn show(self, ui: &mut egui::Ui) {
        let Self { sky } = self;
        ui.checkbox(&mut sky.enabled, "天空（sky_enabled）");
        ui.monospace("BRP: ash_renderer/sky_set / sky_stats");
        ui.separator();
        ui.strong("渐变");
        // 颜色是线性辐亮度系数（0..1），整体亮度走"强度"
        color_row(ui, "天顶", &mut sky.zenith);
        color_row(ui, "地平线", &mut sky.horizon);
        color_row(ui, "地面", &mut sky.ground);
        ui.add(
            egui::Slider::new(&mut sky.intensity, 0.0..=10000.0)
                .text("强度")
                .fixed_decimals(0),
        );
        ui.add(
            egui::Slider::new(&mut sky.horizon_exponent, 0.05..=8.0)
                .text("天顶过渡幂")
                .fixed_decimals(2),
        );
        ui.add(
            egui::Slider::new(&mut sky.ground_exponent, 0.05..=8.0)
                .text("地面过渡幂")
                .fixed_decimals(2),
        );
        ui.separator();
        ui.strong("太阳（方向/颜色跟随方向光）");
        // 角半径存弧度，界面按角度调
        let mut deg = sky.sun_radius.to_degrees();
        if ui
            .add(
                egui::Slider::new(&mut deg, 0.05..=5.0)
                    .text("圆盘半径")
                    .suffix("°")
                    .fixed_decimals(2),
            )
            .changed()
        {
            sky.sun_radius = deg.to_radians();
        }
        ui.add(
            egui::Slider::new(&mut sky.sun_gain, 0.0..=10.0)
                .text("圆盘增益")
                .fixed_decimals(2),
        );
        ui.add(
            egui::Slider::new(&mut sky.glow_intensity, 0.0..=2.0)
                .text("光晕强度")
                .fixed_decimals(2),
        );
        ui.add(
            egui::Slider::new(&mut sky.glow_falloff, 0.5..=64.0)
                .text("光晕衰减幂")
                .fixed_decimals(1),
        );
    }
}

/// 颜色行：标签 + RGB 拾色按钮（拾色器改颜色，强度另调）。
fn color_row(ui: &mut egui::Ui, label: &str, rgb: &mut [f32; 3]) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.color_edit_button_rgb(rgb);
    });
}
