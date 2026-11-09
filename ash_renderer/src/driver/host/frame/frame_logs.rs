//! 帧循环日志状态：跨帧 `Local` 的告警去重与一次性里程碑。各 `OnceLog` 字段由
//! frame 侧直接取 `&mut` 传给各准备函数，组装状态日志走方法。

use bevy::prelude::*;

use crate::common::syntax::OnceLog;

/// 帧循环日志状态（跨帧 `Local`）：缺资源/相机告警去重 + 一次性里程碑日志。
#[derive(Default)]
pub(super) struct FrameLogs {
    /// "快照未全部可画（缺驻留/缺材质）"已报过（下帧自愈，不逐帧刷屏）。
    /// 可逆旗标：pending 清零时复位重开（所以不用 [`OnceLog`]）。
    warned_pending: bool,
    pub(super) warned_no_camera: OnceLog,
    pub(super) warned_no_ambient: OnceLog,
    /// 首个完整 DrawList 的材质日志。
    override_logged: OnceLog,
    /// 首个取到灯光的帧（值随帧可变，只报"链路已通"）。
    pub(super) lights_logged: OnceLog,
    /// 多方向光/无方向光的状态告警。
    pub(super) warned_light_count: OnceLog,
    /// 各 compute 段首个 dispatch 帧。
    pub(super) cull_first_dispatch: OnceLog,
    pub(super) occlusion_first_dispatch: OnceLog,
    pub(super) lod_first_dispatch: OnceLog,
    pub(super) indirect_first_submit: OnceLog,
}

impl FrameLogs {
    /// DrawList 组装状态日志（暂缓行数由调用方推导：总行数 − 可绘槽号数，下游
    /// indirect 组装还要用）。暂缓是可反复的状态，进入第一帧报一次、清零复位
    /// 重开（所以不用 [`OnceLog`]）；首个完整 DrawList 的材质覆盖说明只报一次
    /// ——流式加载中途计数残缺，等零暂缓再报。
    pub(super) fn log_assembly_status(
        &mut self,
        pending_rows: usize,
        total_rows: usize,
        alpha_override: usize,
    ) {
        if pending_rows > 0 {
            if !self.warned_pending {
                self.warned_pending = true;
                info!(
                    "DrawList 暂缓 {}/{} 行（实例槽/mesh/材质未就绪，上传链自愈后消失）",
                    pending_rows, total_rows,
                );
            }
        } else {
            self.warned_pending = false;
        }
        if total_rows > 0 && pending_rows == 0 {
            self.override_logged.log(|| info!(
                "材质调试覆盖：{} 个 primitive 全部按不透明绘制——blend 关闭（alpha<1 的 {} 行照常画出，glTF BLEND 镜片在此列）；材质参数/贴图槽走材质环（每帧从容器现读）；着色模式按 ASH_RENDER_MODE（三态，见灯光 UBO 日志）",
                total_rows,
                alpha_override,
            ));
        }
    }
}
