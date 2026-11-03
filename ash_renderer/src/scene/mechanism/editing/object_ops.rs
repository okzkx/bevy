//! 对象写操作，为 BRP 与其他写入方提供共享实现。
//! 本模块不维护账本；组件变化由 Changed/Removed 通道发现。
//! despawn 和摘组件由 Bevy 的 World 操作处理。

use bevy::{asset::AssetPath, asset::AssetServer, pbr::StandardMaterial, prelude::*};

/// 对象操作被拒绝的原因。
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
    /// 资产路径解析失败（坏标签/坏 source 语法，如 `file.gltf#`）。
    InvalidAssetPath(String),
    /// 请求参数形状非法（数值域越界等；与"缺参数"的 NothingToDo 分列）。
    InvalidParams(String),
    /// chunk id 不在场（整块操作按生成期编号寻址）。
    ChunkMissing(u32),
}

/// 先用 `try_parse` 校验动态路径，避免非法输入在转换时 panic；失败作为操作错误返回。
fn parse_asset_path(path: &str) -> Result<AssetPath<'static>, ObjectOpError> {
    AssetPath::try_parse(path)
        .map(AssetPath::into_owned)
        .map_err(|e| ObjectOpError::InvalidAssetPath(format!("{path}: {e}")))
}

/// 目标实体须在 World。
fn ensure_entity(world: &World, entity: Entity) -> Result<(), ObjectOpError> {
    world
        .get_entity(entity)
        .map(|_| ())
        .map_err(|_| ObjectOpError::EntityMissing(entity))
}

/// 克隆源的三件套（mesh/材质柄 + Transform），供克隆与批量生成共用。
///
/// # Errors
/// 源缺 Mesh3d/MeshMaterial3d（`SourceNotPrimitive`）或缺 Transform。
pub(crate) fn primitive_parts(
    world: &World,
    source: Entity,
) -> Result<(Mesh3d, MeshMaterial3d<StandardMaterial>, Transform), ObjectOpError> {
    let mesh = world
        .get::<Mesh3d>(source)
        .cloned()
        .ok_or(ObjectOpError::SourceNotPrimitive(source))?;
    let material = world
        .get::<MeshMaterial3d<StandardMaterial>>(source)
        .cloned()
        .ok_or(ObjectOpError::SourceNotPrimitive(source))?;
    let transform = world
        .get::<Transform>(source)
        .copied()
        .ok_or(ObjectOpError::ComponentMissing("Transform", source))?;
    Ok((mesh, material, transform))
}

