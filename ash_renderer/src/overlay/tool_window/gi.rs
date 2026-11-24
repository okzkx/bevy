//! 表面缓存（9.5/9.6）页：总开关、调试视图与形态、法线来源、播种/整理参数，9.6 起再补
//! 估计开关与全部估计参数（样本档/预热/短滞后/多跳/预算/自发光强度/清单条数）。控件就地写
//! [`GiSettings`]；`draw_frame` 每帧照资源写 GI UBO / 组装 ④.5 段，改参下一帧生效。
//! BRP `gi_set` 写同一份，面板手动切与 AI 调参互不冲突——**面板是默认入口**，BRP 是脚本通道。
//!
//! 勾上总开关时若还没选视图（view=0），自动切到覆盖图（view=1）；勾上估计开关时若还没选视图，
//! 自动切到辐照度视图（view=10）——否则开了也看不到任何视觉变化。这是面板的便利默认；
//! BRP 侧保持"view>0 才画"的原样语义。Volume/池规格（Size/Res/级联/容量）只能在关态改，
//! 面板不提供，走 BRP `gi_set`。

use crate::render::GiSettings;

/// 视图号 → 名称（与 `gi_cell.wgsl` 的 `view_color` 编号一致）。
const VIEWS: &[(u32, &str)] = &[
    (0, "关（不画视图）"),
    (1, "覆盖（有 patch 绿 / 无 patch 红 / 出 Volume 蓝）"),
    (2, "级联"),
    (3, "方向桶"),
    (4, "rank"),
    (5, "心跳年龄"),
    (6, "池水位与失败计数"),
    (7, "位置偏离（离面距离 / 体素）"),
    (8, "cell 归属"),
    (9, "样本档与稀释（本帧谁在算）"),
    (10, "辐照度 E(n)"),
    (11, "方差（绿稳 / 红抖）"),
    (12, "更新次数（收敛程度）"),
    (13, "rank 分布（0 屏幕播种 / 1 bounce）"),
];

/// 形态名（0 覆盖图 / 1 覆盖图+方片 / 2 只方片）。
const FORMS: &[(u32, &str)] = &[(0, "覆盖图"), (1, "覆盖图 + 方片"), (2, "只方片")];

/// 表面缓存页的一次构建输入（控件就地写资源）。
pub(in crate::overlay) struct GiPage<'a> {
    pub(in crate::overlay) gi: &'a mut GiSettings,
}

impl GiPage<'_> {
    pub(in crate::overlay) fn show(self, ui: &mut egui::Ui) {
        let Self { gi } = self;
        let was_enabled = gi.enabled;
        let was_estimating = gi.estimation;
        ui.checkbox(&mut gi.enabled, "表面缓存 GI（gi_enabled）");
        if gi.enabled && !was_enabled && gi.view == 0 {
            gi.view = 1;
        }
        ui.checkbox(&mut gi.estimation, "估计辐照度（gi_estimation）");
        if gi.estimation && !was_estimating && gi.view == 0 {
            // 开了估计却还在视图 0 = 什么也看不到；自动切到辐照度视图
            gi.view = 10;
        }
        ui.monospace(
            "BRP: ash_renderer/gi_set / gi_stats / gi_dump / gi_census / gi_estimate_probe",
        );
        ui.small("关闭 = 不录任何 dispatch、不画视图，画面与 9.3 逐位一致。");
        ui.separator();

        ui.strong("调试视图");
        egui::ComboBox::from_id_salt("gi_view")
            .selected_text(
                VIEWS
                    .iter()
                    .find(|(v, _)| *v == gi.view)
                    .map_or("未知", |(_, name)| *name),
            )
            .width(420.0)
            .show_ui(ui, |ui| {
                for &(value, name) in VIEWS {
                    ui.selectable_value(&mut gi.view, value, name);
                }
            });
        ui.small("覆盖图直接把 cell/patch 数据铺在可见网格表面；方片按 patch 位置/法线画小方块。");
        ui.separator();

        ui.strong("形态");
        ui.horizontal(|ui| {
            for &(value, name) in FORMS {
                ui.selectable_value(&mut gi.form, value, name);
            }
        });
        ui.separator();

        ui.strong("采集法线");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut gi.normal_source, 0, "深度差分平面法线（默认）");
            ui.selectable_value(&mut gi.normal_source, 1, "G-buffer 着色法线（A/B）");
        });
        ui.small("着色法线带贴图细节，容易把同一平面拆进多个桶（预算 ×16），只作对照。");
        ui.separator();

        ui.checkbox(&mut gi.seed, "屏幕播种（关 = 只回收不分配）");
        ui.separator();

        ui.strong("估计参数（9.6）");
        ui.horizontal(|ui| {
            ui.label("样本档");
            for tier in [1u32, 2, 4, 8] {
                ui.selectable_value(&mut gi.samples, tier, format!("{tier}"));
            }
            ui.separator();
            ui.label("预热倍率");
            for tier in [1u32, 2, 4, 8] {
                ui.selectable_value(&mut gi.warmup, tier, format!("{tier}"));
            }
        });
        ui.checkbox(&mut gi.multi_bounce, "多跳（命中点查缓存 + bounce 分配）");
        ui.add(
            egui::Slider::new(&mut gi.short_hysteresis, 0.5..=0.99)
                .text("短时滞后（越小越跟手）")
                .fixed_decimals(2),
        );
        // 预算按"万条射线"给滑条：直接给条数是 6 位数字，拖不动也不好读
        let mut budget_wan = gi.budget as f32 / 10_000.0;
        if ui
            .add(
                egui::Slider::new(&mut budget_wan, 0.1..=200.0)
                    .text("射线预算（万条/帧）")
                    .fixed_decimals(1),
            )
            .changed()
        {
            gi.budget = (budget_wan * 10_000.0).round().max(1.0) as u32;
        }
        ui.add(
            egui::Slider::new(&mut gi.emissive_intensity, 0.0..=8.0)
                .text("自发光强度倍数")
                .fixed_decimals(2),
        );
        ui.add(
            egui::Slider::new(&mut gi.light_samples, 0..=256)
                .text("光源侧清单条数")
                .fixed_decimals(0),
        );
        ui.small("样本档会被射线预算自动降档（视图 9 看本帧实际档位与稀释）。");
        ui.checkbox(&mut gi.spatial_filter, "空间滤波（切平面抖动 taps）");
        ui.add(
            egui::Slider::new(&mut gi.spatial_radius, 0.0..=8.0)
                .text("空间滤波半径（体素倍数）")
                .fixed_decimals(2),
        );
        ui.add(
            egui::Slider::new(&mut gi.defrag_waves, 0..=16)
                .text("每帧整理波数（每波 512 条）")
                .fixed_decimals(0),
        );
        ui.separator();

        let config = gi.config;
        ui.strong("Volume / 池规格（只读）");
        ui.monospace(format!(
            "Size {:.1} m / Res {} / 级联 {} / 体素 {:.3} m / 池 {}",
            config.size(),
            config.res,
            config.cascades,
            config.voxel_min,
            config.capacity,
        ));
        ui.small("改规格只能在关态：BRP gi_set {\"enabled\":false, \"size\":…, \"cascades\":…}");
    }
}
