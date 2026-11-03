//! 场景侧的帧取值单元：场景取用束（SceneInput）、相机查询、本帧视图
//!（FrameView）、场景世界 AABB 与帧 UBO 写入。draw_frame 的
//! "帧槽 UBO 与环写入"段从这里取数。

use bevy::{
    camera::{Camera, Projection},
    ecs::system::SystemParam,
    light::{AmbientLight, GlobalAmbientLight},
    math::{Mat4, Quat},
    prelude::*,
};

use crate::{
    common::syntax::ResultTierExt,
    render::{
        conservative_world_aabb, pack_frame_uniforms, FrameUniformTable, FrameUniformsData,
        ImageCache, InstanceTables, MeshPool, RenderMode,
    },
    scene::{DrawListVerify, InstanceLedger, LodGroups, ResidencySettings},
};

use super::frame_logs::FrameLogs;
use super::lighting::{FrameLight, LightQuery};

/// 相机查询：投影 + 位姿 + 可选的每相机 `AmbientLight` 覆盖（挂了则压过全局资源）。
pub(super) type CameraQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static Projection,
        &'static GlobalTransform,
        Option<&'static AmbientLight>,
    ),
    With<Camera>,
>;

/// 场景侧输入：账本、资产容器与驻留、GPU 常驻绘制数据、相机与灯光。
#[derive(SystemParam)]
pub(super) struct SceneInput<'w, 's> {
    /// 稳定槽账本（跨帧增量维护；模型环待写集的独占消费点在本系统）。
    pub(super) ledger: ResMut<'w, InstanceLedger>,
    pub(super) std_materials: Res<'w, Assets<StandardMaterial>>,
    pub(super) pool: Res<'w, MeshPool>,
    pub(super) image_cache: Res<'w, ImageCache>,
    /// GPU 常驻绘制数据（对象表/各帧槽环）：本帧环写入与标记环读写的写点。
    pub(super) instance_tables: ResMut<'w, InstanceTables>,
    /// LOD 组登记处（只读——可绘锚解析与 LOD 表环写入的变体柄来源）。
    pub(super) lod_groups: Res<'w, LodGroups>,
    /// 驻留管理器设置（DrawList 可绘锚解析读 floor_anchor 开关）。
    pub(super) residency_settings: Res<'w, ResidencySettings>,
    /// 组装对照账面（BRP 开关；关闭时零成本）。
    pub(super) verify: ResMut<'w, DrawListVerify>,
    pub(super) cameras: CameraQuery<'w, 's>,
    pub(super) lights: LightQuery<'w, 's>,
    pub(super) ambient: Option<Res<'w, GlobalAmbientLight>>,
    /// 着色模式（AshHostPlugin 恒插入；overlay 单选钮与 BRP 写）。
    pub(super) render_mode: Res<'w, RenderMode>,
}

/// 非透视投影的 near 兜底：级联的 near/d 视深换算只对透视投影成立，正交下
/// 级联拟合不保证正确，此值只保证换算有定义。
const NON_PERSPECTIVE_NEAR: f32 = 100.0;

/// 相机位姿（相机在场帧才有）：遮挡跳变判定、级联拟合与光照 V 向量的输入。
#[derive(Clone, Copy)]
pub(super) struct CameraPose {
    pub(super) pos: Vec3,
    pub(super) rot: Quat,
    /// 投影 near（级联 z_ndc↔视深换算）。
    pub(super) near: f32,
}

impl CameraPose {
    pub(super) fn forward(&self) -> Vec3 {
        self.rot * Vec3::NEG_Z
    }
}

