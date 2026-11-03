//! BRP handler 公共标准件：请求参数解析、资源取用与协议错误构造。协议层错误
//! 一律回 [`BrpError`]（参数类 = INVALID_PARAMS，内部失败与资源缺席 =
//! INTERNAL_ERROR），不 panic。

use std::fmt::Display;

use bevy::{
    ecs::component::Mutable,
    prelude::*,
    remote::{builtin_methods::parse, error_codes, BrpError},
};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// 参数缺失/不合法的统一错误（JSON-RPC 标准段 -32602）。
pub(crate) fn invalid_params(message: impl Into<String>) -> BrpError {
    BrpError {
        code: error_codes::INVALID_PARAMS,
        message: message.into(),
        data: None,
    }
}

/// 内部失败 → internal 协议错误，供 `map_err(internal_err("xx 读失败"))` 一行
/// 收口；消息 = `"{what}: {err}"`。
pub(crate) fn internal_err<E: Display>(what: &str) -> impl FnOnce(E) -> BrpError + '_ {
    move |e| BrpError::internal(format!("{what}: {e}"))
}

/// 取资源只读引用；缺席（如禁渲染宿主未插入）回 internal，不 panic。
pub(crate) fn res<T: Resource>(world: &World) -> Result<&T, BrpError> {
    world.get_resource::<T>().ok_or_else(missing::<T>)
}

/// 取资源可变引用；缺席回 internal，同 [`res`]。
pub(crate) fn res_mut<T: Resource<Mutability = Mutable>>(
    world: &mut World,
) -> Result<Mut<'_, T>, BrpError> {
    world.get_resource_mut::<T>().ok_or_else(missing::<T>)
}

fn missing<T>() -> BrpError {
    BrpError::internal(format!("资源缺席：{}", std::any::type_name::<T>()))
}

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

/// 开关写方法共用的 `{"enabled": bool}` 参数。
pub(crate) fn parse_enabled(params: Option<Value>) -> Result<bool, BrpError> {
    #[derive(serde::Deserialize)]
    struct EnabledParams {
        enabled: bool,
    }
    parse_params::<EnabledParams>(params, "params 缺失：需要 enabled").map(|p| p.enabled)
}
