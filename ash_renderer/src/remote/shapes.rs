//! BRP 测试实体方法族（6.1.1）：五形状 + 颜色 → 自动应用/按色复用
//! Lambert 材质的单个测试体——遮挡剔除（Hi-Z）段的遮挡对/墙体样本
//! 载具与通用测试能力。协议薄壳：操作本体在
//! [`crate::scene::mechanism::test_entities`]（与 5.4 生成器同区同构的
//! 机制层），本模块只做参数解析与协议错误映射。

use bevy::{prelude::*, remote::BrpResult};
use serde_json::{json, Value};

use crate::scene::mechanism::{spawn_test_shape, TestShape};

use super::{invalid_params, objects::object_op_error};

use ash_macros::system;

/// `ash_renderer/spawn_shape` 的请求参数：形状必填，其余可选（缺省 =
/// 灰白色 / 原点 / 零旋转 / 单位缩放 / 默认名"测试体-\<shape\>"）。
#[derive(serde::Deserialize)]
struct SpawnShapeParams {
    shape: TestShape,
    /// sRGB [0,1] 三元组；材质只设 base_color（Lambert 路径）。
    #[serde(default)]
    color: Option<[f32; 3]>,
    #[serde(default)]
    translation: Option<[f32; 3]>,
    /// 欧拉角 YXZ 度数（与 set_transform/属性面板同口径）。
    #[serde(default)]
    rotation_euler_deg: Option<[f32; 3]>,
    /// 非均匀缩放直出墙体遮挡样本（cuboid）。
    #[serde(default)]
    scale: Option<[f32; 3]>,
    #[serde(default)]
    name: Option<String>,
}

/// `ash_renderer/spawn_shape`：造一个测试体（五形状之一 + 颜色 →
/// Lambert 材质）。mesh 按形状缓存、材质按颜色缓存——同参重复 spawn
/// 共享柄零新资产；首次形状/颜色走一次上传。账本经 Added 通道自动入行，
/// 画面次帧可见。返回实体号、资产身份与 mesh_created（对账锚点）。
#[system]
pub(crate) fn spawn_shape(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let value = params.ok_or_else(|| invalid_params("params 缺失：需要 shape"))?;
    let p: SpawnShapeParams =
        serde_json::from_value(value).map_err(|e| invalid_params(e.to_string()))?;
    let color = p.color.unwrap_or([0.8, 0.8, 0.8]);
    let readback = spawn_test_shape(
        world,
        p.shape,
        color,
        p.translation.map(Vec3::from_array),
        p.rotation_euler_deg.map(Vec3::from_array),
        p.scale.map(Vec3::from_array),
        p.name.as_deref(),
    )
    .map_err(object_op_error)?;
    Ok(json!({
        "entity": readback.entity.to_string(),
        "shape": readback.shape.as_str(),
        "mesh": readback.mesh.to_string(),
        "material": readback.material.to_string(),
        "mesh_created": readback.mesh_created,
    }))
}
