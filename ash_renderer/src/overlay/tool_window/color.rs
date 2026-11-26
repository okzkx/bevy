//! 颜色校验页（9.9）：tonemap 链三域的直方图读数 + 曝光/曲线/采样开关。
//!
//! 数据来自 [`ColorScopeReader`]（8 Hz 回读 bins 的节流器）。轴口径与
//! `shaders/post/color_scope.wgsl` 同源：A/B 域是 −16…+8 EV 的绝对 log2 轴
//!（0.09375 EV/bin），C 域是 sRGB 编码后的 0…1 轴（1/256 per bin）。页面只把 bins
//! 画出来，不在 CPU 侧另算统计——BRP `color_scope_get` 读的是同一份 bins。

use bevy::math::ops::{ln, powf};

use crate::render::{
    BloomSettings, ColorScopeDomain, ColorScopeReader, ColorScopeSettings, ColorScopeSnapshot,
    ColorScopeTargets, Context, ExposureMode, ExposureSettings, RenderMode, TonemapCurve,
};

/// 单张直方图高度（points）。
const PLOT_HEIGHT: f32 = 84.0;
/// 直方图底部刻度带高度。
const AXIS_STRIP: f32 = 14.0;
/// CbCr 热力图边长（points）。
const CBCR_EDGE: f32 = 168.0;

const LUMA_COLOR: egui::Color32 = egui::Color32::from_rgb(222, 226, 232);
const R_COLOR: egui::Color32 = egui::Color32::from_rgb(236, 104, 104);
const G_COLOR: egui::Color32 = egui::Color32::from_rgb(104, 208, 128);
const B_COLOR: egui::Color32 = egui::Color32::from_rgb(104, 152, 240);
const WARN_COLOR: egui::Color32 = egui::Color32::from_rgb(230, 190, 90);
const ERROR_COLOR: egui::Color32 = egui::Color32::from_rgb(230, 120, 120);

/// A/B 轴刻度（t 是 0…1 的画布比例，标签是 EV）。
const LOG2_TICKS: &[(f32, &str)] = &[
    (0.0, "-16"),
    (1.0 / 3.0, "-8"),
    (2.0 / 3.0, "0"),
    (1.0, "+8"),
];
/// C 轴刻度（sRGB 编码后的 0…1）。
const UNIT_TICKS: &[(f32, &str)] = &[
    (0.0, "0"),
    (0.25, ".25"),
    (0.5, ".5"),
    (0.75, ".75"),
    (1.0, "1"),
];

/// 颜色页的跨帧状态（CbCr 热力图纹理按快照帧号换图）。
#[derive(Default, bevy::prelude::Resource)]
pub(in crate::overlay) struct ColorPageState {
    cbcr: Option<(u64, egui::TextureHandle)>,
}

impl ColorPageState {
    /// 快照帧号变化时重建 64×64 的 CbCr 热力图：色块是"该 CbCr 对应的可显示色"，
    /// 亮度按 log(1+样本数) 缩放（行列各 64 格，行 = Cr、列 = Cb）。
    fn cbcr_texture<'a>(
        &'a mut self,
        ctx: &egui::Context,
        snapshot: &ColorScopeSnapshot,
    ) -> Option<&'a egui::TextureHandle> {
        let frame = snapshot.frame_index();
        if self.cbcr.as_ref().is_none_or(|(f, _)| *f != frame)
            && let Some(image) = build_cbcr_image(snapshot)
        {
            match &mut self.cbcr {
                Some((f, handle)) => {
                    *f = frame;
                    handle.set(image, egui::TextureOptions::NEAREST);
                }
                None => {
                    self.cbcr = Some((
                        frame,
                        ctx.load_texture("颜色校验 CbCr", image, egui::TextureOptions::NEAREST),
                    ));
                }
            }
        }
        self.cbcr.as_ref().map(|(_, handle)| handle)
    }
}

