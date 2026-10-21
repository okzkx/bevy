//! 资产加载指示弹窗（7.12）：右下角纯显示小弹窗——进场加载期间显示旋转菊花
//!（egui `Spinner`）+ 一行固定文本，完成冷却后自动隐藏。
//!
//! 判据（7.12.1 施工钉明，latch 制）：[`LoadActivity`] 启动即置位（覆盖 glb
//! 解析窗口——资产请求尚未发起、pending=0 的时段，账本差值判据会漏亮）；
//! 完成判据 = **所有进场根已展开**（`WorldAssetRoot` 占位先到、实体树展开后
//! 才有 `Children`——runtime `load_world` 的解析窗口由此覆盖：旧场景的
//! primitive 仍在场、pending=0 时段不误判完成，实抓 3.3s 误收账后补此腿）
//! ∧ pending 清零 ∧ 上传票据在飞清零。展开失败的根（伪造资产路径）永无
//! `Children` = 菊花如实长转不消失（计划测试方案的失败例口径），失败详情走
//! BRP readiness 既有读数。
//!
//! 运行期重挂：新进场根（BRP load_world）/流送重请求（pending 回升）如实
//! 重新亮起——资产确实在加载。
//!
//! 节拍集成：菊花动画 `request_repaint` 走 7.8 门控的 `force_next` 消费位
//!（repaint_delay == ZERO）——加载期面板 pass 每帧直跑（相位推进），弹窗隐藏
//! 后触发自然消失回落 7.8 节拍。零交互：弹窗画在 `Order::Background` 层，
//! `layer_id_at` 仲裁只认非 Background——不拦相机控制，不进指针仲裁面。

use bevy::prelude::*;
use ash_macros::system;

use crate::scene::AssetReadiness;
use crate::vulkan::Uploader;

/// 完成判据的稳态确认窗（帧）：pending/票据清零后持续满足的帧数——加载末期
/// 逐行到货的抖动不闪窗。
const SETTLE_FRAMES: u32 = 30;

/// 加载活跃 latch（7.12.1）：Startup 置位（覆盖解析窗口），完成判据冷却后
/// 清零；overlay 弹窗与节拍触发源的读数口。
#[derive(Resource)]
pub struct LoadActivity {
    /// 菊花显示中（true = 加载活跃）。
    pub active: bool,
    /// 完成判据已连续满足的帧数（冷却计步）。
    settled_frames: u32,
}

impl Default for LoadActivity {
    fn default() -> Self {
        Self { active: true, settled_frames: 0 }
    }
}

/// 加载活跃判据 tick（Update，先于 egui pass）：完成判据走冷却，运动中清步。
#[system]
pub(crate) fn load_activity_tick(
    mut activity: ResMut<LoadActivity>,
    readiness: Res<AssetReadiness>,
    uploader: Option<Res<Uploader>>,
    roots: Query<(Entity, Option<&Children>), With<WorldAssetRoot>>,
) -> Result<(), BevyError> {
    let pending = readiness.stats.pending_meshes + readiness.stats.pending_images;
    // 票据在飞：发行号 > 完成号；completed 读取失败（设备丢失级）视作在飞
    //（菊花保守长转，不上报——上传链自身的 fatal 已在通道里）
    let tickets_idle = match uploader.as_deref() {
        Some(up) => match up.completed_ticket() {
            Ok(done) => up.last_issued_ticket() <= done,
            Err(_) => false,
        },
        None => true, // init 前空帧：上传器未建，无从在飞
    };
    // 展开中判定（解析窗口腿）：任一进场根尚无 `Children` = glb 还在解析/
    // 展开（WorldAssetRoot 占位组件先到、实体树后到）。运行期 load_world 的
    // 解析窗口由此覆盖：旧场景 primitive 仍在场、pending=0 时段不误判完成
    //（runtime 重载 3.3s 误收账实抓后的补腿）；展开失败的根永无 Children =
    // 菊花如实长转。
    let expanding = roots.iter().any(|(_, children)| children.is_none());
    if activity.active {
        if !expanding && pending == 0 && tickets_idle {
            activity.settled_frames += 1;
            if activity.settled_frames >= SETTLE_FRAMES {
                activity.active = false;
                info!(
                    "资产加载完成：进场根全展开 ∧ pending 清零 ∧ 票据在飞清零——弹窗冷却隐藏"
                );
            }
        } else {
            activity.settled_frames = 0;
        }
    } else if expanding || pending > 0 || !tickets_idle {
        // 运行期重挂（BRP load_world 新根 / 流送重请求）：任一腿复活即亮起
        activity.active = true;
        activity.settled_frames = 0;
        info!("资产加载弹窗重新亮起（expanding {expanding} / pending {pending}）");
    }
    Ok(())
}

/// 加载弹窗面板（7.12.2）：右下角 Background 层——不进指针仲裁面（layer_id_at
/// 仲裁只认非 Background），相机控制与面板交互不受弹窗遮挡影响；纯显示零
/// 交互。菊花动画经 `request_repaint` 走 7.8 门控 force_next 位，加载期面板
/// pass 每帧直跑（相位推进），隐藏后自然回落节拍。
pub struct AssetLoadPopup<'a> {
    pub ctx: &'a egui::Context,
}

impl AssetLoadPopup<'_> {
    pub fn show(&self) {
        egui::Area::new(egui::Id::new("asset_load_popup"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -16.0])
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
