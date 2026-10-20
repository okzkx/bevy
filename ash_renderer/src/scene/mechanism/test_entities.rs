//! 测试实体方法族本体（6.1.1）：按形状 + 颜色程序化造单个测试体——
//! 遮挡剔除（Hi-Z）段的遮挡对/墙体样本载具，也是通用的"快速摆一个几何体"
//! 测试能力。与 5.4 生成器（[`super::scene_gen`]）同区同构：同为程序化造
//! 测试内容的机制层，BRP 薄壳在 [`crate::remote::shapes`]，egui 面板将来
//! 共用本层。
//!
//! 资产经济：mesh 按形状缓存、材质按颜色缓存（资源 [`TestShapeAssets`]，
//! 惰性初始化）——同形状/同颜色重复 spawn 共享柄零新资产，防重复上传
//! churn；首次出现的 mesh/材质照常走 4.1 上传链。实体带 `Mesh3d` +
//! `MeshMaterial3d` + `Transform` 走账本 Changed 通道自动入行，渲染路径
//! 零触碰（与 [`spawn_primitive_clone`] 同一条进 GPU 的路）。
//!
//! 形状口径（立案拍板五形状，立方体承担墙体遮挡样本）：全部单位尺寸
//!（半径 0.5 / 高 1.0 / 立方体 1³），尺寸缩放交给 Transform.scale——
//! 墙体遮挡样本 = 立方体非均匀缩放直出。颜色 sRGB [0,1] 三元组，
//! 材质只设 base_color（全局 ASH_RENDER_MODE 下的 Lambert 路径）。

use std::collections::HashMap;

use bevy::{pbr::StandardMaterial, prelude::*};

use super::ObjectOpError;

/// 测试体形状枚举（BRP 参数面，小写蛇形：sphere/cylinder/capsule/cone/cuboid）。
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
    /// 树内默认名与读回共用的小写名。
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

/// 材质缓存键：sRGB 三元组的位模式（同输入值同位 → 命中；f32 直接做
/// HashMap 键无 Eq，位模式是精确等价的整数化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ColorKey([u32; 3]);

/// 测试实体资产缓存：mesh 按形状、材质按颜色。惰性初始化——首次 spawn
/// 才进 World，不预造任何资产（空场压测/默认启动零成本）。
#[derive(Resource, Default)]
pub struct TestShapeAssets {
    meshes: HashMap<TestShape, Handle<Mesh>>,
    materials: HashMap<ColorKey, Handle<StandardMaterial>>,
}

/// 测试实体读回：实体号与资产身份（写后读对账用，increment_stats 入行
/// 对账的锚点）。
#[derive(Debug)]
pub struct TestShapeReadback {
    pub entity: Entity,
    pub shape: TestShape,
    pub mesh: AssetId<Mesh>,
    pub material: AssetId<StandardMaterial>,
    /// 本次是否新建了 mesh 资产（false = 缓存命中共享柄，零新上传）。
    pub mesh_created: bool,
}

/// spawn 一个测试体：形状 mesh + 按色 Lambert 材质 + 给定变换，无父独立
/// 成树。transform 各量独立可选（缺省 = 原点/零旋转/单位缩放，形状本体
/// 单位尺寸，尺寸交给 scale）。账本经 Added 通道自动入行。
///
/// # Errors
/// 形状 mesh 构造失败（球体细分越限——单位口径下实际不可达，防御性上抛）。
#[expect(
    clippy::too_many_arguments,
    reason = "spawn 参数面即 BRP 请求字段的逐项镜像（形状/色/变换/命名/缓存旁路），收束会藏掉与协议面的逐项对账"
)]
pub fn spawn_test_shape(
    world: &mut World,
    shape: TestShape,
    color: [f32; 3],
    translation: Option<Vec3>,
    rotation_euler_deg: Option<Vec3>,
    scale: Option<Vec3>,
    name: Option<&str>,
    unique: bool,
) -> Result<TestShapeReadback, ObjectOpError> {
    // unique（6.3 流送测试基建）：绕过形状缓存直接造新资产——每实体独立
    // mesh 身份（内容相同身份不同），"世界大于驻留"的压测前提；缺省 false
    // 走缓存共享（同参零新上传的既有语义不变）。
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
            // 资源惰性首建（resource_mut 不建，缺席即 panic）
            world
                .get_resource_or_insert_with(TestShapeAssets::default)
                .meshes
                .insert(shape, handle.clone());
            (handle, true)
        }
    }
    };
    let key = ColorKey(color.map(f32::to_bits));
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
        // 欧拉角 YXZ 度数，与 set_transform/属性面板同口径
        transform.rotation =
            Quat::from_euler(EulerRot::YXZ, r.y.to_radians(), r.x.to_radians(), r.z.to_radians());
    }
    if let Some(s) = scale {
        transform.scale = s;
    }
    let name = name
        .map(str::to_owned)
        .unwrap_or_else(|| format!("测试体-{}", shape.as_str()));
    let entity = world
        .spawn((
            Mesh3d(mesh_handle),
            MeshMaterial3d(material_handle),
            transform,
            Name::new(name),
        ))
        .id();
    // 刚 spawn 成功，两组件必在；unwrap 的是不变式而非外部输入（Tier② 语义）
    let mesh = world
        .get::<Mesh3d>(entity)
        .map(|m| m.0.id())
        .expect("spawn 带 Mesh3d 后立即查无");
    let material = world
        .get::<MeshMaterial3d<StandardMaterial>>(entity)
        .map(|m| m.0.id())
        .expect("spawn 带 MeshMaterial3d 后立即查无");
    Ok(TestShapeReadback {
        entity,
        shape,
        mesh,
        material,
        mesh_created,
    })
}

/// 五形状 → `Mesh`：bevy 基础体构造（出处核实 2026-10-19，`examples/3d/3d_shapes.rs`
/// 同款调用面）——四形 builder `.build()` 直出，球体 ico 细分返回 Result
///（细分上限内不失败，越限 = 参数非法回协议错误不 panic）。
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
