//! BRP 资产内容变更与运行期动态加载方法族。官方 BRP 没有资产内容写通道（反射
//! `world.mutate_component` 只改组件值，够不着 `Assets<T>` 里的资产本体），故自研；
//! 操作本体在 scene 机制层（[`mutate_asset`] 与对象操作的加载封装，egui 面板
//! 共用），本模块只是协议薄壳。
//!
//! 时序：`Assets::get_mut` 触发的 `AssetEvent::Modified` 下一帧才被资产事件读侧
//! 看见，同帧完成失效与重传，画面次帧生效。加载方法受理即返回加载中柄、不等
//! 到货——路径失败经 `AssetLoadFailedEvent` 读侧收账，到货经上传链与 readiness
//! 计量对账。

use bevy::{
    prelude::*,
    remote::{BrpError, BrpResult},
};
use serde_json::{json, Value};

use crate::scene::{
    load_image_slot, load_mesh_rebind, mutate_asset, spawn_with_loaded_mesh, AssetOpError,
    MutateAsset,
};

use super::objects::{material_id_string, object_op_error};
use crate::remote::common::params::{invalid_params, parse_params, res};

use ash_macros::system;

/// `ash_renderer/mutate_asset` 的请求参数：逐项可选（缺省 = 不动），至少一项。
#[derive(serde::Deserialize)]
struct MutateAssetParams {
    /// 定位实体（取它身上的 Mesh3d / MeshMaterial3d 柄）。
    entity: Entity,
    /// 改材质 base_color（线性域 RGBA）。
    #[serde(default)]
    base_color: Option<[f32; 4]>,
    /// 改材质 metallic（`[0,1]`，PBR 面）。
    #[serde(default)]
    metallic: Option<f32>,
    /// 改材质 perceptual_roughness（`[0,1]`，PBR 面）。
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
    let p: MutateAssetParams = parse_params(
        params,
        "params 缺失：需要 entity 与至少一项（base_color / slot_base_color_from / \
         mesh_vertex_scale / image_tint）",
    )?;
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
    /// spawn 模式的平移偏移（仅 source 模式，缺省 `[0,0,0]`）。
    #[serde(default)]
    offset: Option<[f32; 3]>,
    #[serde(default)]
    name_suffix: Option<String>,
}

/// `ash_renderer/load_mesh`：运行期异步加载新 mesh。受理即返回加载中柄的身份
/// 读回——pending/到货/失败经 `increment_stats` 的 readiness 段与失败日志对账。
#[system]
pub(crate) fn load_mesh_cmd(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: LoadMeshParams = parse_params(params, "params 缺失：需要 path 与 entity|source 二选一")?;
    let readback = match (p.entity, p.source) {
        (Some(entity), None) => load_mesh_rebind(world, entity, &p.path),
        (None, Some(source)) => spawn_with_loaded_mesh(
            world,
            source,
            &p.path,
            p.offset.map(Vec3::from_array).unwrap_or(Vec3::ZERO),
            p.name_suffix.as_deref().unwrap_or("（异步）"),
        ),
        _ => return Err(invalid_params("entity 与 source 恰好给一个")),
    }
    .map_err(object_op_error)?;
    Ok(json!({
        "entity": readback.entity.to_string(),
        "mesh": readback.mesh.to_string(),
        "material": material_id_string(world, readback.entity),
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

/// `ash_renderer/load_image`：运行期异步加载新贴图并换 base_color 槽。
/// 共享材质语义：目标材质被原位改写，全部引用者同变。
#[system]
pub(crate) fn load_image_cmd(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: LoadImageParams = parse_params(params, "params 缺失：需要 path 与 entity")?;
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

/// `ash_renderer/reload_asset`：手动触发重载。`AssetServer::reload` 重走加载链，
/// 失败过的资产修复后由此复活（`Added`/`Modified` 事件 → 重新入队 → 上传重试）；
/// 与文件监听相比不依赖 watcher 时序，供测试确定性复现。受理即返回，不等到货。
#[system]
pub(crate) fn reload_asset_cmd(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: ReloadAssetParams = parse_params(params, "params 缺失：需要 path")?;
    // 坑：String → AssetPath 的 From 解析失败会 panic，先 try_parse 预校验，
    // 非法路径回协议错误
    let path = bevy::asset::AssetPath::try_parse(&p.path)
        .map_err(|e| invalid_params(format!("资产路径解析失败：{}: {e}", p.path)))?
        .into_owned();
    res::<AssetServer>(world)?.reload(path);
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
        AssetOpError::TargetNotPrimitive(e) => BrpError::component_error(format!(
            "目标实体 {e:?} 不是可渲染 primitive（缺 Mesh3d/MeshMaterial3d）"
        )),
        AssetOpError::SourceNotPrimitive(e) => {
            BrpError::component_error(format!("槽拷贝源实体 {e:?} 不是可渲染 primitive"))
        }
        AssetOpError::AssetMissing(what) => {
            BrpError::component_error(format!("资产容器查无：{what}"))
        }
        AssetOpError::MeshPositionsUnsupported => {
            BrpError::component_error("mesh 缺 POSITION 属性或非 F32 三元布局，顶点缩放拒绝")
        }
        AssetOpError::ImageFormatUnsupported(format) => BrpError::component_error(format!(
            "贴图格式 {format:?} 非 4 字节 RGBA 族,像素 tint 拒绝"
        )),
        AssetOpError::InvalidParams(what) => invalid_params(what),
        AssetOpError::NothingToDo => invalid_params(
            "至少一项变更（base_color / metallic / perceptual_roughness / slot_base_color_from / \
             mesh_vertex_scale / image_tint）",
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
/// `WorldAssetRoot`，与 Startup 进场同链：到货后由 bevy_world_serialization 反射
/// 展开成 primitive 实体树。Startup 默认场景不动，其余场景经此叠加进场。
/// 受理即返回加载中，不等到货——到货/失败看进场到货报告与加载失败事件读侧。
#[system]
pub(crate) fn load_world_cmd(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: LoadWorldParams = parse_params(params, "params 缺失：需要 path")?;
    let handle = res::<AssetServer>(world)?
        // load 收 AssetPath<'static>（柄内保存），须传 owned 路径
        .load(bevy::gltf::GltfAssetLabel::Scene(0).from_asset(p.path.clone()));
    let entity = world
        .spawn(bevy::world_serialization::WorldAssetRoot(handle))
        .id();
    Ok(json!({
        "entity": entity.to_string(),
        "path": p.path,
        "state": "loading",
    }))
}
