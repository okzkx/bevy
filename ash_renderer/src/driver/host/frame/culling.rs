//! 剔除链的帧侧单元：视锥剔除（FrustumCullInput + create_frustum_cull_draw）与
//! Hi-Z 遮挡剔除（OcclusionInput + OcclusionGate 跳变闸门 + OcclusionVerdict 判据 +
//! create_occlusion_draw）。compute 录制在 [`crate::render::passes::visibility`]。

use bevy::{
    ecs::system::SystemParam,
    math::{Quat, Vec3},
    prelude::*,
};

use crate::{
    common::syntax::{OnceLog, ResultTierExt},
    render::{
        frustum_planes, CullDiagnostics, CullDraw, CullPass, CullSettings, HizPyramid,
        InstanceTables, OcclusionDiagnostics, OcclusionDraw, OcclusionFallback, OcclusionPass,
        OcclusionSettings, MARK_SIZE,
    },
};

use super::ring::{count_marked, fill_u32, read_ring};
use super::PassFrame;

/// 视锥剔除：compute 管线 + 开关（egui/BRP 写，AshHostPlugin 恒插入）+ 诊断账面。
#[derive(SystemParam)]
pub(super) struct FrustumCullInput<'w> {
    pub(super) pass: Res<'w, CullPass>,
    pub(super) settings: Res<'w, CullSettings>,
    pub(super) diag: ResMut<'w, CullDiagnostics>,
}

/// 准备本帧视锥剔除录制输入：主视图或阴影开启时生成 dispatch，否则 host 填全可见。
/// 诊断读数只进计量；返回 (录制输入, 在场行可见数)。
pub(super) fn create_frustum_cull_draw(
    pf: &PassFrame,
    frustum_cull: &mut FrustumCullInput,
    shadow_enabled: bool,
    instances: &mut InstanceTables,
    first: &mut OnceLog,
) -> Result<(Option<CullDraw>, u32), BevyError> {
    let bound = pf.bound;
    if bound == 0 {
        return Ok((None, 0));
    }
    // shadow 开启时剔除 dispatch 也要产出 shadow 标记，两侧全关才可跳过。主视图开关随
    // push 进 shader：关 = marks 恒写 1，不覆盖下方 host 全可见填充的语义
    let draw = (frustum_cull.settings.enabled || shadow_enabled).then(|| {
        first.log(|| {
            info!(
                "视锥剔除 dispatch 首跑：bound {bound}，组 {}，平面随 112B push 进 compute",
                bound.div_ceil(64),
            )
        });
        frustum_cull.pass.draw(
            &frustum_planes(&pf.view_proj),
            bound,
            frustum_cull.settings.enabled,
        )
    });
    if !frustum_cull.settings.enabled {
        fill_u32(instances.mark_ring(pf.slot), bound as usize, 1)
            .or_fatal("剔除标记环全可见填充失败，渲染链无法继续")?;
    }
    let marks = read_ring(
        instances.mark_ring(pf.slot),
        bound as usize * MARK_SIZE as usize,
    )
    .or_fatal("剔除标记环读数失败，渲染链无法继续")?;
    let visible = count_marked(&marks, pf.live);
    frustum_cull.diag.enabled = frustum_cull.settings.enabled;
    frustum_cull.diag.bound = bound;
    frustum_cull.diag.marks_frame = pf.slot;
    frustum_cull.diag.rows_compared = pf.live.len();
    frustum_cull.diag.visible_count = visible;
    Ok((draw, visible))
}

/// Hi-Z 遮挡：测试管线 + 金字塔（归约链录制句柄 + warmup 走账）+ 开关 + 诊断账面。
#[derive(SystemParam)]
pub(super) struct OcclusionInput<'w> {
    pub(super) pass: Res<'w, OcclusionPass>,
    pub(super) hiz: ResMut<'w, HizPyramid>,
    pub(super) settings: Res<'w, OcclusionSettings>,
    pub(super) diag: ResMut<'w, OcclusionDiagnostics>,
}

/// 遮挡 fallback 的相机历史（`draw_frame` 的 `Local`，跨帧保持）：跳变判定用
/// "当前 vs 两帧前"——遮挡读的金字塔正是两帧前的深度。阈值见 occlusion 模块文档，
/// 连续缓慢环绕不触发。
#[derive(Default)]
pub(super) struct OcclusionGate {
    prev: Option<(Vec3, Quat)>,
    prev_prev: Option<(Vec3, Quat)>,
}

impl OcclusionGate {
    /// 推入本帧机位并返回"相对两帧前是否跳变"（历史不足 = 未就绪 = 视同跳变）。
    pub(super) fn push(&mut self, now: Option<(Vec3, Quat)>) -> bool {
        let jump = match (self.prev_prev, now) {
            (Some((p0, r0)), Some((p1, r1))) => {
                (p1 - p0).length() > 0.5 // 世界单位
                    || Quat::angle_between(r0, r1) > 15f32.to_radians()
            }
            _ => true, // 启动/相机缺席帧
        };
        self.prev_prev = self.prev.take();
        self.prev = now;
        jump
    }
}

/// 遮挡 dispatch 判据（dispatch 与否和 fallback 原因同出一处）。
enum OcclusionVerdict {
    Dispatch,
    /// 跳过 dispatch；原因取首个命中（空场 = None：无可判行，不算 fallback）。
    Skip(Option<OcclusionFallback>),
}

