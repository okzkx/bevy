//! 物理天空页：大气三介质（瑞利/米氏/臭氧）、地面反照率、太阳圆盘与亮度倍数，
//! 控件就地写 [`SkySettings`]。`draw_frame` 每帧照资源写帧 UBO，改参下一帧生效；
//! BRP `sky_set`/`sky_stats` 写读同一份，AI 调参与面板手动调互不冲突。
//!
//! 显示单位：散射/吸收系数是 1e-6 /米 量级，面板按 ×1e-6 显示、写回时还原；
//! 高度按米（臭氧层按公里）。BRP 侧一律原始 SI 单位。

use std::ops::RangeInclusive;

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
        ui.add(
            egui::Slider::new(&mut sky.intensity, 0.0..=10.0)
                .text("亮度倍数")
                .fixed_decimals(2),
        );
        ui.separator();

        ui.strong("大气 · 瑞利（空气分子）");
        scaled_slider(
            ui,
            "R（×1e-6 /m）",
            &mut sky.rayleigh_scattering[0],
            1e6,
            0.0..=100.0,
        );
        scaled_slider(
            ui,
            "G（×1e-6 /m）",
            &mut sky.rayleigh_scattering[1],
            1e6,
            0.0..=100.0,
        );
        scaled_slider(
            ui,
            "B（×1e-6 /m）",
            &mut sky.rayleigh_scattering[2],
            1e6,
            0.0..=100.0,
        );
        ui.add(
            egui::Slider::new(&mut sky.rayleigh_scale_height, 500.0..=20_000.0)
                .text("标高（m）")
                .fixed_decimals(0),
        );
        ui.separator();

        ui.strong("大气 · 米氏（气溶胶）");
        scaled_slider(
            ui,
            "散射（×1e-6 /m）",
            &mut sky.mie_scattering,
            1e6,
            0.0..=100.0,
        );
        ui.add(
            egui::Slider::new(&mut sky.mie_scale_height, 100.0..=10_000.0)
                .text("标高（m）")
                .fixed_decimals(0),
        );
        ui.add(
            egui::Slider::new(&mut sky.mie_anisotropy, -0.9..=0.95)
                .text("各向异性 g")
                .fixed_decimals(2),
        );
        ui.separator();

        ui.strong("大气 · 臭氧（吸收）");
        scaled_slider(
            ui,
            "R（×1e-6 /m）",
            &mut sky.ozone_absorption[0],
            1e6,
            0.0..=5.0,
        );
        scaled_slider(
            ui,
            "G（×1e-6 /m）",
            &mut sky.ozone_absorption[1],
            1e6,
            0.0..=5.0,
        );
        scaled_slider(
            ui,
            "B（×1e-6 /m）",
            &mut sky.ozone_absorption[2],
            1e6,
            0.0..=5.0,
        );
        ui.add(
            egui::Slider::new(&mut sky.ozone_center, 0.0..=60_000.0)
                .text("中心高（km）")
                .custom_formatter(|v, _| format!("{:.1}", v / 1000.0)),
        );
        ui.add(
            egui::Slider::new(&mut sky.ozone_width, 1_000.0..=40_000.0)
                .text("全宽（km）")
                .custom_formatter(|v, _| format!("{:.1}", v / 1000.0)),
        );
        ui.separator();

        ui.strong("行星");
        ui.add(
            egui::Slider::new(&mut sky.atmosphere_height, 1_000.0..=120_000.0)
                .text("大气壳厚（m）")
                .fixed_decimals(0),
        );
        ui.horizontal(|ui| {
            ui.label("地面反照率");
            ui.color_edit_button_rgb(&mut sky.ground_albedo);
        });
        ui.separator();

        ui.strong("太阳");
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
    }
}

/// 按 ×`scale` 显示、写回时还原的滑杆（散射系数 1e-6 量级，原始值没法直接调）。
fn scaled_slider(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    scale: f32,
    range: RangeInclusive<f32>,
) {
    let mut shown = *value * scale;
    if ui
        .add(
            egui::Slider::new(&mut shown, range)
                .text(label)
                .fixed_decimals(2),
        )
        .changed()
    {
        *value = shown / scale;
    }
}
