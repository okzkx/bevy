//! 右上角坐标轴指示器：世界三轴在视空间的正交投影（Unity Scene 视图右上角
//! 同款），随相机旋转实时转动。纯显示 widget——零写路径、零新 GPU 资源，
//! 形状走既有 `EguiFrame` shapes 通道，字母用已有拉丁字形（零纹理增量）。

use bevy::prelude::*;

/// 指示器一次构建输入：框架 pass 每帧组好。
pub(super) struct AxisGizmo<'a> {
    pub(super) ctx: &'a egui::Context,
    /// 主相机世界旋转；`None`（相机缺席/主窗未建）时整帧不画。
    pub(super) cam_rotation: Option<Quat>,
}

/// 盘半径与到窗边的余量（points 域，视觉参数一次定死）。
const RADIUS: f32 = 34.0;
const MARGIN: f32 = 24.0;

impl AxisGizmo<'_> {
    pub(super) fn show(self) {
        let Self { ctx, cam_rotation } = self;
        let Some(cam_rot) = cam_rotation else {
            return;
        };
        let screen = ctx.viewport_rect();
        let center = egui::pos2(
            screen.right() - MARGIN - RADIUS,
            screen.top() + MARGIN + RADIUS,
        );
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("axis_gizmo"),
        ));
        painter.circle_filled(center, RADIUS, egui::Color32::from_black_alpha(150));
        painter.circle_stroke(
            center,
            RADIUS,
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(60)),
        );

        // 世界→视空间：相机旋转取逆；视空间 -Z 朝屏幕里。背向轴先画，近轴压在上层。
        let view_rot = cam_rot.inverse();
        let mut axes: Vec<(Vec3, &'static str, egui::Color32)> = [
            (Vec3::X, "X", egui::Color32::from_rgb(226, 76, 76)),
            (Vec3::Y, "Y", egui::Color32::from_rgb(101, 202, 82)),
            (Vec3::Z, "Z", egui::Color32::from_rgb(88, 138, 235)),
        ]
        .map(|(a, label, color)| (view_rot * a, label, color))
        .into_iter()
        .collect();
        axes.sort_by(|a, b| a.0.z.total_cmp(&b.0.z));
        for (d, label, color) in axes {
            // egui y 向下、视空间 y 向上：屏幕投影 (d.x, -d.y)。**不做归一化**：
            // 前缩短线正是"轴指向观察者"的表达，归一化会抹掉深度信息，小转角
            // 下三轴恒满长，指示器看起来不随相机转。
            let dir = egui::vec2(d.x, -d.y);
            let len = dir.length();
            // 深度 k∈[0,1]：背向 0 → 侧向 0.5 → 朝观察者 1，明暗/线宽随动
            let k = d.z * 0.5 + 0.5;
            let color = darken(color, 0.35 + 0.65 * k);
            if len < 0.05 {
                // 轴正对/正背观察者：投影缩成点，点径代替线宽表达远近
                painter.circle_filled(center, 3.0 + 1.5 * k, color);
                continue;
            }
            let end = center + dir * RADIUS;
            painter.line_segment([center, end], egui::Stroke::new(1.5 + k, color));
            if len > 0.15 {
                // 投影过短的轴不标字母（贴着圆心堆叠），其余标在端点外 9pt
                painter.text(
                    end + dir / len * 9.0,
                    egui::Align2::CENTER_CENTER,
                    label,
                    egui::FontId::proportional(12.0),
                    color,
                );
            }
        }
    }
}

/// 颜色按比例压暗（RGB 同乘，alpha 保持不透明）：远近轴用明暗区分而不用
/// 透明度——半透明亮色在 epaint 的预乘口径下边缘会发灰，明暗两口径皆无歧义。
fn darken(c: egui::Color32, k: f32) -> egui::Color32 {
    let ch = |v: u8| (v as f32 * k) as u8;
    egui::Color32::from_rgb(ch(c.r()), ch(c.g()), ch(c.b()))
}