impl OcclusionVerdict {
    fn judge(
        enabled: bool,
        cull_enabled: bool,
        bound: u32,
        pyramid_ready: bool,
        camera_jumped: bool,
    ) -> Self {
        if !enabled {
            Self::Skip(Some(OcclusionFallback::Disabled))
        } else if !cull_enabled {
            Self::Skip(Some(OcclusionFallback::CullOff))
        } else if bound == 0 {
            Self::Skip(None)
        } else if !pyramid_ready {
            Self::Skip(Some(OcclusionFallback::PyramidWarmup))
        } else if camera_jumped {
            Self::Skip(Some(OcclusionFallback::CameraJump))
        } else {
            Self::Dispatch
        }
    }
}

/// 准备本帧遮挡剔除录制输入：判据任一命中 → 本帧跳过 dispatch——b3 保持视锥结果、
/// b6 host 填全可见（读数 = 遮挡剔除量 0）。诊断读数 b6 = 本帧槽两帧前 compute 输出或
/// host 填充。
pub(super) fn create_occlusion_draw(
    pf: &PassFrame,
    occlusion: &mut OcclusionInput,
    cull_enabled: bool,
    camera_jumped: bool,
    instances: &mut InstanceTables,
    first: &mut OnceLog,
) -> Result<Option<OcclusionDraw>, BevyError> {
    let bound = pf.bound;
    let verdict = OcclusionVerdict::judge(
        occlusion.settings.enabled,
        cull_enabled,
        bound,
        occlusion.hiz.ready(),
        camera_jumped,
    );
    let (dispatch, fallback) = match verdict {
        OcclusionVerdict::Dispatch => {
            first.log(|| info!(
                "Hi-Z 遮挡测试 dispatch 首跑（复用视锥 compute 链）：bound {bound}，\
                 组 {}，金字塔 {} 级（80B push：view_proj+screen+bound；b3=视锥∧遮挡，b6 遮挡独立标记）",
                bound.div_ceil(64),
                occlusion.hiz.levels(),
            ));
            let draw = occlusion.pass.draw(
                &pf.view_proj,
                pf.screen,
                bound,
                occlusion.hiz.occlusion_set(pf.slot),
            );
            (Some(draw), None)
        }
        OcclusionVerdict::Skip(reason) => {
            if bound > 0 {
                fill_u32(instances.occ_mark_ring(pf.slot), bound as usize, 1)
                    .or_fatal("遮挡标记环全可见填充失败，渲染链无法继续")?;
            }
            (None, reason)
        }
    };
    let mut visible = 0u32;
    if bound > 0 {
        let marks = read_ring(
            instances.occ_mark_ring(pf.slot),
            bound as usize * MARK_SIZE as usize,
        )
        .or_fatal("遮挡标记环读数失败，渲染链无法继续")?;
        visible = count_marked(&marks, pf.live);
    }
    let diag = &mut occlusion.diag;
    diag.enabled = occlusion.settings.enabled;
    diag.dispatched = dispatch.is_some();
    diag.fallback = fallback;
    diag.bound = bound;
    diag.marks_frame = pf.slot;
    diag.rows_compared = pf.live.len();
    diag.occlusion_visible_count = visible;
    Ok(dispatch)
}

#[cfg(test)]
mod tests {
    use super::OcclusionGate;
    use bevy::math::{Quat, Vec3};

    /// 比较"本帧 vs 两帧前"，阈值作用于两帧累计量：历史不足、累计微动超阈、单帧大跳
    /// 都触发，静止不触发。
    #[test]
    fn occlusion_gate_jump_thresholds() {
        let mut gate = OcclusionGate::default();
        let p0 = Vec3::new(4.0, 2.0, 6.0);
        let r0 = Quat::IDENTITY;
        // 机位历史不足（前两帧）= 未就绪 = 视同跳变（遮挡测试让路）
        assert!(gate.push(Some((p0, r0))), "无历史");
        assert!(gate.push(Some((p0, r0))), "只有一帧历史");
        // 静止相机：不触发
        assert!(!gate.push(Some((p0, r0))), "静止两帧 = 稳定");
        // 单帧小位移（0.3 < 0.5，窗口累计 0.3）：不触发
        assert!(!gate.push(Some((p0 + Vec3::new(0.3, 0.0, 0.0), r0))));
        // 连续微动两帧窗口累计 0.6 > 0.5：触发（金字塔内容已落后累计量）
        assert!(gate.push(Some((p0 + Vec3::new(0.6, 0.0, 0.0), r0))));
        // 大位移（单帧 2.0 > 0.5）触发
        assert!(gate.push(Some((p0 + Vec3::new(2.0, 0.0, 0.0), r0))));
        // 大旋转（16° > 15°）触发
        assert!(gate.push(Some((p0, Quat::from_rotation_y(16f32.to_radians())))));
        // 历史对齐（同位姿再推两帧，清掉大位移残留，使比较基准 = 16° 姿态）
        assert!(gate.push(Some((p0, Quat::from_rotation_y(16f32.to_radians())))));
        assert!(!gate.push(Some((p0, Quat::from_rotation_y(16f32.to_radians())))));
        // 小旋转（两帧窗累计 2° ≤ 15°）不触发
        assert!(!gate.push(Some((p0, Quat::from_rotation_y(14f32.to_radians())))));
        // 相机缺席帧（None = 场景装配问题）视同未就绪
        assert!(gate.push(None));
    }
}
