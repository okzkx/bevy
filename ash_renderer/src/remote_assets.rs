//! BRP 资产内容变更方法族（4.2.4）：测试驱动与 AI 通道的资产级变更入口。
//!
//! 分工（与 4.1.5 remote_objects 同款钉子）：**官方 BRP 没有资产内容写通道**
//! ——反射 mutate 走 `world.mutate_component` 是组件值反射，够不着
//! `Assets<T>` 容器里的资产本体，本方法族自研；操作本体在
//! [`crate::scene::mechanism::mutate_asset`]（与将来 egui 面板共用的可复用
//! 实现），本模块只是协议薄壳。
//!
//! 机制与边界：handler 是 Bevy system，跑在 `RemoteLast`（Last 之后）按帧
//! 节拍执行——`Assets::get_mut` 触发的 `AssetEvent::Modified` 经 Messages
//! 双缓冲**下一帧**被读侧（asset_events）看见，再同帧完成失效与重传，画面
//! 次帧生效。安全边界：默认只绑 loopback、无鉴权，仅限本机。

use bevy::{prelude::*, remote::{error_codes, BrpError, BrpResult}};
use serde_json::{json, Value};

use crate::scene::mechanism::{mutate_asset, AssetOpError, MutateAsset};

use super::remote::invalid_params;

use ash_macros::system;

/// `ash_renderer/mutate_asset` 的请求参数：逐项可选（缺省 = 不动），至少一项。
#[derive(serde::Deserialize)]
struct MutateAssetParams {
    /// 定位实体（取它身上的 Mesh3d / MeshMaterial3d 柄）。
    entity: Entity,
    /// 改材质 base_color（线性域 RGBA）。
    #[serde(default)]
    base_color: Option<[f32; 4]>,
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
        AssetOpError::NothingToDo => invalid_params(
            "至少一项变更（base_color / slot_base_color_from / mesh_vertex_scale / image_tint）",
        ),
    }
}
