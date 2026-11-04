//! 级联阴影的帧侧单元：取用束（ShadowInput）、shadow UBO 写入（级联拟合落账）与
//! shadow depth pass 录制输入准备（shadow_write_draw）。depth-only 管线与
//! 命令组装 compute 在 [`crate::render::passes::shadow`]。

use bevy::{ecs::system::SystemParam, math::Mat4, prelude::*};

use crate::{
    common::syntax::{OptionTierExt, ResultTierExt},
    render::{
        cascade_matrices, frustum_planes, pack_shadow_uniforms, InstanceTables, MeshPool,
        ShadowBuildPass, ShadowCascadeInput, ShadowCascades, ShadowPipeline, ShadowSettings,
        ShadowTargets, ShadowUniformsData, ShadowWriteDraw, SHADOW_CASCADES, SHADOW_MAP_SIZE,
    },
};

use super::{FrameLight, FrameView};

/// 级联阴影：资源族（级联 UBO/标记/命令环 + 深度图 + 描述符）+ depth-only 绘制
/// 管线 + 命令组装 compute + 开关。
#[derive(SystemParam)]
pub(super) struct ShadowInput<'w> {
    pub(super) targets: ResMut<'w, ShadowTargets>,
    pub(super) pipeline: Res<'w, ShadowPipeline>,
    pub(super) build_pass: Res<'w, ShadowBuildPass>,
    pub(super) settings: Res<'w, ShadowSettings>,
}

impl ShadowInput<'_> {
    /// shadow UBO：级联矩阵（log 分割拟合）+ 四×六平面（cull 内联标记与 shadow_stats
    /// CPU 参照同源）+ 分割带/相机基（composite 选级联）。相机缺席帧 = identity 矩阵。
    pub(super) fn write_ubo(
        &mut self,
        slot: usize,
        view: &FrameView,
        light: &FrameLight,
        scene_aabb: Option<(Vec3, Vec3)>,
    ) -> Result<(), BevyError> {
        let fit = match view.camera {
            Some(cam) => cascade_matrices(&ShadowCascadeInput {
                inv_view_proj: view.inv_view_proj,
                camera_pos: cam.pos,
                camera_forward: cam.forward(),
                near: cam.near,
                dir_to_light: Vec3::from_slice(&light.dir_to_light[..3]),
                scene_aabb,
            }),
            None => ShadowCascades {
                view_proj: [Mat4::IDENTITY; SHADOW_CASCADES],
                splits: [f32::INFINITY; SHADOW_CASCADES],
                texel_world: [1.0; SHADOW_CASCADES],
                inv_span: [0.0; SHADOW_CASCADES],
            },
        };
        let cascade_planes = fit.view_proj.map(|m| frustum_planes(&m));
        let uniform = ShadowUniformsData {
            cascade_view_proj: fit.view_proj,
            planes: std::array::from_fn(|i| cascade_planes[i / 6][i % 6]),
            splits: fit.splits,
            camera_forward: view
                .camera
                .map(|c| c.forward().extend(0.0).to_array())
                .unwrap_or([0.0; 4]),
            camera_pos: view.camera_pos4(),
            map_size: SHADOW_MAP_SIZE as f32,
            enabled: f32::from(self.settings.enabled),
            texel_world: fit.texel_world,
            inv_span: fit.inv_span,
        };
        self.targets
            .ubo(slot)
            .write(0, &pack_shadow_uniforms(&uniform))
            .or_fatal("shadow UBO 写失败，渲染链无法继续")
    }
}

/// shadow depth pass 输入：在场行 > 0 且开关开才录四级联绘制；关 = composite
/// 侧 shader 旁路 shadow 采样。
pub(super) fn shadow_write_draw(
    shadow: &ShadowInput,
    instances: &InstanceTables,
    pool: &MeshPool,
    slot: usize,
    count: u32,
) -> Result<Option<ShadowWriteDraw>, BevyError> {
    if !(shadow.settings.enabled && count > 0) {
        return Ok(None);
    }
    let targets = &shadow.targets;
    Ok(Some(ShadowWriteDraw {
        pipeline: shadow.pipeline.rec(),
        instance_set: instances.frame_set(slot),
        shadow_set: targets.frame_set(slot),
        views: std::array::from_fn(|c| targets.map_view(c)),
        images: std::array::from_fn(|c| targets.map_image(c)),
        commands: std::array::from_fn(|c| targets.command(slot, c)),
        command_count: count,
        vertex_buffer: pool
            .vertex_buffer()
            .ok_or_fatal("shadow 输入在场行非空但顶点池缺席——状态矛盾")?,
        index_buffer: pool
            .index_buffer()
            .ok_or_fatal("shadow 输入在场行非空但索引池缺席——状态矛盾")?,
    }))
}
