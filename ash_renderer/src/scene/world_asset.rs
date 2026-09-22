//! 场景进场（施工 3.1.2）：WorldAsset 的 load 请求与到货统计，零 Vulkan 代码。
//!
//! 进场本身只有三行（load + spawn + 轮询到货）；配额的三个"禁渲染补位"
//! （ImageLoader 注册、反射注册、资产根路径）机制见
//! `.agent/docs/3-静态取数链路/3.1-ECS侧取数/3.1.2-场景进场：一个load请求与三个禁渲染补位.md`。

use bevy::{
    gltf::GltfAssetLabel,
    image::Image,
    pbr::{MeshMaterial3d, StandardMaterial},
    prelude::*,
    world_serialization::WorldAssetRoot,
};

/// FlightHelmet 在 assets/ 下的相对路径（1 gltf + 1 bin + 15 png；
/// 6 材质 = Hose/RubberWood/GlassPlastic/MetalParts/LeatherParts/Lenses，
/// 施工计划 §6.8 早版写的"4 材质"与本文件不符，已订正）。
const FLIGHT_HELMET: &str = "models/FlightHelmet/FlightHelmet.gltf";

/// 场景进场请求（Startup）：`load` 立即返回占位 Handle、数据异步到货；
/// [`WorldAssetRoot`] 的组件 Add hook（bevy_world_serialization/src/lib.rs:96）
/// 在依赖就绪后把整棵实体树展开进主 World——这里只发"进场请求"，不等数据。
pub(super) fn load_flight_helmet(mut commands: Commands, server: Res<AssetServer>) {
    let scene = server.load(GltfAssetLabel::Scene(0).from_asset(FLIGHT_HELMET));
    commands.spawn(WorldAssetRoot(scene));
    info!("场景进场：已请求 {FLIGHT_HELMET}#Scene0，实体树待依赖就绪后异步展开");
}

/// 到货报告（Update，报一次即歇）：primitive 实体出现 = 实体树已展开，
/// 此刻的容器统计就是三钩子的实际触发证据。10s 未见实体按 Tier① warn 一次
///（加载失败不是本系统可修的，根因看资产侧错误日志），帧循环照常。
#[derive(Default)]
pub(super) struct ArrivalState {
    done: bool,
    waited_secs: f32,
}

pub(super) fn report_scene_arrival(
    mut state: Local<ArrivalState>,
    time: Res<Time>,
    meshes: Res<Assets<Mesh>>,
    std_materials: Res<Assets<StandardMaterial>>,
    images: Res<Assets<Image>>,
    primitives: Query<(), With<Mesh3d>>,
    with_material: Query<(), (With<Mesh3d>, With<MeshMaterial3d<StandardMaterial>>)>,
) {
    if state.done {
        return;
    }
    let total = primitives.iter().count();
    if total == 0 {
        state.waited_secs += time.delta_secs();
        if state.waited_secs > 10.0 {
            warn!("场景进场 10s 未见 primitive 实体——资产路径/loader/依赖链有问题，根因看上方加载错误日志");
            state.done = true;
        }
        return;
    }
    let with_material = with_material.iter().count();
    info!(
        "场景进场到货：primitive 实体 {total}（期望 6），带 MeshMaterial3d {with_material}/{total}（期望 6/6）；容器——Mesh {}（期望 6）、StandardMaterial {}（期望 6 = 每材质一份，兜底 DefaultMaterial 无人持柄不驻留）、Image {}（期望 17 = 内置 2 + 文件 15）",
        meshes.len(),
        std_materials.len(),
        images.len(),
    );
    state.done = true;
}