/// CbCr 散布 → RGBA 图（全空返回 `None`，面板显示空态而不是黑图）。
fn build_cbcr_image(snapshot: &ColorScopeSnapshot) -> Option<egui::ColorImage> {
    let cells = snapshot.cbcr();
    let max = cells.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return None;
    }
    let edge = crate::render::COLOR_SCOPE_CBCR_EDGE as usize;
    let denom = ln(1.0 + max as f32);
    let mut rgba = vec![0u8; edge * edge * 4];
    for row in 0..edge {
        let cr = (row as f32 + 0.5) / edge as f32 - 0.5;
        for col in 0..edge {
            let cb = (col as f32 + 0.5) / edge as f32 - 0.5;
            let count = cells[row * edge + col];
            let t = (ln(1.0 + count as f32) / denom).clamp(0.0, 1.0);
            // 全空格留一点底色，让 64×64 栅格可辨
            let gain = 0.16 + 0.84 * t;
            let r = 0.5 + 1.5748 * cr;
            let g = 0.5 - 0.1873 * cb - 0.4681 * cr;
            let b = 0.5 + 1.8556 * cb;
            let pixel = [encode_unit(r), encode_unit(g), encode_unit(b)]
                .map(|v| (v * gain * 255.0).round() as u8);
            let at = (row * edge + col) * 4;
            rgba[at..at + 4].copy_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
    }
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [edge, edge],
        &rgba,
    ))
}

/// 线性值 → sRGB 编码值（0…1），与 shader 的 `srgb_encode` 同式。
fn encode_unit(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * powf(v, 1.0 / 2.4) - 0.055
    }
}

/// 颜色校验页的一次构建输入。
pub(in crate::overlay) struct ColorPage<'a> {
    /// 采样开关（关 = 零 dispatch、零回读）。
    pub(in crate::overlay) settings: &'a mut ColorScopeSettings,
    /// 8 Hz 回读节流器（最近快照与错误）。
    pub(in crate::overlay) reader: &'a mut ColorScopeReader,
    /// bins 资源族（取最近帧槽读数）。
    pub(in crate::overlay) targets: Option<&'a ColorScopeTargets>,
    /// Vulkan 上下文（回读要用设备/队列）。
    pub(in crate::overlay) vulkan: Option<&'a Context>,
    /// 曲线/增益（与管线页、BRP `bloom_set` 同一份资源）。
    pub(in crate::overlay) bloom: &'a mut BloomSettings,
    /// 曝光（自动眼适应；BRP 未包装，面板直写资源）。
    pub(in crate::overlay) exposure: &'a mut ExposureSettings,
    /// 着色模式（判断后链是否启用）。
    pub(in crate::overlay) mode: RenderMode,
    /// 跨帧纹理缓存。
    pub(in crate::overlay) state: &'a mut ColorPageState,
}

