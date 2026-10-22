//! BRP 资产内容变更方法族 + 运行期动态加载：测试驱动与
//! AI 通道的资产级变更入口。
//!
//! 分工（与 objects 同款钉子）：**官方 BRP 没有资产内容写通道**
//! ——反射 mutate 走 `world.mutate_component` 是组件值反射，够不着
//! `Assets<T>` 容器里的资产本体，本方法族自研；操作本体在
//! [`crate::scene::mechanism::mutate_asset`] 与 [`object_ops`] 的加载封装
//! （与将来 egui 面板共用的可复用实现），本模块只是协议薄壳。
//!
//! 动态加载语义：`load_mesh`/`load_image` 受理即返回加载中柄——
//! 路径失败经 [`AssetLoadFailedEvent`] 读侧收账（失败可见、行出队），到货经
//! flush 转换上传、readiness 计量对账；加载是异步链，本方法族不等到货。
//!
//! 机制与边界：handler 是 Bevy system，跑在 `RemoteLast`（Last 之后）按帧
//! 节拍执行——`Assets::get_mut` 触发的 `AssetEvent::Modified` 经 Messages
//! 双缓冲**下一帧**被读侧（asset_events）看见，再同帧完成失效与重传，画面
//! 次帧生效。安全边界：默认只绑 loopback、无鉴权，仅限本机。

use bevy::{prelude::*, remote::{error_codes, BrpError, BrpResult}};
use serde_json::{json, Value};

use crate::scene::mechanism::{
    load_image_slot, load_mesh_rebind, mutate_asset, spawn_with_loaded_mesh, AssetOpError,
    MutateAsset,
};

use super::invalid_params;
use super::objects::object_op_error;

use ash_macros::system;

/// `ash_renderer/mutate_asset` 的请求参数：逐项可选（缺省 = 不动），至少一项。
#[derive(serde::Deserialize)]
struct MutateAssetParams {
    /// 定位实体（取它身上的 Mesh3d / MeshMaterial3d 柄）。
    entity: Entity,
    /// 改材质 base_color（线性域 RGBA）。
    #[serde(default)]
    base_color: Option<[f32; 4]>,
    /// 改材质 metallic（[0,1]，8.2 PBR 面）。
    #[serde(default)]
    metallic: Option<f32>,
    /// 改材质 perceptual_roughness（[0,1]，8.2 PBR 面）。
    #[serde(default)]
    perceptual_roughness: Option<f32>,
    /// 换材质 base_color 贴图槽：拷源实体材质的槽柄（含清槽语义）。
    #[serde(default)]
    slot_base_color_from: Option<Entity>,
    /// 改 mesh 内容：POSITION 顶点绕原点均匀缩放系数。
    #[serde(default)]
    mesh_vertex_scale: Option<f32>,
    /// 改 image 内容：base_color_texture 像素按通道乘的色调（线性域 RGB）。
    #[serde(default)]
    image_tint: Option<[f32; 3]>,
}

/// `ash_renderer/mutate_asset`：改资产内容（同 AssetId 原位变更，触发
/// `AssetEvent::Modified`）。返回动过的资产身份读回（写后读对账用）。
#[system]
pub(crate) fn mutate_asset_cmd(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| {
        invalid_params(
            "params 缺失：需要 entity 与至少一项（base_color / slot_base_color_from / \
             mesh_vertex_scale / image_tint）",
        )
    })?;
    let p: MutateAssetParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    let req = MutateAsset {
        base_color: p.base_color,
        metallic: p.metallic,
        perceptual_roughness: p.perceptual_roughness,
        slot_base_color_from: p.slot_base_color_from,
        mesh_vertex_scale: p.mesh_vertex_scale,
        image_tint: p.image_tint,
    };
    let readback = mutate_asset(world, p.entity, req).map_err(asset_op_error)?;
    Ok(json!({
        "entity": p.entity.to_string(),
        "mutated": {
            "material": readback.material.map(|id| id.to_string()),
            "mesh": readback.mesh.map(|id| id.to_string()),
            "image": readback.image.map(|id| id.to_string()),
        },
    }))
}

