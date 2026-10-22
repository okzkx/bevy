//! BRP 远程直控通道：宿主 World 经 HTTP JSON-RPC 暴露在
//! 127.0.0.1:15702（bevy_remote 默认端口），AI/脚本不开 UI 即可"查询 → 改
//! Transform → 相机环绕推拉"。读侧零自研——官方 inspection 方法族
//!（`world.query`/`world.inspect`/`world.summarize` 等）与内置 CRUD 随
//! [`RemotePlugin`] 默认注册，`rpc.discover` 可列全量；写侧全部复用既有封装
//!（[`TransformEdit`] 的 [`apply_transform_edit`]、[`apply_camera_command`]），
//! 与 egui 面板同款实现、不养两份逻辑。
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`mouse`] | 虚拟鼠标：窗口无关鼠标输入源，注入 `Messages<WindowEvent>` 与 bevy_winit 同层 |
//! | [`objects`] | 对象生命周期方法族：spawn 克隆/换柄写方法 + increment_stats 统计读数 |
//! | [`shapes`] | 测试实体方法族（6.1.1）：spawn_shape 五形状 + 颜色 → Lambert 材质（遮挡对/墙体样本载具） |
//! | [`generate`] | 常驻大场景方法族（5.4）：generate_scene 程序化铺实例；chunk 身份与整块增删（5.4.2） |
//! | [`assets`] | 资产内容变更 + 运行期动态加载：mutate/load_mesh/load_image/reload |
//! | [`cull`] | 剔除读数与开关（5.2）：cull_stats 对照 + cull_set_enabled 开关 |
//! | [`draw`] | indirect 闭环读数（5.3）：draw_stats 绘制调用/命令条数/不可见计数 |
//! | [`perf`] | 性能读数与变更形态（5.5）：perf_stats 分位数/分段/显存 + perf_samples 原始导出 + workload 变更形态开关 |
//! | [`monitor`] | 持续监控读数（7.5）：monitor_stats fps/内存/显存堆级预算 + 内部账，与 egui 监控窗口同账；frame_sync 帧率同步开关读写（7.6） |
//! | [`residency`] | 驻留管理器读数与设置（6.3）：residency_stats 预算/实占/逐资产清单 + residency_set 开关/预算/半径 |
//! | [`gap`] | GPU 缺口请求读数与模式（6.6）：gap_stats 聚合 + gap_census 逐槽位图 + gap_set 三态模式 |
//!
//! 机制与边界（侦察笔记《bevy_remote：BRP远程协议与自定义方法》）：handler 本身
//! 就是一个 Bevy system，跑在 `RemoteLast`（Last 之后）按帧节拍执行——写值下一帧
//! 生效（BRP 天然以帧为单位量化延迟），参数即请求 `params` 的 JSON、返回值即响应。
//! 安全边界：默认只绑 loopback、完全无鉴权，仅限本机调试使用，不改绑对外地址。

pub mod assets;
pub mod cluster;
pub mod cull;
pub mod draw;
pub mod generate;
pub mod gap;
pub mod lod;
pub mod monitor;
pub mod mouse;
pub mod objects;
pub mod perf;
pub mod residency;
pub mod ring_census;
pub mod shapes;

use std::collections::HashMap;

use bevy::{
    prelude::*,
    remote::{error_codes, BrpError, BrpResult, RemotePlugin},
    remote::http::RemoteHttpPlugin,
};
use serde_json::{json, Value};

use self::{
    assets::{load_image_cmd, load_mesh_cmd, load_world_cmd, mutate_asset_cmd, reload_asset_cmd},
    cull::{cull_set_enabled, cull_stats, hiz_census, occlusion_probe, occlusion_set_enabled},
    cluster::{cluster_census, light_remove, light_spawn, light_stats, light_update, transparent_sort_check},
    draw::{draw_stats, drawlist_verify_set},
    generate::{chunk_census, chunk_despawn, generate_chunks_cmd, generate_scene},
    gap::{gap_census, gap_set, gap_stats},
    lod::{lod_census, lod_set_enabled, lod_set_params},
    monitor::{frame_sync_cmd, monitor_stats, set_render_mode},
    mouse::{mouse_button, mouse_move, mouse_status, mouse_wheel},
    objects::{increment_stats, replace_handles, spawn_primitive},
    residency::{residency_set, residency_stats},
    ring_census::ring_census,
    perf::{perf_samples, perf_stats, workload},
    shapes::{spawn_lod, spawn_shape},
};

use crate::{
    overlay::{transform_edit::{apply_transform_edit, TransformEdit, TransformEditError}, EntityRow, is_relevant, node_label},
    scene::mechanism::{apply_camera_command, CameraCommandError, CameraOrbitCommand},
};

