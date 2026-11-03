//! 场景直控方法族：`scene_tree`（层级树只读）、`set_transform`（写 Transform）、
//! `set_camera`（相机轨道给值）、`sun_get`/`sun_set`（太阳方位/仰角读改）。写侧
//! 全部直调 scene 机制层的既有封装，与 egui 面板/鼠标拖拽同一实现、不养两份逻辑。

use bevy::{
    prelude::*,
    remote::{BrpError, BrpResult},
};
use serde_json::{json, Value};

use ash_macros::system;

use crate::scene::{
    apply_camera_command, apply_sun_rotation, apply_transform_edit, clamp_elevation,
    CameraCommandError, CameraOrbitCommand, EntityRow, SceneTreeIndex, SunOrbit, TransformEdit,
    TransformEditError, TREE_MAX_DEPTH,
};

use super::params::{invalid_params, parse_params, parse_params_or_default};

/// `ash_renderer/scene_tree`：场景层级树（只读）。索引与 egui 层级树页共用
/// [`SceneTreeIndex`]——CLI 树与面板树看到同一棵树、同一套"相关"语义。根 =
/// 相关且无父的实体，经 `Children` 递归；不相关子树整枝跳过（同面板）。
/// DefaultPlugins 注册的数百个内部实体被过滤，`relevant`/`total` 如实报出过滤量。
#[system]
pub(crate) fn scene_tree(In(_params): In<Option<Value>>, rows: Query<EntityRow>) -> BrpResult {
    let index = SceneTreeIndex::build(&rows);
    let tree: Vec<Value> = index
        .roots
        .iter()
        .map(|r| tree_node_json(*r, &index, 0))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "relevant": index.nodes.len(),
        "total": index.total,
        "roots": tree,
    }))
}

/// 递归序列化一个相关子树：子实体里不相关的整枝跳过（与 egui 树同语义）。
/// `entity` 不在表内或 `depth` 越过 [`TREE_MAX_DEPTH`] 回 internal——栈溢出走
/// abort，是"禁 panic"规则下唯一不可兜底的路径。
fn tree_node_json(entity: Entity, index: &SceneTreeIndex, depth: usize) -> BrpResult {
    if depth > TREE_MAX_DEPTH {
        return Err(BrpError::internal(format!(
            "scene_tree 层级超过深度上限 {TREE_MAX_DEPTH}（病态深树拒绝序列化）"
        )));
    }
    let node = index
        .nodes
        .get(&entity)
        .ok_or_else(|| BrpError::internal(format!("scene_tree 索引缺实体 {entity}——状态矛盾")))?;
    let children: Vec<Value> = node
        .children
        .iter()
        .filter(|c| index.nodes.contains_key(*c))
        .map(|c| tree_node_json(*c, index, depth + 1))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "entity": entity.to_string(),
        "label": node.label,
        "children": children,
    }))
}

/// `ash_renderer/set_transform` 的请求参数：逐组可选（缺省 = 不动该组），
/// 与 [`TransformEdit`] 逐组 Option 同形状。欧拉角 YXZ 度数，与属性面板同口径。
#[derive(serde::Deserialize)]
struct SetTransformParams {
    entity: Entity,
    #[serde(default)]
    translation: Option<[f32; 3]>,
    #[serde(default)]
    rotation_euler_deg: Option<[f32; 3]>,
    #[serde(default)]
    scale: Option<[f32; 3]>,
}

/// `ash_renderer/set_transform`：写一个实体的 Transform（独占 &mut World 直调
/// [`apply_transform_edit`]，不排队不走 UI）。至少一组量必须出现；成功返回
/// 写后的读回值（客户端可拿它做写后读对账）。
#[system]
pub(crate) fn set_transform(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: SetTransformParams = parse_params(params, "params 缺失：需要 entity 与至少一组量")?;
    let edit = TransformEdit {
        translation: p.translation.map(Vec3::from_array),
        rotation_euler_deg: p.rotation_euler_deg.map(Vec3::from_array),
        scale: p.scale.map(Vec3::from_array),
    };
    if edit.translation.is_none() && edit.rotation_euler_deg.is_none() && edit.scale.is_none() {
        return Err(invalid_params(
            "至少一组量（translation / rotation_euler_deg / scale）",
        ));
    }
    apply_transform_edit(world, p.entity, edit).map_err(|e| match e {
        TransformEditError::EntityMissing => BrpError::entity_not_found(p.entity),
        TransformEditError::TransformMissing => BrpError::component_not_present("Transform", p.entity),
    })?;
    // 刚写入成功，组件必在；查无 = 内部矛盾，回协议错误不 panic
    let t = world
        .get_entity(p.entity)
        .ok()
        .and_then(|er| er.get::<Transform>())
        .ok_or_else(|| BrpError::internal("apply_transform_edit 成功后 Transform 缺席——状态矛盾"))?;
    let (yaw, pitch, roll) = t.rotation.to_euler(bevy::math::EulerRot::YXZ);
    Ok(json!({
        "entity": p.entity.to_string(),
        "transform": {
            "translation": [t.translation.x, t.translation.y, t.translation.z],
            "rotation_euler_deg": [yaw.to_degrees(), pitch.to_degrees(), roll.to_degrees()],
            "scale": [t.scale.x, t.scale.y, t.scale.z],
        },
    }))
}

