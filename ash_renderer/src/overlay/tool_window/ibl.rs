//! 环境光照（IBL）页：开关、强度与漫反射编码（SH-L2 / GlobalProbe 16 桶），
//! 控件就地写 [`IblSettings`]。`draw_frame` 每帧照资源写帧 UBO，改参下一帧生效；
//! BRP `ibl_set` 写同一份，面板手动切与 AI 调参互不冲突。
//!
//! 语义提示：天空是环境光的上游（9.2.8），天空关时 IBL 自动不生效；漫反射编码
//! 只换求值分支，高光始终走 9.2 的预滤波 cubemap。

use crate::render::{IblSettings, IBL_MODE_PROBE16, IBL_MODE_SH_L2};

/// IBL 页的一次构建输入（控件就地写资源）。
pub(in crate::overlay) struct IblPage<'a> {
    pub(in crate::overlay) ibl: &'a mut IblSettings,
}

impl IblPage<'_> {
    pub(in crate::overlay) fn show(self, ui: &mut egui::Ui) {
        let Self { ibl } = self;
        ui.checkbox(&mut ibl.enabled, "环境光照（ibl_enabled）");
        ui.monospace("BRP: ash_renderer/ibl_set / ibl_stats / ibl_census");
        ui.add(
            egui::Slider::new(&mut ibl.intensity, 0.0..=10.0)
                .text("强度倍数")
                .fixed_decimals(2),
        );
        ui.separator();

        ui.strong("漫反射编码");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut ibl.mode, IBL_MODE_SH_L2, "SH-L2（默认）");
            ui.selectable_value(&mut ibl.mode, IBL_MODE_PROBE16, "GlobalProbe 16 桶");
        });
        ui.small("A/B 结论：默认 SH-L2；16 桶与 patch 同格式，留给 9.7 Volume 外兜底。");
        ui.small("天空关时环境项回退常量 ambient（9.2.8），IBL 与编码都不生效。");
    }
}
