//! 场景进场：WorldAsset 的 load 请求与到货统计，零 Vulkan 代码。
//!
//! 进场本身只有三行（load + spawn + 轮询到货）；三个"禁渲染补位"
//! （ImageLoader 注册、反射注册、资产根路径）机制见
//! `.agents/docs/3-静态取数链路/3.1-ECS侧取数/3.1.2-场景进场：一个load请求与三个禁渲染补位.md`。

use bevy::{
    gltf::GltfAssetLabel,
    image::Image,
    pbr::{MeshMaterial3d, StandardMaterial},
    prelude::*,
    world_serialization::WorldAssetRoot,
};

use ash_macros::system;

/// 默认场景：Sponza 在 assets/ 下的相对路径（1 gltf + 1 bin + 69 贴图；节点
/// scale 0.008 把 ~3700 单位的源模型缩到 ~30×12×18 米世界尺寸；1 mesh 拆
/// 103 primitive，25 材质全带 baseColor+metallicRoughness，69 张贴图全部被引用）。
const SPONZA: &str = "sponza/glTF/Sponza.gltf";

/// 启动参数 `--scene <路径>`：指定进场场景（相对 assets/，.gltf/.glb 均可），
/// 不给或缺值时回退默认 Sponza。只改 Startup 进场请求；BRP
/// `ash_renderer/load_world` 是运行期叠加口，与本参数无关。
fn scene_override() -> Option<String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let idx = args.iter().position(|a| a == "--scene")?;
    match args.get(idx + 1).filter(|v| !v.starts_with("--")) {
        Some(v) => Some(v.clone()),
        None => {
            warn!("--scene 缺值，回退默认 Sponza");
            None
        }
    }
}

/// 场景进场请求（Startup）：`load` 立即返回占位 Handle、数据异步到货；
/// [`WorldAssetRoot`] 组件到货后由 bevy_world_serialization 的
/// `world_instance_spawner` 轮询系统（Changed 触发，非组件 Add hook）
/// 把整棵实体树反射展开进主 World——这里只发"进场请求"，不等数据。
#[system]
pub(super) fn load_scene(mut commands: Commands, server: Res<AssetServer>) {
    let path = scene_override().unwrap_or_else(|| SPONZA.to_string());
    let scene = server.load(GltfAssetLabel::Scene(0).from_asset(path.clone()));
    commands.spawn(WorldAssetRoot(scene));
    info!("场景进场：已请求 {path}#Scene0，实体树待依赖就绪后异步展开");
}

/// 到货报告（Update，报一次即歇）：primitive 实体出现 = 实体树已展开，
/// 此刻的容器统计就是三钩子的实际触发证据。10s 未见实体按 Tier① warn 一次
///（加载失败不是本系统可修的，根因看资产侧错误日志），帧循环照常。
#[derive(Default)]
pub(super) struct ArrivalState {
    done: bool,
    waited_secs: f32,
}

#[system]
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
    if scene_override().is_some() {
        info!(
            "场景进场到货（--scene 自定义，无期望值口径）：primitive 实体 {total}，带 MeshMaterial3d {with_material}/{total}；容器——Mesh {}、StandardMaterial {}、Image {}",
            meshes.len(),
            std_materials.len(),
            images.len(),
        );
    } else {
        info!(
            "场景进场到货：primitive 实体 {total}（期望 103），带 MeshMaterial3d {with_material}/{total}（期望 103/103）；容器——Mesh {}（期望 103 = 每 primitive 一份）、StandardMaterial {}（期望 25 = 每材质一份，兜底 DefaultMaterial 无人持柄不驻留）、Image {}（期望 71 = 内置 2 + 文件 69）",
            meshes.len(),
            std_materials.len(),
            images.len(),
        );
    }
    state.done = true;
}
