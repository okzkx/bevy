//! 管线控制页：渲染管线开关与参数——阴影（cascade CSM）、
//! 后处理（Bloom/tonemap、SMAA）。checkbox/滑杆直写
//! [`ShadowSettings`]/[`BloomSettings`]/[`SmaaSettings`] 资源，`draw_frame`
//! 每帧照资源执行；BRP `shadow_set`/`bloom_set`/`smaa_set` 写同一份资源，
//! 面板与脚本天然同源（不养两份逻辑）。
//!
//! 开关语义：
//! - 阴影关 = shadow 剔除标记/命令/绘制/采样全链旁路（无阴影基线画面）；
//! - Bloom/tonemap 关 = composite 直出 HDR 线性值；
//! - SMAA 关 = composite 直出无边缘平滑。

use crate::render::{BloomSettings, ShadowSettings, SmaaSettings, TonemapCurve};

/// 管线控制页的一次构建输入：框架 pass 每帧组好，开关就地写资源。
pub(in crate::overlay) struct PipelinePage<'a> {
    /// 阴影开关（checkbox 直接写，draw_frame 读；BRP shadow_set 同源）。
    pub(in crate::overlay) shadow: &'a mut ShadowSettings,
    /// Bloom/tonemap 开关与参数（checkbox/滑杆直接写；BRP bloom_set 同源）。
    pub(in crate::overlay) bloom: &'a mut BloomSettings,
    /// SMAA 开关（checkbox 直接写，draw_frame 读；BRP smaa_set 同源）。
    pub(in crate::overlay) smaa: &'a mut SmaaSettings,
}

impl PipelinePage<'_> {
    pub(in crate::overlay) fn show(self, ui: &mut egui::Ui) {
        let Self { shadow, bloom, smaa } = self;
        ui.strong("阴影");
        ui.checkbox(&mut shadow.enabled, "阴影（cascade CSM）");
        ui.monospace("BRP: ash_renderer/shadow_set");
        ui.separator();
        ui.strong("后处理");
        ui.checkbox(&mut bloom.enabled, "Bloom");
        if bloom.enabled {
            ui.add(
                egui::Slider::new(&mut bloom.strength, 0.0..=0.25)
                    .text("强度")
                    .fixed_decimals(3),
            );
            ui.checkbox(&mut bloom.tonemap, "ACES tone mapping");
            if bloom.tonemap {
                ui.horizontal(|ui| {
                    ui.label("曲线");
                    ui.radio_value(&mut bloom.curve, TonemapCurve::Narkowicz, "Narkowicz");
                    ui.radio_value(&mut bloom.curve, TonemapCurve::FilamentAces, "Filament");
                });
                if bloom.curve == TonemapCurve::FilamentAces {
                    ui.add(
                        egui::Slider::new(&mut bloom.gain, 0.5..=2.5)
                            .text("亮度增益")
                            .fixed_decimals(3),
                    );
                }
            }
        }
        ui.checkbox(&mut smaa.enabled, "SMAA 抗锯齿");
        ui.monospace("BRP: ash_renderer/bloom_set / smaa_set");
    }
}
