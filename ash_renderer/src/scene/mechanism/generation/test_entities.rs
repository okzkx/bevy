//! 测试实体生成：按形状创建 primitive，并按形状缓存 mesh、按颜色和 PBR 参数缓存材质。
//! `unique` 可绕过 mesh 缓存，为实体创建独立 mesh 身份。
//! 五种形状均为单位尺寸，大小通过 Transform.scale 设置。

use std::collections::HashMap;

use bevy::{pbr::StandardMaterial, prelude::*};

use crate::scene::mechanism::ObjectOpError;

/// 测试体形状枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestShape {
    /// 球体（ico 4 细分，半径 0.5）。
    Sphere,
    /// 圆柱体（半径 0.5，高 1.0，沿 Y 轴）。
    Cylinder,
    /// 胶囊体（半径 0.5，总长 1.0，沿 Y 轴）。
    Capsule,
    /// 圆锥体（底半径 0.5，高 1.0，沿 Y 轴）。
    Cone,
    /// 立方体（1×1×1；非均匀缩放 = 墙体遮挡样本）。
    Cuboid,
}

impl TestShape {
    /// 形状的小写名称。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TestShape::Sphere => "sphere",
            TestShape::Cylinder => "cylinder",
            TestShape::Capsule => "capsule",
            TestShape::Cone => "cone",
            TestShape::Cuboid => "cuboid",
        }
    }
}

/// 材质缓存键：颜色及 PBR 参数的浮点位模式；缺省标量使用 `f32::NAN` 位模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MaterialKey {
    color: [u32; 3],
    metallic: u32,
    perceptual_roughness: u32,
}

/// 延迟创建的测试资产缓存；首次需要时才创建并登记资源。
#[derive(Resource, Default)]
pub struct TestShapeAssets {
    meshes: HashMap<TestShape, Handle<Mesh>>,
    materials: HashMap<MaterialKey, Handle<StandardMaterial>>,
}

/// 创建结果与资产身份。
#[derive(Debug)]
pub struct TestShapeReadback {
    pub entity: Entity,
    pub shape: TestShape,
    pub mesh: AssetId<Mesh>,
    pub material: AssetId<StandardMaterial>,
    /// 本次是否新建了 mesh 资产（false = 缓存命中共享柄，零新上传）。
    pub mesh_created: bool,
}