impl ColorPage<'_> {
    pub(in crate::overlay) fn show(self, ui: &mut egui::Ui) {
        // 页面在工具窗口的纵向 ScrollArea 里：egui 默认给内容区的最大高度是视口高度
        //（"尽量不滚动、优先压缩"），靠近底部时图片/图块会被按剩余高度缩水，
        // 内容高度随之少算。放开上限后按自然高度排版，滚动到底能看全。
        ui.set_max_height(f32::INFINITY);
        let Self {
            settings,
            reader,
            targets,
            vulkan,
            bloom,
            exposure,
            mode,
            state,
        } = self;
        let chain_active = mode == RenderMode::Lambert && bloom.tonemap;

        let mut sample_once = false;
        ui.horizontal(|ui| {
            ui.checkbox(&mut settings.enabled, "颜色校验采样");
            sample_once = ui
                .button("单帧采样")
                .on_hover_text(
                    "只录一帧（不动总开关），录完自动复位；BRP 的 color_scope_set {once:true} 同义",
                )
                .clicked();
            ui.weak(if settings.enabled {
                "（8 Hz 回读；关掉零 dispatch、零回读）"
            } else if settings.once {
                "（单帧采样请求已排队）"
            } else {
                "（关闭）"
            });
        });
        if sample_once {
            settings.once = true;
        }
        ui.monospace("BRP: ash_renderer/color_scope_get");
        if settings.enabled && !chain_active {
            ui.colored_label(
                WARN_COLOR,
                "链未启用（当前非 Lambert 或 tone mapping 关闭）：present 直出 HDR，C 域无数据",
            );
        }

        // 回读：只在开着采样时做，节流在 reader 内部
        if settings.enabled {
            match (vulkan, targets) {
                (Some(ctx), Some(targets)) => {
                    if let Err(err) = reader.refresh(ctx, targets, targets.last_slot(), false) {
                        ui.colored_label(ERROR_COLOR, format!("bins 回读失败：{err}"));
                    }
                }
                _ => {
                    ui.weak("Vulkan 上下文或 bins 未就绪（init 前首帧）");
                }
            }
        }

        ui.separator();
        ui.strong("曝光（自动眼适应）");
        ui.horizontal(|ui| {
            ui.radio_value(&mut exposure.mode, ExposureMode::Auto, "自动");
            ui.radio_value(&mut exposure.mode, ExposureMode::Manual, "手动（旁路）");
        });
        if exposure.mode == ExposureMode::Auto {
            ui.add(
                egui::Slider::new(&mut exposure.key, 0.002..=0.05)
                    .logarithmic(true)
                    .text("目标中灰 key")
                    .fixed_decimals(4),
            );
            ui.add(
                egui::Slider::new(&mut exposure.min_scale, 0.05..=1.0)
                    .text("倍数下限")
                    .fixed_decimals(2),
            );
            ui.add(
                egui::Slider::new(&mut exposure.max_scale, 1.0..=16.0)
                    .text("倍数上限")
                    .fixed_decimals(2),
            );
            ui.add(
                egui::Slider::new(&mut exposure.rate_brighten, 0.1..=10.0)
                    .text("提亮速率 1/s")
                    .fixed_decimals(2),
            );
            ui.add(
                egui::Slider::new(&mut exposure.rate_darken, 0.1..=10.0)
                    .text("压暗速率 1/s")
                    .fixed_decimals(2),
            );
        }

        ui.separator();
        ui.strong("曲线与增益");
        ui.checkbox(&mut bloom.tonemap, "tone mapping（关 = present 直出 HDR）");
        ui.horizontal(|ui| {
            ui.radio_value(&mut bloom.curve, TonemapCurve::Narkowicz, "Narkowicz");
            ui.radio_value(
                &mut bloom.curve,
                TonemapCurve::FilamentAces,
                "Filament ACES",
            );
        });
        if bloom.curve == TonemapCurve::FilamentAces {
            ui.add(
                egui::Slider::new(&mut bloom.gain, 0.5..=16.0)
                    .logarithmic(true)
                    .text("ACES 亮度增益")
                    .fixed_decimals(2),
            );
        }

        ui.add(
            egui::Slider::new(&mut bloom.tonemap_strength, 0.0..=1.0)
                .text("校色强度")
                .fixed_decimals(3),
        )
        .on_hover_text(
            "曲线末端 mix：0 = 曝光后线性钳位（灰平、保留高光层次），1 = 完整曲线；\
             默认 1.0 与立案基线逐位一致。BRP: bloom_set {tonemap_strength}",
        );

        ui.separator();
        ui.strong("快照");
        ui.monospace("轴：A/B −16…+8 EV（0.09375 EV/bin）· C 0…1 sRGB（1/256 per bin）");
        let Some(snapshot) = reader.snapshot() else {
            ui.weak("还没有 bins 快照（打开采样后约 125 ms 出现）");
            return;
        };
        show_snapshot_header(ui, snapshot, targets);
        if !snapshot.magic_ok() {
            ui.colored_label(ERROR_COLOR, "bins 头 magic 不符：本槽还没被采样过");
            return;
        }

        ui.separator();
        domain_block(
            ui,
            snapshot,
            ColorScopeDomain::A,
            "A 域：原始 HDR（present 采样点，未加 Bloom、未乘曝光）",
        );
        ui.separator();
        domain_block(
            ui,
            snapshot,
            ColorScopeDomain::B,
            "B 域：曲线输入色（Bloom×强度 + 曝光后，与 present 同式）",
        );
        if let (Some(mean), Some(expected)) = (
            snapshot.mean_log2(ColorScopeDomain::B),
            snapshot.expected_b_mean(),
        ) {
            // 判定线⑤：B 域 bin 均值应与"测光 avg_log2 + log2(scale)"贴住（≤0.15 EV）；
            // B 已乘曝光倍数，头里的 avg_log2 是乘法之前的测光值，直接比会固定偏 log2(scale)。
            let delta = mean - expected;
            let color = if delta.abs() <= 0.15 {
                G_COLOR
            } else {
                WARN_COLOR
            };
            ui.colored_label(
                color,
                format!(
                    "B 域均值 {mean:+.2} EV · 期望（avg_log2+log2 scale）{expected:+.2} EV · 差 {delta:+.2} EV"
                ),
            );
        }
        ui.separator();
        domain_block(
            ui,
            snapshot,
            ColorScopeDomain::C,
            "C 域：tonemap 输出 LDR（sRGB 编码轴；线性钳位在 0/1 的样本另行计数）",
        );

        ui.separator();
        ui.strong("C 域 CbCr 散布（64×64，行 Cr、列 Cb）");
        if let Some(texture) = state.cbcr_texture(ui.ctx(), snapshot) {
            // 先精确占位再手绘：Image 会按剩余空间收缩，滚动区底部会把热力图压扁/裁掉。
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(CBCR_EDGE, CBCR_EDGE), egui::Sense::hover());
            ui.painter().image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            ui.monospace("色块 = 该 CbCr 的可显示色，亮度按 log(1+样本数)");
        } else {
            ui.weak("还没有 C 域样本");
        }
    }
}

