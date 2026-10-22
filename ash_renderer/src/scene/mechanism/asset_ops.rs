//! 资产写操作封装："改资产内容"四类驱动的可复用实现——BRP 方法族
//! （[`crate::remote::assets`]）与将来 egui 面板共用这一层，不养两份逻辑
//! （apply_transform_edit / object_ops 同款惯例）。
//!
//! 定位锚定实体（与换柄同口径）：BRP 参数给 entity，取它身上的
//! `Mesh3d`/`MeshMaterial3d` 柄再去资产容器 `get_mut`——**get_mut 即触发
//! `AssetEvent::Modified`**（bevy_asset/src/assets.rs:376/391），读侧
//! （asset_events）自动收到，下游失效→重传全链不用任何写入方配合。这是
//! "发现与写入解耦"的验收面：任何路径（BRP/egui/游戏代码）改资产内容，
//! 走的都是同一个 `Assets` 容器语义。
//!
//! 四驱动与判定线的对应：
//! | 驱动 | 参数 | 验证面 |
//! |---|---|---|
//! | 改材质参数 | `base_color` | Modified 收到（材质参数每帧 UBO 自动生效，作事件读侧观测锚） |
//! | 换材质贴图槽 | `slot_base_color_from` | 关联 Image 失效传播 |
//! | 改 Mesh 内容 | `mesh_vertex_scale` | 同柄重传恰一份（共享去重） |
//! | 改 Image 内容 | `image_tint` | graveyard + 原槽重写 |
//!
//! 边界：本模块只做 `Assets` 容器写路径，**不碰账本与失效集**——变更经
//! AssetEvent 通道进读侧，与"发现与写入解耦"同一验收面。像素级 tint
//! 只收 4 字节 RGBA 族格式（逐字节乘系数），其余格式明确拒绝不硬做。

use bevy::{
    image::Image,
    mesh::{Mesh, VertexAttributeValues},
    pbr::StandardMaterial,
    prelude::*,
};
use wgpu_types::TextureFormat;

use super::ledger::InstanceLedger;

