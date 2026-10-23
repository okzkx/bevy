//! 剔除与 LOD 页：视锥剔除 / Hi-Z 遮挡 / GPU LOD 三开关与各自读数，外加
//! indirect 提交读数。读数都是帧槽直读（陈旧 ≤2 帧），只做展示不驱动绘制。

use crate::render::{
    CullDiagnostics, CullSettings, IndirectStats, LodDiagnostics, LodSettings,
    OcclusionDiagnostics, OcclusionFallback, OcclusionSettings,
};

/// 剔除与 LOD 页的一次构建输入：开关就地写资源（BRP cull/occlusion/lod 方法写同一份）。
pub(in crate::overlay) struct CullingPage<'a> {
    pub(in crate::overlay) cull_settings: &'a mut CullSettings,
    pub(in crate::overlay) cull_diag: &'a CullDiagnostics,
    pub(in crate::overlay) occlusion_settings: &'a mut OcclusionSettings,
    pub(in crate::overlay) occlusion_diag: &'a OcclusionDiagnostics,
    pub(in crate::overlay) lod_settings: &'a mut LodSettings,
    pub(in crate::overlay) lod_diag: &'a LodDiagnostics,
    pub(in crate::overlay) indirect_stats: &'a IndirectStats,
}

impl CullingPage<'_> {
    pub(in crate::overlay) fn show(self, ui: &mut egui::Ui) {
        let Self {
            cull_settings,
            cull_diag,
            occlusion_settings,
            occlusion_diag,
            lod_settings,
            lod_diag,
            indirect_stats,
        } = self;
        // 视锥剔除：开关切 compute dispatch / host 全可见填充（两条路径画面相同）
        ui.checkbox(&mut cull_settings.enabled, "视锥剔除（compute）");
        ui.monospace(format!(
            "可见 {}/{} 行（bound {}，帧槽 {} 读数）",
            cull_diag.visible_count,
            cull_diag.rows_compared,
            cull_diag.bound,
            cull_diag.marks_frame,
        ));
        ui.separator();
        // Hi-Z 遮挡：关闭或回退时显示原因（相机跳变/金字塔 warmup）
        ui.checkbox(&mut occlusion_settings.enabled, "Hi-Z 遮挡剔除");
        let state = if occlusion_diag.dispatched {
            "dispatch"
        } else {
            match occlusion_diag.fallback {
                Some(OcclusionFallback::Disabled) => "关",
                Some(OcclusionFallback::CullOff) => "关（视锥关闭）",
                Some(OcclusionFallback::PyramidWarmup) => "warmup",
                Some(OcclusionFallback::CameraJump) => "相机跳变",
                None => "空场",
            }
        };
        ui.monospace(format!(
            "状态 {state} · 遮挡判可见 {}/{} 行（帧槽 {} 读数）",
            occlusion_diag.occlusion_visible_count,
            occlusion_diag.rows_compared,
            occlusion_diag.marks_frame,
        ));
        ui.separator();
        // GPU LOD：关闭 = 全部取最细档
        ui.checkbox(&mut lod_settings.enabled, "GPU LOD 选档");
        ui.monospace(format!(
            "{}（阈值 {:.0}px，迟滞 {:.2}）",
            if lod_diag.dispatched { "dispatch" } else { "关" },
            lod_settings.threshold_px,
            lod_settings.hysteresis,
        ));
        ui.monospace(format!(
            "档位分布 {:?}（帧槽 {} 读数）",
            lod_diag.level_counts, lod_diag.marks_frame,
        ));
        ui.separator();
        ui.strong("indirect 提交");
        ui.monospace(format!(
            "提交 {} 次 · 命令 {} 条（零实例 {}）",
            indirect_stats.draw_calls_recorded,
            indirect_stats.command_count,
            indirect_stats.zero_instance_count,
        ));
        ui.monospace(format!(
            "在场走访 {} 行 · 暂缓 {}",
            indirect_stats.walk_rows, indirect_stats.pending_rows,
        ));
    }
}
