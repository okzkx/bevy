//! Transform 编辑操作封装（3.11.2）：egui 拖值与 3.12 的 BRP `set_transform`
//! 共用的唯一实现，不养两份逻辑。
//!
//! 显示读 [`super::entity_tree_window::SelectedDetails`] 的同帧快照；写值走
//! [`TransformEditQueue`]——egui pass 里组好编辑，[`apply_transform_edits`]
//! 在 Update 内紧随 pass 清账（链式排程），PostUpdate 传播 → collect_scene →
//! Last 绘制，画面当帧跟随。写值直接改 `Transform` 组件，不走反射。

use bevy::{math::EulerRot, prelude::*};

use ash_macros::system;

/// 一次 Transform 编辑：逐分量可选（None = 不动该组）。欧拉角用度数（与属性
/// 区展示、编辑控件一致），内部换弧度；旋转分解/合成固定 YXZ 序，与属性区
/// 展示同口径。
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

/// 编辑排队资源：egui pass 产，[`apply_transform_edits`] 消费。3.12 的 BRP
/// 写方法不排队——持 &mut World 直接调 [`apply_transform_edit`]，同款实现。
#[derive(Resource, Default)]
pub struct TransformEditQueue(pub Vec<(Entity, TransformEdit)>);

/// 应用一次编辑：实体存在且带 `Transform` 才生效，逐组覆盖。
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

/// Update 清账：把队列里的编辑逐条写进 World，链在 egui pass 之后、传播之前。
/// 单条失败 warn 丢弃继续（Tier①：调试编辑不影响帧循环）。
#[system]
pub(super) fn apply_transform_edits(world: &mut World) {
    let edits = std::mem::take(&mut world.resource_mut::<TransformEditQueue>().0);
    for (entity, edit) in edits {
        if let Err(e) = apply_transform_edit(world, entity, edit.clone()) {
            warn!("Transform 编辑被拒（{e:?}）：实体 {entity:?}，编辑 {edit:?}");
        }
    }
}