/// `ash_renderer/load_mesh` 的请求参数：`entity`（换到既有实体）与 `source`
/// （克隆源结构挂新实体）二选一，都缺/都给 = 参数错。
#[derive(serde::Deserialize)]
struct LoadMeshParams {
    /// 资产路径（含 glTF 子资产标签，如 `models/.../x.gltf#Mesh0/Primitive0`）。
    path: String,
    #[serde(default)]
    entity: Option<Entity>,
    #[serde(default)]
    source: Option<Entity>,
    /// spawn 模式的平移偏移（仅 source 模式，缺省 [0,0,0]）。
    #[serde(default)]
    offset: Option<[f32; 3]>,
    #[serde(default)]
    name_suffix: Option<String>,
}

/// `ash_renderer/load_mesh`：运行期异步加载新 mesh（动态加载驱动）。
/// 受理即返回加载中柄的身份读回——pending/到货/失败经 `increment_stats` 的
/// readiness 段与失败日志对账（加载是异步链，本方法不等到货）。
#[system]
pub(crate) fn load_mesh_cmd(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params
        .ok_or_else(|| invalid_params("params 缺失：需要 path 与 entity|source 二选一"))?;
    let p: LoadMeshParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    if p.entity.is_some() == p.source.is_some() {
        return Err(invalid_params("entity 与 source 恰好给一个"));
    }
    let readback = if let Some(entity) = p.entity {
        load_mesh_rebind(world, entity, &p.path)
    } else {
        spawn_with_loaded_mesh(
            world,
            p.source.unwrap_or(Entity::PLACEHOLDER),
            &p.path,
            p.offset.map(Vec3::from_array).unwrap_or(Vec3::ZERO),
            p.name_suffix.as_deref().unwrap_or("（异步）"),
        )
    }
    .map_err(object_op_error)?;
    let material = world
        .get::<bevy::pbr::MeshMaterial3d<bevy::pbr::StandardMaterial>>(readback.entity)
        .map(|m| m.0.id().to_string())
        .unwrap_or_default();
    Ok(json!({
        "entity": readback.entity.to_string(),
        "mesh": readback.mesh.to_string(),
        "material": material,
        "state": "loading",
    }))
}

/// `ash_renderer/load_image` 的请求参数。
#[derive(serde::Deserialize)]
struct LoadImageParams {
    /// 贴图文件路径（相对资产根，如 `models/.../base_color.png`）。
    path: String,
    /// 定位实体（取它材质的 base_color 槽写入）。
    entity: Entity,
}

/// `ash_renderer/load_image`：运行期异步加载新贴图并换 base_color 槽
/// （贴图腿驱动）。共享材质语义：目标材质被原位改写，全部引用者同变。
#[system]
pub(crate) fn load_image_cmd(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要 path 与 entity"))?;
    let p: LoadImageParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    let image = load_image_slot(world, p.entity, &p.path).map_err(object_op_error)?;
    Ok(json!({
        "entity": p.entity.to_string(),
        "image": image.to_string(),
        "state": "loading",
    }))
}

/// `ash_renderer/reload_asset` 的请求参数。
#[derive(serde::Deserialize)]
struct ReloadAssetParams {
    /// 资产路径（含 glTF 子资产标签，与 load 方法同口径）。
    path: String,
}