use ash_macros::system;

/// BRP 远程通道插件：装协议 + HTTP 传输 + 场景语义
/// 方法族（本模块三个 + mouse 虚拟鼠标四个 + objects 对象生命周期与统计 +
/// shapes 测试实体 + assets 资产变更与动态加载）。宿主 main 照插件清单惯例
/// 只 `add_plugins`，方法注册收在本插件内。
pub struct AshRemotePlugin;

impl Plugin for AshRemotePlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            RemotePlugin::default()
                .with_method_main("ash_renderer/scene_tree", scene_tree)
                .with_method_main("ash_renderer/set_transform", set_transform)
                .with_method_main("ash_renderer/set_camera", set_camera)
                .with_method_main("ash_renderer/spawn_primitive", spawn_primitive)
                .with_method_main("ash_renderer/spawn_shape", spawn_shape)
                .with_method_main("ash_renderer/spawn_lod", spawn_lod)
                .with_method_main("ash_renderer/generate_scene", generate_scene)
                .with_method_main("ash_renderer/generate_chunks", generate_chunks_cmd)
                .with_method_main("ash_renderer/chunk_census", chunk_census)
                .with_method_main("ash_renderer/chunk_despawn", chunk_despawn)
                .with_method_main("ash_renderer/replace_handles", replace_handles)
                .with_method_main("ash_renderer/mutate_asset", mutate_asset_cmd)
                .with_method_main("ash_renderer/load_mesh", load_mesh_cmd)
                .with_method_main("ash_renderer/load_image", load_image_cmd)
                .with_method_main("ash_renderer/load_world", load_world_cmd)
                .with_method_main("ash_renderer/reload_asset", reload_asset_cmd)
                .with_method_main("ash_renderer/increment_stats", increment_stats)
                .with_method_main("ash_renderer/mouse_move", mouse_move)
                .with_method_main("ash_renderer/mouse_button", mouse_button)
                .with_method_main("ash_renderer/mouse_wheel", mouse_wheel)
                .with_method_main("ash_renderer/mouse_status", mouse_status)
                .with_method_main("ash_renderer/cull_stats", cull_stats)
                .with_method_main("ash_renderer/light_spawn", light_spawn)
                .with_method_main("ash_renderer/light_update", light_update)
                .with_method_main("ash_renderer/light_remove", light_remove)
                .with_method_main("ash_renderer/light_stats", light_stats)
                .with_method_main("ash_renderer/cluster_census", cluster_census)
                .with_method_main("ash_renderer/transparent_sort_check", transparent_sort_check)
                .with_method_main("ash_renderer/hiz_census", hiz_census)
                .with_method_main("ash_renderer/occlusion_probe", occlusion_probe)
                .with_method_main("ash_renderer/occlusion_set_enabled", occlusion_set_enabled)
                .with_method_main("ash_renderer/cull_set_enabled", cull_set_enabled)
                .with_method_main("ash_renderer/lod_census", lod_census)
                .with_method_main("ash_renderer/lod_set_enabled", lod_set_enabled)
                .with_method_main("ash_renderer/lod_set_params", lod_set_params)
                .with_method_main("ash_renderer/draw_stats", draw_stats)
                .with_method_main("ash_renderer/drawlist_verify_set", drawlist_verify_set)
                .with_method_main("ash_renderer/residency_stats", residency_stats)
                .with_method_main("ash_renderer/residency_set", residency_set)
                .with_method_main("ash_renderer/gap_stats", gap_stats)
                .with_method_main("ash_renderer/gap_census", gap_census)
                .with_method_main("ash_renderer/gap_set", gap_set)
                .with_method_main("ash_renderer/perf_stats", perf_stats)
                .with_method_main("ash_renderer/perf_samples", perf_samples)
                .with_method_main("ash_renderer/workload", workload)
                .with_method_main("ash_renderer/monitor_stats", monitor_stats)
                .with_method_main("ash_renderer/frame_sync", frame_sync_cmd)
                .with_method_main("ash_renderer/set_render_mode", set_render_mode)
                .with_method_main("ash_renderer/ring_census", ring_census),
            RemoteHttpPlugin::default(),
        ));
    }
}

/// 参数缺失/不合法的统一错误（JSON-RPC 标准段 -32602）。mouse 族方法
/// （[`mouse`]）复用同款，协议面错误口径只有一处。
pub(crate) fn invalid_params(message: impl Into<String>) -> BrpError {
    BrpError {
        code: error_codes::INVALID_PARAMS,
        message: message.into(),
        data: None,
    }
}