/// 快照头：分辨率/帧号/曝光快照/frame_age。
fn show_snapshot_header(
    ui: &mut egui::Ui,
    snapshot: &ColorScopeSnapshot,
    targets: Option<&ColorScopeTargets>,
) {
    let (width, height) = snapshot.extent();
    ui.monospace(format!(
        "采样 {width}×{height} · bins 帧 {} · {} 项",
        snapshot.frame_index(),
        snapshot.bins.len()
    ));
    let ev = snapshot.exposure_state();
    ui.monospace(format!(
        "曝光快照：adapted {:.2} EV · scale {:.3} · 测光 avg {:.2} EV",
        ev[0], ev[1], ev[2]
    ));
    if let Some(targets) = targets {
        let age = targets.frame_index().wrapping_sub(snapshot.frame_index());
        let color = if age <= 2 { G_COLOR } else { WARN_COLOR };
        ui.colored_label(color, format!("frame_age {age} 帧（0 = 本帧刚写）"));
    }
}

/// 一个域的两张直方图 + 计数行。
fn domain_block(
    ui: &mut egui::Ui,
    snapshot: &ColorScopeSnapshot,
    domain: ColorScopeDomain,
    title: &str,
) {
    let (ticks, axis_note) = match domain {
        ColorScopeDomain::A | ColorScopeDomain::B => (LOG2_TICKS, "log2 绝对亮度 / EV"),
        ColorScopeDomain::C => (UNIT_TICKS, "sRGB 编码值"),
    };
    let luma = snapshot.luma(domain);
    let rgb = [
        snapshot.rgb(domain, 0),
        snapshot.rgb(domain, 1),
        snapshot.rgb(domain, 2),
    ];
    let luma_max = luma.iter().copied().max().unwrap_or(0);
    let rgb_max = rgb
        .iter()
        .flat_map(|c| c.iter())
        .copied()
        .max()
        .unwrap_or(0);

    ui.strong(title);
    ui.monospace(format!("luma（REC.709 权重）· {axis_note}"));
    plot(ui, &[(LUMA_COLOR, luma)], ticks, luma_max as f32);
    ui.monospace(format!("RGB parade（共用同一轴）· {axis_note}"));
    plot(
        ui,
        &[(R_COLOR, rgb[0]), (G_COLOR, rgb[1]), (B_COLOR, rgb[2])],
        ticks,
        rgb_max as f32,
    );
    let counts = snapshot.counts(domain);
    if let Some(ratios) = snapshot.ratios(domain) {
        ui.monospace(format!(
            "样本 {} · 低于轴 {:.2}% · 高于轴 {:.2}% · 零 {:.2}% · 触顶 {:.2}%",
            counts.samples,
            ratios.below * 100.0,
            ratios.above * 100.0,
            ratios.zero * 100.0,
            ratios.clip * 100.0,
        ));
    } else {
        ui.weak("无样本");
    }
    if matches!(domain, ColorScopeDomain::A | ColorScopeDomain::B)
        && let Some(mean) = snapshot.mean_log2(domain)
    {
        ui.monospace(format!("bin 均值 {mean:+.2} EV（越轴样本不计入）"));
    }
}

