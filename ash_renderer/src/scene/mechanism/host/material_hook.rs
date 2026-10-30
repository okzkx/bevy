//! 禁用 PbrPlugin 时，注册 StandardMaterial 容器并为 glTF loader 接入材质转换钩子。
//! 钩子写入与读取的材质标签必须完全一致。

use bevy::{
    asset::{AssetApp, LoadContext},
    gltf::{
        extensions::{ErasedGltfExtensionHandler, GltfExtensionHandler, GltfExtensionHandlers},
        // 此处的 gltf-rs 类型与 bevy::gltf 下的同名资产类型不是同一组类型。
        gltf,
        GltfAssetLabel, GltfLoaderSettings, GltfMaterial,
    },
    pbr::{gltf::standard_material_from_gltf_material, MeshMaterial3d, StandardMaterial},
    prelude::*,
};

use ash_macros::system;

/// 注册 StandardMaterial 容器、反射类型与 glTF 材质钩子。
pub struct AshMaterialHookPlugin;

impl Plugin for AshMaterialHookPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GltfExtensionHandlers>()
            .init_asset::<StandardMaterial>()
            // WorldAssetRoot 反射写入 MeshMaterial3d 依赖这两项注册；缺失时会 panic。
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

// 坑：材质写入与 primitive 取回必须使用同一 `{label}/std` 标签，否则实体拿不到材质。
#[derive(Default, Clone)]
struct AshMaterialHook;

impl GltfExtensionHandler for AshMaterialHook {
    fn dyn_clone(&self) -> Box<dyn ErasedGltfExtensionHandler> {
        Box::new((*self).clone())
    }

    /// 为没有材质的 primitive 注册 DefaultMaterial/std 回退材质。
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

    /// 将解析后的 glTF 材质转换为 StandardMaterial 并写入对应标签。
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

    /// 将对应的 StandardMaterial 句柄写入 primitive 实体。
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

/// Startup 自检应恰有一个材质钩子；数量异常时告警，避免重复注册造成材质处理异常。
#[system]
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
    if hook_count != 1 {
        warn!(
            "材质缝自检异常：GltfExtensionHandlers 挂载 {hook_count} 个 handler（期望 1 = \
             AshMaterialHook；0 = 材质缝断裂，>1 = 双注册劣化），Assets<StandardMaterial> {container}"
        );
    } else {
        info!("材质缝自检：Assets<StandardMaterial> {container}；GltfExtensionHandlers 挂载 {hook_count} 个 handler（期望 1 = AshMaterialHook）");
    }
}
