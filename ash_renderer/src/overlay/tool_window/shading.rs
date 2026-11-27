//! 着色模式页：场景着色模式单选（光照/直出/G-buffer 单通道调试视图）。
//! 模式本体与名称表住 [`crate::render::RenderMode`]。

use crate::render::RenderMode;

/// 单选钮分组：(组名, 组内模式)，按显示顺序。
const GROUPS: &[(&str, &[RenderMode])] = &[
    ("光照", &[RenderMode::Lambert, RenderMode::Unlit]),
    (
        "几何",
        &[RenderMode::Normal, RenderMode::Position, RenderMode::Depth],
    ),
    (
        "G-buffer 材质通道",
        &[
            RenderMode::Metallic,
            RenderMode::Roughness,
            RenderMode::AmbientOcclusion,
            RenderMode::Reflectance,
        ],
    ),
    // 10.2：材质 AO（上面那档）是 G-buffer 原值，本场景无 occlusion 贴图时全白；
    // 这一档才是屏幕空间算出来的 AO，接触带深浅在这里看。
    ("环境光遮蔽（10.2）", &[RenderMode::Ssao]),
];

/// 着色模式页的一次构建输入。
pub(in crate::overlay) struct ShadingPage<'a> {
    /// 单选钮直接写它（借用自 pass 侧 ResMut，就地落资源）。
    pub(in crate::overlay) mode: &'a mut RenderMode,
}

impl ShadingPage<'_> {
    pub(in crate::overlay) fn show(self, ui: &mut egui::Ui) {
        let Self { mode } = self;
        for (i, (title, modes)) in GROUPS.iter().enumerate() {
            if i > 0 {
                ui.separator();
            }
            ui.strong(*title);
            ui.horizontal_wrapped(|ui| {
                for &m in *modes {
                    // 调试页的显示名比 BRP 名称表更具体：材质 AO 与屏幕空间 AO 是两件事，
                    // 前者是 G-buffer 原值（本场景无 occlusion 贴图时全白），后者是 10.2 算出来的。
                    let text = match m {
                        RenderMode::AmbientOcclusion => "ao（材质）",
                        RenderMode::Ssao => "ssao（屏幕空间）",
                        _ => m.label(),
                    };
                    ui.selectable_value(mode, m, text);
                }
            });
        }
    }
}
