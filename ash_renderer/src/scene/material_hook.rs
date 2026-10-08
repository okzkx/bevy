//! 材质缝接线（施工 3.1.1）：补 `Assets<StandardMaterial>` 容器 + 自写三钩子。
//!
//! step2 连带禁用 PbrPlugin 后，官方材质缝两头失守：`Assets<StandardMaterial>`
//! 容器无人注册；官方 `GltfExtensionHandlerPbr` 是 `pub(crate)`，其注册点
//! `add_gltf`（bevy_pbr/src/gltf.rs:13）又在 PbrPlugin::build 里——禁用后
//! `GltfExtensionHandlers` 永远是空的，glTF loader 只会 spawn 出没有材质的
//! 裸 `Mesh3d` 实体。本模块手动接线：`init_asset` 补容器，自写 [`AshMaterialHook`]
//! 复刻官方三钩子，标签格式与 loader 的 `material_label` 约定逐字对齐。
//!
//! 机制与证据：`.agent/docs/3-静态取数链路/3.1-ECS侧取数/3.1.1-材质缝接线：自写AshMaterialHook三钩子.md`

use bevy::{
    asset::{AssetApp, LoadContext},
    gltf::{
        extensions::{ErasedGltfExtensionHandler, GltfExtensionHandler, GltfExtensionHandlers},
        // gltf = gltf-rs crate 经 bevy_gltf 再出口（bevy_gltf/src/lib.rs:171）——
        // trait 钩子签名里的 Gltf/Material/Mesh/Primitive 都是这个 crate 的类型，
        // 与 bevy::gltf 下的同名资产类型（Gltf/GltfMaterial…）不是一回事
        gltf,
        GltfAssetLabel, GltfLoaderSettings, GltfMaterial,
    },
    pbr::{gltf::standard_material_from_gltf_material, MeshMaterial3d, StandardMaterial},
    prelude::*,
};

/// 材质缝接线插件：注册 `Assets<StandardMaterial>` 容器，挂 [`AshMaterialHook`]，
/// 并注册 Startup 自检（[`report_material_seam`]）——缝的全部接线收在本插件内，
/// main 只 `add_plugins`，不引用本模块内部件。
///
/// 官方对应动作在 PbrPlugin::build 里，禁用后由本插件补位。`init_resource` 是
/// 防御性幂等调用：正常路径 GltfPlugin::build（DefaultPlugins 内，不依赖渲染）
/// 已建好 `GltfExtensionHandlers`，此处是空操作；万一 GltfPlugin 被移除，缺的
/// 是资产 loader——失败会在资产加载时以"无 loader"冒泡（Tier②），而不是这里 panic。
pub struct AshMaterialHookPlugin;

impl Plugin for AshMaterialHookPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GltfExtensionHandlers>()
            .init_asset::<StandardMaterial>()
            // 官方在 PbrPlugin（register_asset_reflect）与 MaterialPlugin::<StandardMaterial>
            // （register_type，bevy_pbr/src/material.rs:448）里做——两者都被连带禁用。
            // 不补这个注册，WorldAssetRoot 展开实体树时反射写入 MeshMaterial3d 直接 panic
            //（world_asset_spawner.rs:635，2026-09-22 实测）。
            .register_asset_reflect::<StandardMaterial>()
            .register_type::<MeshMaterial3d<StandardMaterial>>()
            .add_systems(Startup, report_material_seam);
        app.world_mut()
            .resource_mut::<GltfExtensionHandlers>()
            .0
            .write_blocking()
            .push(Box::new(AshMaterialHook));
    }
}

/// 官方 [`GltfExtensionHandlerPbr`]（`pub(crate)`，bevy_pbr/src/gltf.rs:102）的复刻。
///
/// 三钩子的调用方是 GltfLoader：每个 primitive 先算 `material_label`
/// （bevy_gltf/src/loader/gltf_ext/material.rs:162——有索引走 `Material{index}`，
/// 无材质走 `DefaultMaterial`），`on_material` 把转换出的 StandardMaterial
/// 以 `{label}/std` 入标签库，`on_spawn_mesh_and_material` 再按同一标签取
/// handle 插 `MeshMaterial3d`。标签错一个字，hook 之间就互相失联。
#[derive(Default, Clone)]
struct AshMaterialHook;

impl GltfExtensionHandler for AshMaterialHook {
    fn dyn_clone(&self) -> Box<dyn ErasedGltfExtensionHandler> {
        Box::new((*self).clone())
    }

    /// 兜底材质：glTF 文件里没写材质的 primitive 也会落到 `DefaultMaterial`
    /// 标签，这里预置一份默认 StandardMaterial 供其命中（官方同款逻辑）。
    fn on_root(
        &mut self,
        load_context: &mut LoadContext<'_>,
        _gltf: &gltf::Gltf,
        _settings: &GltfLoaderSettings,
    ) {
        let std_label = format!("{}/std", GltfAssetLabel::DefaultMaterial);
        load_context.add_labeled_asset(
            std_label,
            standard_material_from_gltf_material(&GltfMaterial::default()),
        );
    }

    /// 材质转换：loader 已把 glTF 材质解析成 [`GltfMaterial`]，这里经官方 pub
    /// 转换函数变成 StandardMaterial，存成 `{材质标签}/std`。
    fn on_material(
        &mut self,
        load_context: &mut LoadContext<'_>,
        _gltf_material: &gltf::Material,
        _material: Handle<GltfMaterial>,
        material_asset: &GltfMaterial,
        material_label: &str,
    ) {
        let std_label = format!("{material_label}/std");
        load_context.add_labeled_asset(
            std_label,
            standard_material_from_gltf_material(material_asset),
        );
    }

    /// primitive 实体收口：按 on_material 写入的同一标签取回 handle，
    /// 给裸 `Mesh3d` 实体补上 `MeshMaterial3d`——采集系统（3.1.4）以后就从这里读到材质。
    fn on_spawn_mesh_and_material(
        &mut self,
        load_context: &mut LoadContext<'_>,
        _primitive: &gltf::Primitive,
        _mesh: &gltf::Mesh,
        _material: &gltf::Material,
        entity: &mut EntityWorldMut,
        material_label: &str,
    ) {
        let std_label = format!("{material_label}/std");
        let handle = load_context.get_label_handle::<StandardMaterial>(std_label);
        entity.insert(MeshMaterial3d(handle));
    }
}

/// 材质缝自检（Startup）：缝的两侧各报一句——容器在不在、hook 挂了几个。
/// 运行时 `GltfExtensionHandlers` 应恰好含 1 个 handler（本 hook；KHR 扩展
/// 不走 handler 注册，是 loader 在 load_material 里内联解析的）。
fn report_material_seam(
    handlers: Option<Res<GltfExtensionHandlers>>,
    materials: Option<Res<Assets<StandardMaterial>>>,
) {
    let hook_count = handlers
        .as_ref()
        .and_then(|h| h.0.try_read())
        .map(|list| list.len())
        .unwrap_or(0);
    let container = if materials.is_some() { "已注册" } else { "未注册" };
    info!("材质缝自检：Assets<StandardMaterial> {container}；GltfExtensionHandlers 挂载 {hook_count} 个 handler（期望 1 = AshMaterialHook）");
}
