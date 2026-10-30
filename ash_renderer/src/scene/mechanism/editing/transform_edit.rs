//! Transform 编辑由 egui 与 BRP 共用。
//! UI 在 egui pass 后、Transform 传播前应用队列；BRP 直接调用 [`apply_transform_edit`]。

use bevy::{math::EulerRot, prelude::*};

use ash_macros::system;

/// 一次 Transform 修改；旋转以度数输入，并按 YXZ 顺序转换。
#[derive(Clone, Debug, Default)]
pub struct TransformEdit {
    pub translation: Option<Vec3>,
    pub rotation_euler_deg: Option<Vec3>,
    pub scale: Option<Vec3>,
}

/// 编辑被拒的原因：目标实体不在 World，或实体上没有 Transform。
#[derive(Debug)]
pub enum TransformEditError {
    EntityMissing,
    TransformMissing,
}

/// egui pass 生成、[`apply_transform_edits`] 消费的编辑队列。
#[derive(Resource, Default)]
pub struct TransformEditQueue(pub Vec<(Entity, TransformEdit)>);

/// 仅覆盖 Some 指定的字段；目标须存在且包含 `Transform`。
pub fn apply_transform_edit(
    world: &mut World,
    entity: Entity,
    edit: TransformEdit,
) -> Result<(), TransformEditError> {
    if world.get_entity(entity).is_err() {
        return Err(TransformEditError::EntityMissing);
    }
    let Some(mut t) = world.get_mut::<Transform>(entity) else {
        return Err(TransformEditError::TransformMissing);
    };
    if let Some(v) = edit.translation {
        t.translation = v;
    }
    if let Some(e) = edit.rotation_euler_deg {
        t.rotation = Quat::from_euler(
            EulerRot::YXZ,
            e.x.to_radians(),
            e.y.to_radians(),
            e.z.to_radians(),
        );
    }
    if let Some(s) = edit.scale {
        t.scale = s;
    }
    Ok(())
}

/// 单条编辑失败时记录警告并丢弃该项，继续处理后续编辑。
#[system]
pub(crate) fn apply_transform_edits(world: &mut World) {
    let edits = std::mem::take(&mut world.resource_mut::<TransformEditQueue>().0);
    for (entity, edit) in edits {
        if let Err(e) = apply_transform_edit(world, entity, edit.clone()) {
            warn!("Transform 编辑被拒（{e:?}）：实体 {entity:?}，编辑 {edit:?}");
        }
    }
}