/// `ash_renderer/reload_asset`：手动触发重载（热重载链的可复现驱动）。
/// `AssetServer::reload` 重走加载链——失败过的资产修复后由此复活（`Added`/
/// `Modified` 事件 → revive → flush 重试），文件监听是自动通道、本方法是
/// 测试与 AI 的确定性对照（不依赖 watcher 时序，受理即返回）。加载是异步链，
/// 本方法不等到货。
#[system]
pub(crate) fn reload_asset_cmd(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要 path"))?;
    let p: ReloadAssetParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    // reload 收 `impl Into<AssetPath>`，String → AssetPath 的 From 走 panic
    // 解析器——统一 try_parse 预校验，非法路径回协议错误
    let path = bevy::asset::AssetPath::try_parse(&p.path)
        .map_err(|e| invalid_params(format!("资产路径解析失败：{}: {e}", p.path)))?
        .into_owned();
    world.resource::<AssetServer>().reload(path);
    Ok(json!({
        "path": p.path,
        "state": "reloading",
    }))
}

/// 资产操作错误 → 协议错误码（AssetOpError 是 Tier② 语义的载体，只翻译
/// 不吞——原 message 原样带出）。
fn asset_op_error(err: AssetOpError) -> BrpError {
    match err {
        AssetOpError::EntityMissing(e) => BrpError::entity_not_found(e),
        AssetOpError::TargetNotPrimitive(e) => BrpError {
            code: error_codes::COMPONENT_ERROR,
            message: format!("目标实体 {e:?} 不是可渲染 primitive（缺 Mesh3d/MeshMaterial3d）"),
            data: None,
        },
        AssetOpError::SourceNotPrimitive(e) => BrpError {
            code: error_codes::COMPONENT_ERROR,
            message: format!("槽拷贝源实体 {e:?} 不是可渲染 primitive"),
            data: None,
        },
        AssetOpError::AssetMissing(what) => BrpError {
            code: error_codes::COMPONENT_ERROR,
            message: format!("资产容器查无：{what}"),
            data: None,
        },
        AssetOpError::MeshPositionsUnsupported => BrpError {
            code: error_codes::COMPONENT_ERROR,
            message: "mesh 缺 POSITION 属性或非 F32 三元布局，顶点缩放拒绝".to_owned(),
            data: None,
        },
        AssetOpError::ImageFormatUnsupported(format) => BrpError {
            code: error_codes::COMPONENT_ERROR,
            message: format!("贴图格式 {format:?} 非 4 字节 RGBA 族,像素 tint 拒绝"),
            data: None,
        },
        AssetOpError::InvalidParams(what) => invalid_params(what),
        AssetOpError::NothingToDo => invalid_params(
            "至少一项变更（base_color / metallic / perceptual_roughness / slot_base_color_from /              mesh_vertex_scale / image_tint）",
        ),
    }
}

/// `ash_renderer/load_world` 的请求参数。
#[derive(serde::Deserialize)]
struct LoadWorldParams {
    /// 场景资产路径（相对资产根，.gltf 或 .glb，如 `zero_day/zero_day_measure_one.glb`）。
    path: String,
}

/// `ash_renderer/load_world`：整场景动态进场——load Scene0 并 spawn
/// [`WorldAssetRoot`]，与 Startup 进场组（scene/content/world_asset）同链：
/// 到货后由 bevy_world_serialization 的 `world_instance_spawner` 反射展开成
/// primitive 实体树。多场景共存口：Startup 默认场景（Sponza）不动，其余
/// 场景经此进（7.2 zero_day / 7.3 Poly Haven / 7.4 复验全走这里）。
///
/// 异步语义与 load_mesh 同款：受理即返回加载中，本方法族不等到货——
/// 到货/失败看进场到货报告与 AssetLoadFailedEvent 读侧。
pub(crate) fn load_world_cmd(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要 path"))?;
    let p: LoadWorldParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    let handle = world
        .resource::<AssetServer>()
        // load 收 AssetPath<'static>（柄内保存），&str 须转 owned（load_mesh_rebind 同款钉）
        .load(bevy::gltf::GltfAssetLabel::Scene(0).from_asset(p.path.clone()));
    let entity = world.spawn(bevy::world_serialization::WorldAssetRoot(handle)).id();
    Ok(json!({
        "entity": entity.to_string(),
        "path": p.path,
        "state": "loading",
    }))
}