/// 复制源实体的 mesh/material 句柄和 Transform，并按 offset 平移；新实体与源共享资产。
///
/// # Errors
/// 源实体缺席或缺 Mesh3d/MeshMaterial3d/Transform。
pub fn spawn_primitive_clone(
    world: &mut World,
    source: Entity,
    offset: Vec3,
    name_suffix: &str,
) -> Result<Entity, ObjectOpError> {
    let (mesh, material, mut transform) = primitive_parts(world, source)?;
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

#[derive(Debug, Default)]
pub struct HandleSwapReadback {
    pub mesh: Option<AssetId<Mesh>>,
    pub material: Option<AssetId<StandardMaterial>>,
}

/// 将源实体的 mesh/material 句柄写入目标；目标与源共享资产。至少提供一个来源实体。
///
/// # Errors
/// 目标/源实体缺席、源非 primitive、目标缺对应组件。
pub fn replace_handles(
    world: &mut World,
    target: Entity,
    mesh_from: Option<Entity>,
    material_from: Option<Entity>,
) -> Result<HandleSwapReadback, ObjectOpError> {
    ensure_entity(world, target)?;
    if mesh_from.is_none() && material_from.is_none() {
        return Err(ObjectOpError::NothingToDo);
    }
    let mut readback = HandleSwapReadback::default();
    if let Some(source) = mesh_from {
        let handle = world
            .get::<Mesh3d>(source)
            .map(|m| m.0.clone())
            .ok_or(ObjectOpError::SourceNotPrimitive(source))?;
        let mut slot = world
            .get_mut::<Mesh3d>(target)
            .ok_or(ObjectOpError::ComponentMissing("Mesh3d", target))?;
        readback.mesh = Some(handle.id());
        slot.0 = handle;
    }
    if let Some(source) = material_from {
        let handle = world
            .get::<MeshMaterial3d<StandardMaterial>>(source)
            .map(|m| m.0.clone())
            .ok_or(ObjectOpError::SourceNotPrimitive(source))?;
        let mut slot = world
            .get_mut::<MeshMaterial3d<StandardMaterial>>(target)
            .ok_or(ObjectOpError::ComponentMissing("MeshMaterial3d", target))?;
        readback.material = Some(handle.id());
        slot.0 = handle;
    }
    Ok(readback)
}

/// 加载中句柄的资产 ID；加载完成前后保持不变。
#[derive(Debug)]
pub struct LoadedMeshReadback {
    pub entity: Entity,
    pub mesh: AssetId<Mesh>,
}

/// 发起异步加载并把加载中句柄写进 `entity` 的 `Mesh3d`。
fn bind_loaded_mesh(
    world: &mut World,
    entity: Entity,
    path: AssetPath<'static>,
) -> Result<LoadedMeshReadback, ObjectOpError> {
    let handle: Handle<Mesh> = world.resource::<AssetServer>().load(path);
    let mut slot = world
        .get_mut::<Mesh3d>(entity)
        .ok_or(ObjectOpError::ComponentMissing("Mesh3d", entity))?;
    let mesh = handle.id();
    slot.0 = handle;
    Ok(LoadedMeshReadback { entity, mesh })
}

/// 异步加载 mesh 并换绑到既有实体；立即写入加载中句柄，路径先经 [`parse_asset_path`] 校验。
///
/// # Errors
/// 目标实体缺席、路径非法或缺 `Mesh3d`。
pub fn load_mesh_rebind(
    world: &mut World,
    target: Entity,
    path: &str,
) -> Result<LoadedMeshReadback, ObjectOpError> {
    ensure_entity(world, target)?;
    let path = parse_asset_path(path)?;
    bind_loaded_mesh(world, target, path)
}

/// 异步加载 mesh 并挂到新实体上：克隆 `source` 的结构（材质柄/
/// 变换/Name，语义同 [`spawn_primitive_clone`]），`Mesh3d` 换成加载中柄——
/// 账本立即入行、画面在到货帧出现新对象。失败语义同 [`load_mesh_rebind`]。
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
    let path = parse_asset_path(path)?;
    let entity = spawn_primitive_clone(world, source, offset, name_suffix)?;
    bind_loaded_mesh(world, entity, path)
}

/// 异步加载贴图并写入目标材质的 base_color_texture 槽；资产事件通知读侧。
/// 坑：材质是原位修改，共享该材质的实体都会受影响。
///
/// # Errors
/// 目标实体缺席、路径非法、非 primitive、缺 `MeshMaterial3d`，或材质资产不在容器。
pub fn load_image_slot(
    world: &mut World,
    target: Entity,
    path: &str,
) -> Result<AssetId<Image>, ObjectOpError> {
    ensure_entity(world, target)?;
    let material = world
        .get::<MeshMaterial3d<StandardMaterial>>(target)
        .map(|m| m.0.clone())
        .ok_or(ObjectOpError::ComponentMissing("MeshMaterial3d", target))?;
    let path = parse_asset_path(path)?;
    let handle = world.resource::<AssetServer>().load(path);
    let mut assets = world.resource_mut::<Assets<StandardMaterial>>();
    let mut asset = assets
        .get_mut(&material)
        .ok_or(ObjectOpError::MaterialAssetMissing(target))?;
    asset.base_color_texture = Some(handle.clone());
    Ok(handle.id())
}