/// 按形状创建 mesh、按颜色与可选 PBR 参数创建材质，并生成带指定变换的独立实体。
///
/// # Errors
/// 形状网格构造失败时返回错误。
#[expect(
    clippy::too_many_arguments,
    reason = "spawn 参数面即 BRP 请求字段的逐项镜像（形状/色/变换/命名/缓存旁路），收束会藏掉与协议面的逐项对账"
)]
pub fn spawn_test_shape(
    world: &mut World,
    shape: TestShape,
    color: [f32; 3],
    metallic: Option<f32>,
    perceptual_roughness: Option<f32>,
    translation: Option<Vec3>,
    rotation_euler_deg: Option<Vec3>,
    scale: Option<Vec3>,
    name: Option<&str>,
    unique: bool,
) -> Result<TestShapeReadback, ObjectOpError> {
    // unique 为每个实体创建独立 mesh 资产；默认复用缓存 mesh。
    let (mesh_handle, mesh_created) = if unique {
        (world.resource_mut::<Assets<Mesh>>().add(build_shape_mesh(shape)?), true)
    } else {
    match world
        .get_resource::<TestShapeAssets>()
        .and_then(|a| a.meshes.get(&shape))
        .cloned()
    {
        Some(handle) => (handle, false),
        None => {
            let mesh = build_shape_mesh(shape)?;
            let handle = world.resource_mut::<Assets<Mesh>>().add(mesh);
            // 调用方须先初始化 Assets<Mesh>，否则 resource_mut 会 panic。
            world
                .get_resource_or_insert_with(TestShapeAssets::default)
                .meshes
                .insert(shape, handle.clone());
            (handle, true)
        }
    }
    };
    let key = MaterialKey {
        color: color.map(f32::to_bits),
        metallic: metallic.unwrap_or(f32::NAN).to_bits(),
        perceptual_roughness: perceptual_roughness.unwrap_or(f32::NAN).to_bits(),
    };
    let material_handle = match world
        .get_resource::<TestShapeAssets>()
        .and_then(|a| a.materials.get(&key))
        .cloned()
    {
        Some(handle) => handle,
        None => {
            let handle = world
                .resource_mut::<Assets<StandardMaterial>>()
                .add(StandardMaterial {
                    base_color: Color::srgb(color[0], color[1], color[2]),
                    // 未提供时采用 metallic=0、perceptual_roughness=0.5。
                    metallic: metallic.unwrap_or(0.0),
                    perceptual_roughness: perceptual_roughness.unwrap_or(0.5),
                    ..Default::default()
                });
            world
                .get_resource_or_insert_with(TestShapeAssets::default)
                .materials
                .insert(key, handle.clone());
            handle
        }
    };
    let mut transform = Transform::IDENTITY;
    if let Some(t) = translation {
        transform.translation = t;
    }
    if let Some(r) = rotation_euler_deg {
        // 与 Transform 编辑共用 YXZ 顺序，输入为度数。
        transform.rotation = Quat::from_euler(
            EulerRot::YXZ,
            r.x.to_radians(),
            r.y.to_radians(),
            r.z.to_radians(),
        );
    }
    if let Some(s) = scale {
        transform.scale = s;
    }
    let name = name
        .map(str::to_owned)
        .unwrap_or_else(|| format!("测试体-{}", shape.as_str()));
    let mesh = mesh_handle.id();
    let material = material_handle.id();
    let entity = world
        .spawn((
            Mesh3d(mesh_handle),
            MeshMaterial3d(material_handle),
            transform,
            Name::new(name),
        ))
        .id();
    Ok(TestShapeReadback {
        entity,
        shape,
        mesh,
        material,
        mesh_created,
    })
}

/// 构造指定形状的网格；构造失败时返回参数错误。
fn build_shape_mesh(shape: TestShape) -> Result<Mesh, ObjectOpError> {
    match shape {
        TestShape::Sphere => Sphere::new(0.5)
            .mesh()
            .ico(4)
            .map_err(|e| ObjectOpError::InvalidParams(format!("球体细分失败：{e}"))),
        TestShape::Cylinder => Ok(Cylinder::new(0.5, 1.0).mesh().build()),
        TestShape::Capsule => Ok(Capsule3d::new(0.5, 1.0).mesh().build()),
        TestShape::Cone => Ok(Cone::new(0.5, 1.0).mesh().build()),
        TestShape::Cuboid => Ok(Cuboid::new(1.0, 1.0, 1.0).mesh().build()),
    }
}

#[cfg(test)]
mod tests {
    use super::{spawn_test_shape, TestShape};
    use crate::scene::mechanism::{apply_transform_edit, TransformEdit};
    use bevy::{math::{EulerRot, Quat, Vec3}, prelude::*};

    #[test]
    fn spawn_shape_rotation_tuple_matches_transform_edit_order() {
        let mut world = World::new();
        world.init_resource::<Assets<Mesh>>();
        world.init_resource::<Assets<StandardMaterial>>();
        let euler = Vec3::new(23.0, 41.0, -17.0);
        let readback = spawn_test_shape(
            &mut world,
            TestShape::Cuboid,
            [0.8, 0.8, 0.8],
            None,
            None,
            None,
            Some(euler),
            None,
            None,
            false,
        )
        .expect("测试 cuboid 构建应成功");
        apply_transform_edit(
            &mut world,
            readback.entity,
            TransformEdit { rotation_euler_deg: Some(euler), ..Default::default() },
        )
        .expect("新实体应包含 Transform");
        let from_spawn = world.get::<Transform>(readback.entity).unwrap().rotation;
        let from_edit = Quat::from_euler(
            EulerRot::YXZ,
            euler.x.to_radians(),
            euler.y.to_radians(),
            euler.z.to_radians(),
        );
        assert!(from_spawn.abs_diff_eq(from_edit, 1e-6));
    }
}
