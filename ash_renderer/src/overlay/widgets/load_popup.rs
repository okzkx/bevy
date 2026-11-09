//! 资产加载指示弹窗：右下角纯显示小弹窗——进场加载期间显示旋转菊花
//!（egui `Spinner`）+ 一行固定文本，完成冷却后自动隐藏。
//!
//! 判据（latch 制）：[`LoadActivity`] 启动即置位；完成判据 = 所有进场根已展开
//!（`WorldAssetRoot` 占位先到、实体树展开后才有 `Children`）∧ pending 清零 ∧
//! 上传票据在飞清零，且稳态持续 [`SETTLE_FRAMES`] 帧。运行期新进场根（BRP
//! load_world）/流送重请求（pending 回升）会重新亮起。
//!
//! 坑：glb 解析窗口内资产请求尚未发起、pending=0——只看账本差值会漏亮；runtime
//! `load_world` 时旧场景 primitive 仍在场、pending=0，不看"根已展开"会在解析期
//! 误判完成。展开失败的根永无 `Children`，菊花如实长转（失败详情走 BRP readiness）。

use ash_macros::system;
use bevy::prelude::*;

use crate::render::Uploader;
use crate::scene::AssetReadiness;

/// 完成判据的稳态确认窗（帧）：pending/票据清零后持续满足的帧数——加载末期
/// 逐行到货的抖动不闪窗。
const SETTLE_FRAMES: u32 = 30;

/// 加载活跃 latch：Startup 置位（覆盖解析窗口），完成判据冷却后
/// 清零；overlay 弹窗与节拍触发源的读数口。
#[derive(Resource)]
pub(in crate::overlay) struct LoadActivity {
    /// 菊花显示中（true = 加载活跃）。
    pub(in crate::overlay) active: bool,
    /// 完成判据已连续满足的帧数（冷却计步）。
    settled_frames: u32,
}

impl Default for LoadActivity {
    fn default() -> Self {
        Self {
            active: true,
            settled_frames: 0,
        }
    }
}

/// 加载活跃判据 tick（Update，先于 egui pass）：完成判据走冷却，运动中清步。
#[system]
pub(in crate::overlay) fn load_activity_tick(
    mut activity: ResMut<LoadActivity>,
    readiness: Res<AssetReadiness>,
    uploader: Option<Res<Uploader>>,
    unexpanded_roots: Query<(), (With<WorldAssetRoot>, Without<Children>)>,
) {
    let pending = readiness.stats.pending_meshes + readiness.stats.pending_images;
    // 票据在飞：发行号 > 完成号。上传器未建（init 前空帧）= 无从在飞；completed
    // 读取失败（设备丢失级）视作在飞——菊花保守长转，上传链自身的错误已在通道里
    let tickets_idle = uploader.as_deref().is_none_or(|up| {
        up.completed_ticket()
            .is_ok_and(|done| up.last_issued_ticket() <= done)
    });
    // 任一进场根尚无 Children = glb 还在解析/展开
    let expanding = !unexpanded_roots.is_empty();
    if activity.active {
        if !expanding && pending == 0 && tickets_idle {
            activity.settled_frames += 1;
            if activity.settled_frames >= SETTLE_FRAMES {
                activity.active = false;
                info!("资产加载完成：进场根全展开 ∧ pending 清零 ∧ 票据在飞清零——弹窗冷却隐藏");
            }
        } else {
            activity.settled_frames = 0;
        }
    } else if expanding || pending > 0 || !tickets_idle {
        activity.active = true;
        activity.settled_frames = 0;
        info!("资产加载弹窗重新亮起（expanding {expanding} / pending {pending}）");
    }
}

/// 加载弹窗：画在 `Order::Background` 层——输入仲裁（`layer_id_at`）只认非
/// Background，故不拦相机控制与面板交互。菊花动画的 `request_repaint` 经 pass
/// 门控的立即重绘位消费，加载期面板 pass 每帧直跑。
pub(in crate::overlay) struct AssetLoadPopup<'a> {
    pub(in crate::overlay) ctx: &'a egui::Context,
}

impl AssetLoadPopup<'_> {
    pub(in crate::overlay) fn show(&self) {
        egui::Area::new(egui::Id::new("asset_load_popup"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -16.0]) // 距右下角 16pt
            .order(egui::Order::Background)
            .show(self.ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(16.0));
                        ui.label("加载资产中…");
                    });
                });
            });
    }
}
