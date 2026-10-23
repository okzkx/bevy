//! 管线控制窗口（业务面板）：渲染管线开关与参数——阴影（cascade CSM）、
//! 后处理（Bloom/tonemap、SMAA）。checkbox/滑杆直写
//! [`ShadowSettings`]/[`BloomSettings`]/[`SmaaSettings`] 资源，`draw_frame`
//! 每帧照资源执行；BRP `shadow_set`/`bloom_set`/`smaa_set` 写同一份资源，
//! 面板与脚本天然同源（不养两份逻辑）。
//!
//! 开关语义：
//! - 阴影关 = shadow 剔除标记/命令/绘制/采样全链旁路，画面与 8.2 基线一致；
//! - Bloom/tonemap 关 = composite 直出 HDR 线性值（8.4 前基线形态）；
//! - SMAA 关 = composite 直出无边缘平滑（8.5 前基线形态）。

use crate::render::{BloomSettings, ShadowSettings, SmaaSettings};

/// 管线控制窗口的一次构建输入：框架 pass 每帧组好，开关就地写资源。
pub(in crate::overlay) struct PipelineWindow<'a> {
    pub(in crate::overlay) ctx: &'a egui::Context,
    /// 显隐开关：总控 checkbox 与窗口 [×] 写同一字段（egui `Window::open` 借用）。
    pub(in crate::overlay) open: &'a mut bool,
    /// 阴影开关（checkbox 直接写，draw_frame 读；BRP shadow_set 同源）。
    pub(in crate::overlay) shadow: &'a mut ShadowSettings,
    /// Bloom/tonemap 开关与参数（checkbox/滑杆直接写；BRP bloom_set 同源）。
    pub(in crate::overlay) bloom: &'a mut BloomSettings,
    /// SMAA 开关（checkbox 直接写，draw_frame 读；BRP smaa_set 同源）。
    pub(in crate::overlay) smaa: &'a mut SmaaSettings,
}

impl PipelineWindow<'_> {
    pub(in crate::overlay) fn show(self) {
        let Self { ctx, open, shadow, bloom, smaa } = self;
        egui::Window::new("管线控制")
            .open(open)
            .default_pos(egui::pos2(12.0, 320.0))
            .default_width(240.0)
            .show(ctx, |ui| {
                ui.heading("阴影");
                ui.checkbox(&mut shadow.enabled, "阴影（cascade CSM）");
                ui.monospace("BRP: ash_renderer/shadow_set");
                ui.separator();
                ui.heading("后处理");
                ui.checkbox(&mut bloom.enabled, "Bloom");
                if bloom.enabled {
                    ui.add(
                        egui::Slider::new(&mut bloom.strength, 0.0..=0.25)
                            .text("强度")
                            .fixed_decimals(3),
                    );
                    ui.checkbox(&mut bloom.tonemap, "ACES tone mapping");
                }
                ui.checkbox(&mut smaa.enabled, "SMAA 抗锯齿");
                ui.monospace("BRP: ash_renderer/bloom_set / smaa_set");
            });
    }
}