/// 资产操作被拒的原因（Tier②外泄给调用方分级：BRP 映射协议错误码）。
#[derive(Debug)]
pub enum AssetOpError {
    /// 目标实体不在 World。
    EntityMissing(Entity),
    /// 目标实体不是可渲染 primitive（缺 Mesh3d/MeshMaterial3d）。
    TargetNotPrimitive(Entity),
    /// 槽拷贝源实体不是可渲染 primitive。
    SourceNotPrimitive(Entity),
    /// 柄在资产容器查无（资产已卸载/句柄悬空）。
    AssetMissing(&'static str),
    /// mesh 缺 POSITION 属性或非 F32 三元布局，顶点缩放拒绝。
    MeshPositionsUnsupported,
    /// image 非 4 字节 RGBA 族格式，像素 tint 拒绝。
    ImageFormatUnsupported(TextureFormat),
    /// 请求没带任何要动的量。
    NothingToDo,
}

/// mutate 请求（与 BRP 参数同形，egui 面板直接复用）：逐项可选，至少一项。
#[derive(Debug, Default)]
pub struct MutateAsset {
    /// 改材质参数：base_color（线性域 RGBA）。
    pub base_color: Option<[f32; 4]>,
    /// 换材质贴图槽：把源实体的 base_color_texture 柄拷过来（None 源 = 清槽）。
    pub slot_base_color_from: Option<Entity>,
    /// 改 Mesh 内容：POSITION 顶点绕原点均匀缩放系数。
    pub mesh_vertex_scale: Option<f32>,
    /// 改 Image 内容：该实体材质 base_color_texture 像素按通道乘的色调
    ///（线性域 RGB，1.0 = 不变）。
    pub image_tint: Option<[f32; 3]>,
}

/// mutate 读回：动过的资产身份（写后读对账用）。
#[derive(Debug, Default)]
pub struct AssetOpReadback {
    pub material: Option<AssetId<StandardMaterial>>,
    pub mesh: Option<AssetId<Mesh>>,
    pub image: Option<AssetId<Image>>,
}

/// mutate 总入口：**先全量预校验、再逐项执行**——边校验边修改会让组合
/// 请求单项失败时前面的项已落改，与"整体拒绝，不留半完成"的
/// 承诺不符。校验段只读（`Assets::get`），不触发 `Modified` 事件——被拒
/// 请求零副作用；执行段全部校验通过后才 `get_mut` 落改。**每项动用
/// `Assets::get_mut`，Modified 事件由容器自动发出**。
///
/// # Errors
/// 实体缺席/非 primitive、资产容器查无、格式不支持、空请求。
pub fn mutate_asset(
    world: &mut World,
    target: Entity,
    p: MutateAsset,
) -> Result<AssetOpReadback, AssetOpError> {
    if world.get_entity(target).is_err() {
        return Err(AssetOpError::EntityMissing(target));
    }
    let mesh_handle = world
        .get::<Mesh3d>(target)
        .map(|m| m.0.clone())
        .ok_or(AssetOpError::TargetNotPrimitive(target))?;
    let material_handle = world
        .get::<MeshMaterial3d<StandardMaterial>>(target)
        .map(|m| m.0.clone())
        .ok_or(AssetOpError::TargetNotPrimitive(target))?;
    if p.base_color.is_none()
        && p.slot_base_color_from.is_none()
        && p.mesh_vertex_scale.is_none()
        && p.image_tint.is_none()
    {
        return Err(AssetOpError::NothingToDo);
    }

    // —— 预校验段（只读）：逐项检查可动性，执行段需要的解析结果就地收集。
    // 换槽源槽柄与 tint 目标贴图柄都在这里定案。
    let new_slot: Option<Option<Handle<Image>>> = match p.slot_base_color_from {
        None => None,
        Some(source) => {
            let source_texture = world
                .get::<MeshMaterial3d<StandardMaterial>>(source)
                .ok_or(AssetOpError::SourceNotPrimitive(source))?
                .0
                .clone();
            // 源材质的 base_color 槽柄（None = 清目标槽——拷槽语义含"没有"）
            let assets = world
                .get_resource::<Assets<StandardMaterial>>()
                .ok_or(AssetOpError::AssetMissing("Assets<StandardMaterial> 容器缺席"))?;
            let source_material = assets
                .get(&source_texture)
                .ok_or(AssetOpError::AssetMissing("StandardMaterial"))?;
            Some(source_material.base_color_texture.clone())
        }
    };
    let tint_target: Option<Handle<Image>> = match p.image_tint {
        None => None,
        Some(_) => {
            let texture_handle = {
                let assets = world
                    .get_resource::<Assets<StandardMaterial>>()
                    .ok_or(AssetOpError::AssetMissing("Assets<StandardMaterial> 容器缺席"))?;
                let material = assets
                    .get(&material_handle)
                    .ok_or(AssetOpError::AssetMissing("StandardMaterial"))?;
                material.base_color_texture.clone().ok_or(AssetOpError::AssetMissing(
                    "该实体材质没有 base_color_texture,无处 tint",
                ))?
            };
            let assets = world
                .get_resource::<Assets<Image>>()
                .ok_or(AssetOpError::AssetMissing("Assets<Image> 容器缺席"))?;
            let image = assets
                .get(&texture_handle)
                .ok_or(AssetOpError::AssetMissing("Image"))?;
            check_tint_rgba8(image)?;
            Some(texture_handle)
        }
    };
    if p.mesh_vertex_scale.is_some() {
        let assets =
            world.get_resource::<Assets<Mesh>>().ok_or(AssetOpError::AssetMissing(
                "Assets<Mesh> 容器缺席",
            ))?;
        let mesh = assets
            .get(&mesh_handle)
            .ok_or(AssetOpError::AssetMissing("Mesh"))?;
        match mesh.attribute(Mesh::ATTRIBUTE_POSITION) {
            Some(VertexAttributeValues::Float32x3(_)) => {}
            _ => return Err(AssetOpError::MeshPositionsUnsupported),
        }
    }

    // —— 执行段：全部校验通过，逐项 get_mut 落改（此段起才触发 Modified 事件）
    let mut readback = AssetOpReadback::default();

    if let Some(color) = p.base_color {
        let [r, g, b, a] = color;
        let mut assets = world.get_resource_mut::<Assets<StandardMaterial>>().ok_or(
            AssetOpError::AssetMissing("Assets<StandardMaterial> 容器缺席"),
        )?;
        let mut material = assets
            .get_mut(&material_handle)
            .ok_or(AssetOpError::AssetMissing("StandardMaterial"))?;
        material.base_color = Color::LinearRgba(LinearRgba::new(r, g, b, a));
        readback.material = Some(material_handle.id());
    }

    if let Some(source_slot) = new_slot.clone() {
        let mut assets = world.get_resource_mut::<Assets<StandardMaterial>>().ok_or(
            AssetOpError::AssetMissing("Assets<StandardMaterial> 容器缺席"),
        )?;
        let mut material = assets
            .get_mut(&material_handle)
            .ok_or(AssetOpError::AssetMissing("StandardMaterial"))?;
        material.base_color_texture = source_slot;
        readback.material = Some(material_handle.id());
    }

    if let Some(scale) = p.mesh_vertex_scale {
        let mut assets =
            world.get_resource_mut::<Assets<Mesh>>().ok_or(AssetOpError::AssetMissing(
                "Assets<Mesh> 容器缺席",
            ))?;
        let mut mesh = assets
            .get_mut(&mesh_handle)
            .ok_or(AssetOpError::AssetMissing("Mesh"))?;
        let VertexAttributeValues::Float32x3(positions) =
            mesh.attribute(Mesh::ATTRIBUTE_POSITION).ok_or(AssetOpError::MeshPositionsUnsupported)?
        else {
            return Err(AssetOpError::MeshPositionsUnsupported);
        };
        let scaled: Vec<[f32; 3]> = positions.iter().map(|p| [p[0] * scale, p[1] * scale, p[2] * scale]).collect();
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, scaled);
        readback.mesh = Some(mesh_handle.id());
    }

