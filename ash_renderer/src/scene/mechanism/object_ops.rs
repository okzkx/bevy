//! 对象写操作封装（4.1.5）：spawn 克隆/换柄两类实体级变更的可复用实现——
//! BRP 方法族（[`crate::remote_objects`]）与将来的 egui 面板共用这一层，
//! 不养两份逻辑（3.11.2 的 apply_transform_edit 同款惯例）。despawn/摘组件
//! 不在此层：官方 BRP 内置 `world.despawn_entity`/`world.remove_components`
//! 已覆盖该通道，且当前无 egui 消费者——不为"将来可能"预写零调用代码。
//!
//! 边界：本模块只做 ECS 写路径，**不碰账本**——变更进账走
//! [`crate::scene::mechanism::update_ledger`] 的 Changed/Removed 通道，任何
//! 写入方（BRP/UI/代码）自动被账本看见，这是"发现与写入解耦"的验收面。

use bevy::{asset::AssetServer, pbr::StandardMaterial, prelude::*};

/// 对象操作被拒的原因（Tier②外泄给调用方分级：BRP 映射协议错误码）。
#[derive(Debug)]
pub enum ObjectOpError {
    /// 目标实体不在 World。
    EntityMissing(Entity),
    /// 源实体不是可渲染 primitive（缺 Mesh3d/MeshMaterial3d）。
    SourceNotPrimitive(Entity),
    /// 目标实体缺要求的组件。
    ComponentMissing(&'static str, Entity),
    /// 请求没带任何要动的量。
    NothingToDo,
    /// 目标实体的材质资产不在容器里（槽写操作无处可写）。
    MaterialAssetMissing(Entity),
}

/// 克隆一个可渲染 primitive：复制源的 Mesh3d/MeshMaterial3d 柄与 Transform
///（平移加 `offset`），Name 加后缀便于树内辨认。柄复制意味着**共享资产**——
/// 不产生新上传（驻留缓存按 AssetId 去重），账本只见新行。无父独立成树。
///
/// # Errors
/// 源实体缺席或缺 Mesh3d/MeshMaterial3d/Transform。
pub fn spawn_primitive_clone(
    world: &mut World,
    source: Entity,
    offset: Vec3,
    name_suffix: &str,
) -> Result<Entity, ObjectOpError> {
    let Some(mesh) = world.get::<Mesh3d>(source).cloned() else {
        return Err(ObjectOpError::SourceNotPrimitive(source));
    };
    let Some(material) = world.get::<MeshMaterial3d<StandardMaterial>>(source).cloned() else {
        return Err(ObjectOpError::SourceNotPrimitive(source));
    };
    let Some(mut transform) = world.get::<Transform>(source).cloned() else {
        return Err(ObjectOpError::ComponentMissing("Transform", source));
    };
    transform.translation += offset;
    let name = world
        .get::<Name>(source)
        .map(|n| Name::new(format!("{}{name_suffix}", n.as_str())));
    let mut spawned = world.spawn((mesh, material, transform));
    if let Some(name) = name {
        spawned.insert(name);
    }
    Ok(spawned.id())
}

/// 换柄读回：换绑后目标实体的资产身份。
#[derive(Debug)]
pub struct HandleSwapReadback {
    pub mesh: Option<AssetId<Mesh>>,
    pub material: Option<AssetId<StandardMaterial>>,
}

/// 换柄：把 `mesh_from`/`material_from`（源实体）身上的柄拷到 `target`。柄是
/// 引用语义——目标与源共享同一资产身份；账本行换绑走 Changed 通道，GPU 侧
/// 旧资产是否回收归 4.4。至少一项给值，两项都缺 = [`ObjectOpError::NothingToDo`]。
///
/// # Errors
/// 目标/源实体缺席、源非 primitive、目标缺对应组件。
pub fn replace_handles(
    world: &mut World,
    target: Entity,
    mesh_from: Option<Entity>,
    material_from: Option<Entity>,
) -> Result<HandleSwapReadback, ObjectOpError> {
    if world.get_entity(target).is_err() {
        return Err(ObjectOpError::EntityMissing(target));
    }
    let mut readback = HandleSwapReadback {
        mesh: None,
        material: None,
    };
    if mesh_from.is_none() && material_from.is_none() {
        return Err(ObjectOpError::NothingToDo);
    }
    if let Some(source) = mesh_from {
        let Some(handle) = world.get::<Mesh3d>(source).map(|m| m.0.clone()) else {
            return Err(ObjectOpError::SourceNotPrimitive(source));
        };
        let Some(mut slot) = world.get_mut::<Mesh3d>(target) else {
            return Err(ObjectOpError::ComponentMissing("Mesh3d", target));
        };
        slot.0 = handle.clone();
        readback.mesh = Some(handle.id());
    }
    if let Some(source) = material_from {
        let Some(handle) = world
            .get::<MeshMaterial3d<StandardMaterial>>(source)
            .map(|m| m.0.clone())
        else {
            return Err(ObjectOpError::SourceNotPrimitive(source));
        };
        let Some(mut slot) = world.get_mut::<MeshMaterial3d<StandardMaterial>>(target) else {
            return Err(ObjectOpError::ComponentMissing("MeshMaterial3d", target));
        };
        slot.0 = handle.clone();
        readback.material = Some(handle.id());
    }
    Ok(readback)
}

/// 动态加载换柄读回（4.3.3）：受理的资产身份（加载中柄的 id，到货前后不变
/// ——AssetId 锚身份非内容，pending 期间即定型）。
#[derive(Debug)]
pub struct LoadedMeshReadback {
    pub entity: Entity,
    pub mesh: AssetId<Mesh>,
}

/// 异步加载 mesh 并换到既有实体上（4.3.3）：`server.load(path)` 立即返回
/// **加载中柄**，换绑走 [`Mesh3d`] 组件改写（Changed 通道账本自动换行）。
/// 资产未到货期间行保持脏标（pending 账面），到货帧 flush 转换上传、画面
/// 换上新内容；路径失败的失败读侧收账（出重试队列，行出队）。同步不判路径
/// 合法性——加载是异步链，本函数只负责"发请求 + 换柄"。
///
/// # Errors
/// 目标实体缺席或缺 `Mesh3d`。
pub fn load_mesh_rebind(
    world: &mut World,
    target: Entity,
    path: &str,
) -> Result<LoadedMeshReadback, ObjectOpError> {
    if world.get_entity(target).is_err() {
        return Err(ObjectOpError::EntityMissing(target));
    }
    // load 收 AssetPath<'static>（柄内保存），&str 须转 owned
    let handle = world.resource::<AssetServer>().load::<Mesh>(path.to_owned());
    let Some(mut slot) = world.get_mut::<Mesh3d>(target) else {
        return Err(ObjectOpError::ComponentMissing("Mesh3d", target));
    };
    slot.0 = handle.clone();
    Ok(LoadedMeshReadback {
        entity: target,
        mesh: handle.id(),
    })
}

/// 异步加载 mesh 并挂到新实体上（4.3.3）：克隆 `source` 的结构（材质柄/
/// 变换/Name，语义同 [`spawn_primitive_clone`]），`Mesh3d` 换成加载中柄——
/// 账本立即入行、画面在到货帧出现新对象（"动态加载新资产"的驱动面，
/// 3.11 范围修订并回 4.3 的落点）。失败语义同 [`load_mesh_rebind`]。
///
/// # Errors
/// 源实体缺席、非 primitive 或缺 `Transform`（经克隆封装上抛）。
pub fn spawn_with_loaded_mesh(
    world: &mut World,
    source: Entity,
    path: &str,
    offset: Vec3,
    name_suffix: &str,
) -> Result<LoadedMeshReadback, ObjectOpError> {
    let entity = spawn_primitive_clone(world, source, offset, name_suffix)?;
    // load 收 AssetPath<'static>（柄内保存），&str 转 owned
    let handle = world.resource::<AssetServer>().load::<Mesh>(path.to_owned());
    let Some(mut slot) = world.get_mut::<Mesh3d>(entity) else {
        return Err(ObjectOpError::ComponentMissing("Mesh3d", entity));
    };
    slot.0 = handle.clone();
    Ok(LoadedMeshReadback {
        entity,
        mesh: handle.id(),
    })
}

/// 异步加载贴图并换到目标实体的材质 base_color 槽上（4.3.3）：加载中柄直接
/// 写进 `Assets<StandardMaterial>` 的槽字段（`Modified` 事件 → 4.2 失效联动
/// 把引用行重挂脏标），贴图本体未到货期间槽腿 pending，到货帧重传换指。
/// **共享材质语义**：目标实体的材质被原位改写，全部引用者同变（与 4.2
/// mutate 贴图槽同语义）。
///
/// # Errors
/// 目标实体缺席、非 primitive、缺 `MeshMaterial3d`，或材质资产不在容器。
pub fn load_image_slot(
    world: &mut World,
    target: Entity,
    path: &str,
) -> Result<AssetId<Image>, ObjectOpError> {
    if world.get_entity(target).is_err() {
        return Err(ObjectOpError::EntityMissing(target));
    }
    let Some(material) = world
        .get::<MeshMaterial3d<StandardMaterial>>(target)
        .map(|m| m.0.clone())
    else {
        return Err(ObjectOpError::ComponentMissing("MeshMaterial3d", target));
    };
    let handle = world.resource::<AssetServer>().load::<Image>(path.to_owned());
    let mut assets = world.resource_mut::<Assets<StandardMaterial>>();
    let Some(mut asset) = assets.get_mut(&material) else {
        return Err(ObjectOpError::MaterialAssetMissing(target));
    };
    asset.base_color_texture = Some(handle.clone());
    Ok(handle.id())
}