/// 本帧视图：相机矩阵 + 位姿 + 每相机环境光覆盖。相机缺席是场景装配问题，
/// 报一次后 identity 兜底、位姿 None（画面不可读但不崩帧循环）。矩阵乘序/
/// 列主序与 WGSL 端同一约定。
pub(super) struct FrameView<'a> {
    pub(super) view_proj: Mat4,
    /// clip→world 反投影（深度重建与级联角点反推；与 view_proj 同帧同源）。
    pub(super) inv_view_proj: Mat4,
    pub(super) camera: Option<CameraPose>,
    /// 相机组件 `AmbientLight`（若挂）压过全局 `GlobalAmbientLight` 资源。
    pub(super) ambient_override: Option<&'a AmbientLight>,
}

impl<'a> FrameView<'a> {
    pub(super) fn from_camera(cameras: &'a CameraQuery, logs: &mut FrameLogs) -> Self {
        match cameras.single() {
            Ok((projection, global, ambient_override)) => {
                let view_proj = projection.get_clip_from_view() * global.affine().inverse();
                let near = match projection {
                    Projection::Perspective(p) => p.near,
                    _ => NON_PERSPECTIVE_NEAR,
                };
                Self {
                    view_proj,
                    // glam 的 Mat4::inverse 通用逆，投影矩阵可逆性由投影构造保证
                    inv_view_proj: view_proj.inverse(),
                    camera: Some(CameraPose {
                        pos: global.translation(),
                        rot: global.rotation(),
                        near,
                    }),
                    ambient_override,
                }
            }
            Err(_) => {
                logs.warned_no_camera.log(|| {
                    warn!("相机缺席：view_proj 用 identity 兜底（画面不可读属预期，查相机装配）")
                });
                Self {
                    view_proj: Mat4::IDENTITY,
                    inv_view_proj: Mat4::IDENTITY,
                    camera: None,
                    ambient_override: None,
                }
            }
        }
    }

    /// 相机世界位置（w = 0；相机缺席帧 zeros 兜底）。
    pub(super) fn camera_pos4(&self) -> [f32; 4] {
        self.camera.map(|c| c.pos.extend(0.0).to_array()).unwrap_or([0.0; 4])
    }
}

/// 本帧槽主 UBO：相机矩阵 + 灯光 + 材质模式 + 相机世界位置（光照累积 V 向量）。
impl FrameUniformTable {
    /// impl 留在 driver 层而非 frame_uniform_table.rs：方法消费帧侧私有的
    /// FrameView/FrameLight（scene/lighting 两文件），放 render 侧会反向依赖
    /// （同四环写入的落位理由）。
    pub(super) fn write_frame_ubo(
        &mut self,
        slot: usize,
        view: &FrameView,
        light: &FrameLight,
        render_mode: RenderMode,
    ) -> Result<(), BevyError> {
        self.ubo(slot)
            .write(
                0,
                &pack_frame_uniforms(&FrameUniformsData {
                    view_proj: view.view_proj,
                    inv_view_proj: view.inv_view_proj,
                    dir_to_light: light.dir_to_light,
                    ambient_color: light.ambient_color,
                    light_color: light.light_color,
                    mode: render_mode.as_u32(),
                    camera_pos: view.camera_pos4(),
                }),
            )
            .or_fatal("帧 UBO 写失败，渲染链无法继续")
    }
}

/// 场景世界 AABB 并集（级联 far 外扩与深度盒向太阳侧延伸的输入）。
pub(super) fn scene_world_aabb(ledger: &InstanceLedger, pool: &MeshPool) -> Option<(Vec3, Vec3)> {
    let mut scene_aabb: Option<(Vec3, Vec3)> = None;
    for (_, row) in ledger.rows() {
        let Some(mesh_slot) = pool.resident(row.mesh.id()) else { continue };
        let (wmin, wmax) =
            conservative_world_aabb(&row.model, mesh_slot.local_aabb.0, mesh_slot.local_aabb.1);
        let (wmin, wmax) = (Vec3::from_array(wmin), Vec3::from_array(wmax));
        scene_aabb = Some(match scene_aabb {
            None => (wmin, wmax),
            Some((mn, mx)) => (mn.min(wmin), mx.max(wmax)),
        });
    }
    scene_aabb
}