    if let Some(tint) = p.image_tint {
        // 目标贴图 = 该实体材质的 base_color 槽（校验段已解析柄；image_tint 与
        // 贴图槽参数是同一驱动的两面:先换槽再 tint 时 tint 作用于新槽——
        // 执行序即参数序）
        let texture_handle = tint_target.ok_or(AssetOpError::AssetMissing(
            "该实体材质没有 base_color_texture,无处 tint",
        ))?;
        let mut assets = world
            .get_resource_mut::<Assets<Image>>()
            .ok_or(AssetOpError::AssetMissing("Assets<Image> 容器缺席"))?;
        let mut image = assets
            .get_mut(&texture_handle)
            .ok_or(AssetOpError::AssetMissing("Image"))?;
        tint_rgba8(&mut image, tint)?;
        readback.image = Some(texture_handle.id());
    }

    // 组装联动（7.9）：BRP 内容写入落在 RemoteLast——资产事件读侧隔帧才可读，
    // 等事件链标记重估会让组装缓存对全量路径晚一帧（对照模式实抓）。写入点
    // 直接标记材质引用行重估，同帧 draw_frame 即收敛（材质内容进组装判定
    // 的是 alpha 计数；贴图槽/mesh 内容不进元组，重估幂等无害）。
    if (p.base_color.is_some() || new_slot.is_some())
        && let Some(mut ledger) = world.get_resource_mut::<InstanceLedger>()
    {
        for entity in ledger.rows_with_material(material_handle.id()) {
            ledger.mark_assembly_stale(entity);
        }
    }

    Ok(readback)
}

/// tint 前置校验（mutate 预校验段用）：格式与像素数据在场判定，零副作用。
///
/// # Errors
/// 格式不在 4 字节 RGBA 族，或像素数据缺席。
fn check_tint_rgba8(image: &Image) -> Result<(), AssetOpError> {
    let format = image.texture_descriptor.format;
    if !tint_format_supported(format) {
        return Err(AssetOpError::ImageFormatUnsupported(format));
    }
    if image.data.is_none() {
        return Err(AssetOpError::AssetMissing("Image 像素数据"));
    }
    Ok(())
}

/// 4 字节 RGBA 族判定（tint 可动格式的唯一清单）。
fn tint_format_supported(format: TextureFormat) -> bool {
    matches!(
        format,
        TextureFormat::Rgba8Unorm
            | TextureFormat::Rgba8UnormSrgb
            | TextureFormat::Rgba8Snorm
            | TextureFormat::Rgba8Uint
            | TextureFormat::Rgba8Sint
            | TextureFormat::Bgra8Unorm
            | TextureFormat::Bgra8UnormSrgb
    )
}

/// RGBA 4 字节族逐像素乘色调（线性域系数直接乘字节值——验证观测以画面
/// 变化为准，不做色彩管理换算）。只收 4B RGBA 族，其余格式明确拒绝。
/// 预校验段已过 [`check_tint_rgba8`]，此处防御性复查同清单。
///
/// # Errors
/// 格式不在 4 字节 RGBA 族。
fn tint_rgba8(image: &mut Image, tint: [f32; 3]) -> Result<(), AssetOpError> {
    let format = image.texture_descriptor.format;
    if !tint_format_supported(format) {
        return Err(AssetOpError::ImageFormatUnsupported(format));
    }
    let Some(data) = image.data.as_mut() else {
        return Err(AssetOpError::AssetMissing("Image 像素数据"));
    };
    for px in data.as_chunks_mut::<4>().0 {
        px[0] = (f32::from(px[0]) * tint[0]).clamp(0.0, 255.0) as u8;
        px[1] = (f32::from(px[1]) * tint[1]).clamp(0.0, 255.0) as u8;
        px[2] = (f32::from(px[2]) * tint[2]).clamp(0.0, 255.0) as u8;
    }
    Ok(())
}