/// `ash_renderer/set_camera` 的请求参数：逐项可选（缺省 = 不动该量），
/// 与 [`CameraOrbitCommand`] 同形状；zoom 与滚轮缩放同语义（<1 推近）。
#[derive(serde::Deserialize)]
struct SetCameraParams {
    #[serde(default)]
    target: Option<[f32; 3]>,
    #[serde(default)]
    yaw_deg: Option<f32>,
    #[serde(default)]
    pitch_deg: Option<f32>,
    #[serde(default)]
    radius: Option<f32>,
    #[serde(default)]
    zoom: Option<f32>,
}

/// `ash_renderer/set_camera`：相机给值（轨道控制的 CLI 输入源，独占
/// &mut World 直调 [`apply_camera_command`]）。环绕（yaw/pitch）与推拉
///（radius/zoom）绝对值或系数覆盖，限位与鼠标输入共用；成功返回命令后的
/// 轨道参数读回值。
#[system]
pub(crate) fn set_camera(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: SetCameraParams = parse_params(params, "params 缺失：需要至少一项给值")?;
    if p.target.is_none()
        && p.yaw_deg.is_none()
        && p.pitch_deg.is_none()
        && p.radius.is_none()
        && p.zoom.is_none()
    {
        return Err(invalid_params(
            "至少一项给值（target / yaw_deg / pitch_deg / radius / zoom）",
        ));
    }
    let cmd = CameraOrbitCommand {
        target: p.target.map(Vec3::from_array),
        yaw_deg: p.yaw_deg,
        pitch_deg: p.pitch_deg,
        radius: p.radius,
        zoom: p.zoom,
    };
    let orbit = apply_camera_command(world, cmd).map_err(|e| {
        BrpError::component_error(match e {
            CameraCommandError::NoCamera => "World 里没有带 CameraOrbit 的相机",
            CameraCommandError::AmbiguousCamera => {
                "World 里带 CameraOrbit 的相机不止一台,机位目标不明确"
            }
        })
    })?;
    Ok(json!({
        "orbit": {
            "target": [orbit.target.x, orbit.target.y, orbit.target.z],
            "yaw_deg": orbit.yaw.to_degrees(),
            "pitch_deg": orbit.pitch.to_degrees(),
            "radius": orbit.radius,
        },
    }))
}

/// 方向光实体缺席 = 灯光装配问题（状态不可信，协议报 internal）。
fn no_sun() -> BrpError {
    BrpError::internal("方向光实体缺席——灯光装配未跑或实体被删")
}

/// `ash_renderer/sun_get`：方向光的 SunOrbit 球坐标与实际前向读数。
///
/// # Errors
/// 方向光实体缺席。
#[system]
pub(crate) fn sun_get(In(_params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let mut q = world.query_filtered::<(&SunOrbit, &GlobalTransform), With<DirectionalLight>>();
    let (orbit, tf) = q.iter(world).next().ok_or_else(no_sun)?;
    Ok(json!({
        "azimuth_deg": orbit.azimuth.to_degrees(),
        "elevation_deg": orbit.elevation.to_degrees(),
        "sun_dir": orbit.sun_dir().to_array(),
        "light_forward": tf.forward().to_array(),
    }))
}

/// `ash_renderer/sun_set` 参数（方位/仰角可选，缺省项保持原值）。
#[derive(serde::Deserialize, Default)]
struct SunSetParams {
    azimuth_deg: Option<f32>,
    elevation_deg: Option<f32>,
}

/// `ash_renderer/sun_set`：按方位角/仰角（度）改太阳方向，缺省项保持原值。
/// 写路径与中键拖拽共用 [`apply_sun_rotation`] 与仰角限位（[`clamp_elevation`]），
/// 两个入口不漂移；写后同帧生效。
///
/// # Errors
/// 参数非法数值、方向光实体缺席。
#[system]
pub(crate) fn sun_set(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: SunSetParams = parse_params_or_default(params)?;
    if p.azimuth_deg.is_some_and(|a| !a.is_finite()) || p.elevation_deg.is_some_and(|e| !e.is_finite()) {
        return Err(invalid_params("azimuth_deg/elevation_deg 必须是有限数值"));
    }
    let mut q = world.query_filtered::<(&mut SunOrbit, &mut Transform), With<DirectionalLight>>();
    let (orbit, tf) = q.iter_mut(world).next().ok_or_else(no_sun)?;
    let orbit = orbit.into_inner();
    if let Some(az) = p.azimuth_deg {
        orbit.azimuth = az.to_radians();
    }
    if let Some(el) = p.elevation_deg {
        orbit.elevation = clamp_elevation(el.to_radians());
    }
    apply_sun_rotation(orbit, tf.into_inner());
    Ok(json!({
        "azimuth_deg": orbit.azimuth.to_degrees(),
        "elevation_deg": orbit.elevation.to_degrees(),
        "sun_dir": orbit.sun_dir().to_array(),
    }))
}
