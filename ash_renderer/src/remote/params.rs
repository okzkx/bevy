//! BRP handler 公共标准件：请求参数解析与主相机矩阵。协议层错误一律回
//! [`BrpError`]（参数类 = INVALID_PARAMS，相机缺席 = COMPONENT_ERROR），不 panic。

use bevy::{
    camera::{Camera, Projection},
    prelude::*,
    remote::{builtin_methods::parse, BrpError},
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::invalid_params;

/// 必填参数：缺失回 `hint`（写明需要哪些字段），反序列化失败回 serde 原文。
pub(crate) fn parse_params<T: DeserializeOwned>(
    params: Option<Value>,
    hint: &str,
) -> Result<T, BrpError> {
    parse(params.ok_or_else(|| invalid_params(hint))?)
}

/// 可省参数：缺失或 `null` = `T::default()`，其余同 [`parse_params`]。
pub(crate) fn parse_params_or_default<T: DeserializeOwned + Default>(
    params: Option<Value>,
) -> Result<T, BrpError> {
    match params {
        Some(value) if !value.is_null() => parse(value),
        _ => Ok(T::default()),
    }
}

/// 场景无唯一相机的统一错误；`consequence` 写明哪项读数因此算不出。
pub(crate) fn no_camera(consequence: &str) -> BrpError {
    BrpError::component_error(format!("World 里没有相机——{consequence}"))
}

/// 主相机 view_proj（clip_from_view × view_from_world，与 draw_frame 同一乘序），
/// 供 CPU 参照现算视锥/投屏。相机缺席或不唯一回 [`no_camera`]。
pub(crate) fn brp_view_proj(world: &mut World, consequence: &str) -> Result<Mat4, BrpError> {
    let mut cameras = world.query_filtered::<(&Projection, &GlobalTransform), With<Camera>>();
    let (projection, global) = cameras.single(world).map_err(|_| no_camera(consequence))?;
    Ok(projection.get_clip_from_view() * global.affine().inverse())
}

/// 投屏矩形钳进 `[0, screen]`，与 lod_select.wgsl / occlusion.wgsl 同式（离屏
/// 部分不渲染，完全离屏 → 零尺寸）。
///
/// 坑：[`crate::render::aabb_screen_rect`] 返回未钳制矩形（其他调用方依赖该
/// 语义），CPU 参照拿它算尺寸/选 mip 前必须先过本函数，否则离屏/半离屏物体
/// 尺寸偏大，与 GPU 判档/选层对不上。
pub(crate) fn clamp_rect_to_screen(px_min: Vec2, px_max: Vec2, screen: [f32; 2]) -> (Vec2, Vec2) {
    let screen = Vec2::from_array(screen);
    (px_min.clamp(Vec2::ZERO, screen), px_max.clamp(Vec2::ZERO, screen))
}