/// `ash_renderer/scene_tree`：场景层级树（只读）。行数据、相关性过滤与节点
/// 命名复用 egui 层级树窗口的同款（[`EntityRow`]/`is_relevant`/`node_label`）
/// ——CLI 树与面板树看到同一棵树、同一套"相关"语义。根 = 相关且无父的实体，
/// 经 `Children` 递归；不相关子树整枝跳过（同面板）。DefaultPlugins 注册的
/// 数百个内部实体被过滤，`relevant`/`total` 如实报出过滤量。
#[system]
fn scene_tree(In(_params): In<Option<Value>>, rows: Query<EntityRow>) -> BrpResult {
    let mut total = 0usize;
    // 相关实体表：entity → (节点文本, 子实体清单, 是否有父)
    let mut relevant: HashMap<Entity, (String, Vec<Entity>, bool)> = HashMap::new();
    for (entity, name, children, has_parent, cam, light, mesh, mat, root, local, global) in &rows {
        total += 1;
        if is_relevant(name, children, has_parent, cam, light, mesh, mat, root, local, global) {
            let label = node_label(entity, name, cam, light, mesh.is_some(), mat.is_some(), root);
            relevant.insert(
                entity,
                (
                    label,
                    children.map(|c| c.iter().collect()).unwrap_or_default(),
                    has_parent,
                ),
            );
        }
    }
    let mut roots: Vec<Entity> = relevant
        .iter()
        .filter(|(_, (_, _, has_parent))| !*has_parent)
        .map(|(entity, _)| *entity)
        .collect();
    roots.sort_by_key(|e| e.index());
    let relevant_count = relevant.len();
    let tree: Vec<Value> = roots
        .iter()
        .map(|r| tree_node_json(*r, &relevant, 0))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "relevant": relevant_count,
        "total": total,
        "roots": tree,
    }))
}

/// 递归深度上限：病态深树在栈溢出前显式拒绝——栈溢出走 abort 是
/// "禁 panic"规则下唯一不可兜底路径，深度越限回协议错误。上限 1024 远超真实
/// 场景层级，同时远低于典型线程栈的溢出深度。
const TREE_MAX_DEPTH: usize = 1024;

/// 递归序列化一个相关子树：子实体里不相关的整枝跳过（与 egui 树同语义）。
/// 调用方保证 `entity` 在表内；`depth` 越过 [`TREE_MAX_DEPTH`] 回协议错误。
fn tree_node_json(
    entity: Entity,
    relevant: &HashMap<Entity, (String, Vec<Entity>, bool)>,
    depth: usize,
) -> BrpResult {
    if depth > TREE_MAX_DEPTH {
        return Err(BrpError {
            code: error_codes::INTERNAL_ERROR,
            message: format!("scene_tree 层级超过深度上限 {TREE_MAX_DEPTH}（病态深树拒绝序列化）"),
            data: None,
        });
    }
    let (label, children, _) = &relevant[&entity];
    let children: Vec<Value> = children
        .iter()
        .filter(|c| relevant.contains_key(*c))
        .map(|c| tree_node_json(*c, relevant, depth + 1))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "entity": entity.to_string(),
        "label": label,
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
/// [`apply_transform_edit`]，不排队不走 UI——Transform 编辑"唯一实现"复用点）。
/// 至少一组量必须出现；成功返回写后的读回值（客户端可拿它做写后读对账）。
#[system]
fn set_transform(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要 entity 与至少一组量"))?;
    let p: SetTransformParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
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
    match apply_transform_edit(world, p.entity, edit) {
        Ok(()) => {}
        Err(TransformEditError::EntityMissing) => {
            return Err(BrpError::entity_not_found(p.entity));
        }
        Err(TransformEditError::TransformMissing) => {
            return Err(BrpError::component_not_present("Transform", p.entity));
        }
    }
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
fn set_camera(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要至少一项给值"))?;
    let p: SetCameraParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
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
    let orbit = match apply_camera_command(world, cmd) {
        Ok(orbit) => orbit,
        Err(CameraCommandError::NoCamera) => {
            return Err(BrpError {
                code: error_codes::COMPONENT_ERROR,
                message: "World 里没有带 CameraOrbit 的相机".to_owned(),
                data: None,
            });
        }
        Err(CameraCommandError::AmbiguousCamera) => {
            return Err(BrpError {
                code: error_codes::COMPONENT_ERROR,
                message: "World 里带 CameraOrbit 的相机不止一台,机位目标不明确".to_owned(),
                data: None,
            });
        }
    };
    Ok(json!({
        "orbit": {
            "target": [orbit.target.x, orbit.target.y, orbit.target.z],
            "yaw_deg": orbit.yaw.to_degrees(),
            "pitch_deg": orbit.pitch.to_degrees(),
            "radius": orbit.radius,
        },
    }))
}