/// 画一组叠在一起的直方图：竖条 + 4 等分网格 + 底部刻度。
fn plot(ui: &mut egui::Ui, series: &[(egui::Color32, &[u32])], ticks: &[(f32, &str)], max: f32) {
    let width = ui.available_width().max(160.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, PLOT_HEIGHT), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, egui::CornerRadius::ZERO, egui::Color32::from_gray(16));
    let plot = egui::Rect::from_min_max(
        rect.min,
        egui::pos2(rect.max.x, (rect.max.y - AXIS_STRIP).max(rect.min.y)),
    );
    for i in 0..=4 {
        let x = plot.left() + plot.width() * i as f32 / 4.0;
        painter.line_segment(
            [egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())],
            egui::Stroke::new(1.0, egui::Color32::from_gray(36)),
        );
    }
    let denom = max.max(1.0);
    for (color, bins) in series {
        let n = bins.len().max(1);
        let bar_w = (plot.width() / n as f32).max(1.0);
        for (i, &count) in bins.iter().enumerate() {
            if count == 0 {
                continue;
            }
            // sqrt 压缩：底部大量样本不淹没长尾
            let t = (count as f32 / denom).sqrt().clamp(0.0, 1.0);
            let x0 = plot.left() + i as f32 * bar_w;
            let top = plot.bottom() - t * plot.height();
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(x0, top),
                    egui::pos2(x0 + bar_w, plot.bottom()),
                ),
                egui::CornerRadius::ZERO,
                *color,
            );
        }
    }
    let label_font = egui::FontId::monospace(9.0);
    for &(t, label) in ticks {
        let x = plot.left() + t.clamp(0.0, 1.0) * plot.width();
        painter.line_segment(
            [egui::pos2(x, plot.bottom()), egui::pos2(x, rect.bottom())],
            egui::Stroke::new(1.0, egui::Color32::from_gray(80)),
        );
        // 两端刻度（−16 / +8）居中会被裁掉半个字符，把文字夹进矩形内。
        let half = painter
            .layout_no_wrap(label.to_owned(), label_font.clone(), egui::Color32::WHITE)
            .size()
            .x
            * 0.5;
        let text_x = x.clamp(rect.left() + half + 1.0, rect.right() - half - 1.0);
        painter.text(
            egui::pos2(text_x, rect.bottom() - 1.0),
            egui::Align2::CENTER_BOTTOM,
            label,
            label_font.clone(),
            egui::Color32::from_gray(150),
        );
    }
    if max <= 0.0 {
        painter.text(
            plot.center(),
            egui::Align2::CENTER_CENTER,
            "无样本",
            egui::FontId::proportional(11.0),
            egui::Color32::from_gray(110),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::encode_unit;

    #[test]
    fn srgb_encode_matches_shader_formula() {
        assert!((encode_unit(0.0)).abs() < 1e-6);
        assert!((encode_unit(1.0) - 1.0).abs() < 1e-6);
        // 线性 0.5 → sRGB ≈ 0.7354
        assert!((encode_unit(0.5) - 0.735_357).abs() < 1e-3);
        // 线性值越界被钳到 0…1
        assert!((encode_unit(4.0) - 1.0).abs() < 1e-6);
        assert!((encode_unit(-1.0)).abs() < 1e-6);
    }
}
