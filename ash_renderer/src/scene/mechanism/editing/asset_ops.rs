//! 资产写操作，供 BRP 与其他写入方共用。
//! 通过实体持有的资产句柄修改资产；`Assets::get_mut` 会发出修改事件。
//! 像素 tint 仅支持 4 字节 RGBA 格式。

use bevy::{
    image::Image,
    mesh::{Mesh, VertexAttributeValues},
    pbr::StandardMaterial,
    prelude::*,
};
use wgpu_types::TextureFormat;

use crate::scene::mechanism::InstanceLedger;

/// 资产操作被拒的原因。
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
    /// 参数域不合法（PBR 标量出界等）。
    InvalidParams(&'static str),
    /// 请求没带任何要动的量。
    NothingToDo,
}

/// mutate 请求（与 BRP 参数同形，egui 面板直接复用）：逐项可选，至少一项。
#[derive(Debug, Default)]
pub struct MutateAsset {
    /// 写入材质 base_color 的线性 RGBA 值。
    pub base_color: Option<[f32; 4]>,
    /// 改材质参数：metallic（[0,1]，PBR 面）。
    pub metallic: Option<f32>,
    /// 改材质参数：perceptual_roughness（[0,1]，PBR 面）。
    pub perceptual_roughness: Option<f32>,
    /// 复制源实体的 base_color_texture 槽；源材质无贴图时清空目标槽。
    pub slot_base_color_from: Option<Entity>,
    /// 改 Mesh 内容：POSITION 顶点绕原点均匀缩放系数。
    pub mesh_vertex_scale: Option<f32>,
    /// 改 Image 内容：base_color_texture 的 RGB 字节乘数（1.0 = 不变；不做色彩空间换算）。
    pub image_tint: Option<[f32; 3]>,
}

/// mutate 读回：动过的资产身份（写后读对账用）。
#[derive(Debug, Default)]
pub struct AssetOpReadback {
    pub material: Option<AssetId<StandardMaterial>>,
    pub mesh: Option<AssetId<Mesh>>,
    pub image: Option<AssetId<Image>>,
}

/// 先只读预校验所有操作，再逐项写入，避免组合请求失败时留下部分修改。
/// 实际写入通过 `Assets::get_mut` 发出修改事件。
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
        && p.metallic.is_none()
        && p.perceptual_roughness.is_none()
        && p.slot_base_color_from.is_none()
        && p.mesh_vertex_scale.is_none()
        && p.image_tint.is_none()
    {
        return Err(AssetOpError::NothingToDo);
    }
    // PBR 标量域校验（[0,1]；shader 端也有 clamp，这里把误用挡在入口）
    if p.metallic.is_some_and(|m| !(0.0..=1.0).contains(&m))
        || p.perceptual_roughness.is_some_and(|r| !(0.0..=1.0).contains(&r))
    {
        return Err(AssetOpError::InvalidParams(
            "metallic / perceptual_roughness 须在 [0,1]",
        ));
    }

    // 预校验并解析换槽和 tint 的目标柄。
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
    let tint_target: Option<Handle<Image>> = if p.image_tint.is_some() {
        let texture_handle = match new_slot.as_ref() {
            Some(Some(handle)) => handle.clone(),
            Some(None) => {
                return Err(AssetOpError::AssetMissing(
                    "换槽请求清除了 base_color_texture,无处 tint",
                ));
            }
            None => {
                let assets = world
                    .get_resource::<Assets<StandardMaterial>>()
                    .ok_or(AssetOpError::AssetMissing("Assets<StandardMaterial> 容器缺席"))?;
                assets
                    .get(&material_handle)
                    .ok_or(AssetOpError::AssetMissing("StandardMaterial"))?
                    .base_color_texture
                    .clone()
                    .ok_or(AssetOpError::AssetMissing(
                        "该实体材质没有 base_color_texture,无处 tint",
                    ))?
            }
        };
        let assets = world
            .get_resource::<Assets<Image>>()
            .ok_or(AssetOpError::AssetMissing("Assets<Image> 容器缺席"))?;
        let image = assets
            .get(&texture_handle)
            .ok_or(AssetOpError::AssetMissing("Image"))?;
        check_tint_rgba8(image)?;
        Some(texture_handle)
    } else {
        None
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

    // 全部校验通过后才写入资产并触发修改事件。
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

    if p.metallic.is_some() || p.perceptual_roughness.is_some() {
        let mut assets = world.get_resource_mut::<Assets<StandardMaterial>>().ok_or(
            AssetOpError::AssetMissing("Assets<StandardMaterial> 容器缺席"),
        )?;
        let mut material = assets
            .get_mut(&material_handle)
            .ok_or(AssetOpError::AssetMissing("StandardMaterial"))?;
        if let Some(m) = p.metallic {
            material.metallic = m;
        }
        if let Some(r) = p.perceptual_roughness {
            material.perceptual_roughness = r;
        }
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
        // 同时换槽与 tint 时，对新槽贴图着色。
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

    // BRP 写入发生在 RemoteLast；直接标记材质引用行重估，避免等待资产事件导致组装晚一帧。
    if (p.base_color.is_some() || new_slot.is_some())
        && let Some(mut ledger) = world.get_resource_mut::<InstanceLedger>()
    {
        for entity in ledger.rows_with_material(material_handle.id()) {
            ledger.mark_assembly_stale(entity);
        }
    }

    Ok(readback)
}

/// 校验 tint 支持的像素格式及数据是否存在。
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

/// tint 支持的像素格式。
fn tint_format_supported(format: TextureFormat) -> bool {
    matches!(
        format,
        TextureFormat::Rgba8Unorm
            | TextureFormat::Rgba8UnormSrgb
            | TextureFormat::Rgba8Snorm
            | TextureFormat::Rgba8Uint
            | TextureFormat::Rgba8Sint
    )
}

/// 对 RGBA8 像素的 RGB 字节直接乘以系数，不做色彩空间转换。格式或数据缺失时返回错误。
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

#[cfg(test)]
mod tests {
    use super::tint_format_supported;
    use wgpu_types::TextureFormat;

    #[test]
    fn tint_support_matches_rgba_byte_order() {
        assert!(tint_format_supported(TextureFormat::Rgba8Unorm));
        assert!(tint_format_supported(TextureFormat::Rgba8UnormSrgb));
        assert!(!tint_format_supported(TextureFormat::Bgra8Unorm));
        assert!(!tint_format_supported(TextureFormat::Bgra8UnormSrgb));
    }
}
