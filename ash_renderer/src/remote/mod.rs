//! BRP 远程直控通道：宿主 World 经 HTTP JSON-RPC 暴露在 127.0.0.1（开发实例用
//! bevy_remote 默认端口 15702，拷出 target 的正式版用 [`PUBLISHED_PORT`]，环境
//! 变量 `ASH_BRP_PORT` 可改，见 [`brp_port`]），AI/脚本不开 UI 即可查询与改场景。
//! 读侧优先用官方 inspection 方法族（`world.query`/`world.inspect` 等，随
//! [`RemotePlugin`] 默认注册，`rpc.discover` 可列全量）；本模块只注册官方够不着
//! 的自定义方法，写侧全部复用 scene 机制层封装，与 egui 面板同一实现。
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`edit::scene`] | 场景直控：层级树读、Transform/相机/太阳方向写 |
//! | [`edit::mouse`] | 拟鼠标：注入 `Messages<WindowEvent>`，与 bevy_winit 同层 |
//! | [`edit::objects`] | 对象生命周期：spawn 克隆/换柄 + increment_stats 账本读数 |
//! | [`edit::shapes`] | 测试实体：spawn_shape 五形状 + 颜色/PBR 参数、spawn_lod 阶梯体 |
//! | [`edit::generate`] | 大场景生成：generate_scene 批量铺实例、chunk 生成/盘点/整块删除 |
//! | [`edit::assets`] | 资产内容变更与运行期动态加载：mutate/load_mesh/load_image/load_world/reload |
//! | [`features::cull`] | 视锥/遮挡剔除：cull_stats 对照、开关、Hi-Z 读回与遮挡判定参照 |
//! | [`features::lod`] | LOD：lod_census 档位对照、开关与判据参数 |
//! | [`features::cluster`] | cluster 光照与透明层：灯表增删改读、cluster/透明排序对照 |
//! | [`features::shadow`] | 阴影：开关 + 逐 cascade 标记对照 |
//! | [`features::render_settings`] | 画面设置：Bloom/tonemap、SMAA、着色调试模式 |
//! | [`features::draw`] | indirect 闭环读数与组装对照开关 |
//! | [`features::residency`] | 驻留管理器读数与设置 |
//! | [`features::gap`] | GPU 缺口请求读数与模式 |
//! | [`census::ring_census`](mod@census::ring_census) | 环与常驻表 GPU 内容 vs 宿主期望字节对账 |
//! | [`census::perf`] | 性能读数、原始样本导出与 workload 变更形态开关 |
//! | [`census::monitor`] | 持续监控读数（与 egui 监控窗口同账）与帧率同步开关 |
//! | [`census::layout_contract`](mod@census::layout_contract) | 描述符集布局与管线配方清单 |
//!
//! 分组目录：`edit`（场景编辑写通道，操作本体在 scene 机制层）/ `features`
//! （渲染功能读数与开关）/ `census`（全帧对账与监控）/ `common`（公共标准件）。
//!
//! 公共件收在 [`common`]：`params`（参数解析与协议错误）、`readback`（GPU 读回
//! 与字节解析）、`reference`（CPU 参照用相机矩阵与投屏钳制）。
//!
//! 机制与边界（详见 `.agents/docs/笔记/Bevy/bevy_remote：BRP远程协议与自定义方法.md`）：
//! handler 本身是 Bevy system，跑在 `RemoteLast`（Last 之后）按帧节拍执行——写值
//! 下一帧生效，参数即请求 `params` 的 JSON、返回值即响应。协议层错误一律回
//! `BrpError`，不 panic。安全边界：只绑 loopback、无鉴权，仅限本机调试。

mod census;
mod common;
mod edit;
mod features;


use bevy::{
    prelude::*,
    render::RenderApp,
    remote::{
        http::{DEFAULT_PORT, HostPort, RemoteHttpPlugin},
        RemotePlugin,
    },
};

use self::{
    census::{
        layout_contract::layout_contract,
        monitor::{frame_sync_cmd, monitor_stats},
        perf::{perf_samples, perf_stats, workload},
        ring_census::ring_census,
    },
    edit::{
        assets::{load_image_cmd, load_mesh_cmd, load_world_cmd, mutate_asset_cmd, reload_asset_cmd},
        generate::{chunk_census, chunk_despawn, generate_chunks_cmd, generate_scene},
        mouse::{mouse_button, mouse_move, mouse_status, mouse_wheel},
        objects::{increment_stats, replace_handles, spawn_primitive},
        scene::{scene_tree, set_camera, set_transform, sun_get, sun_set},
        shapes::{spawn_lod, spawn_shape},
    },
    features::{
        cluster::{cluster_census, light_remove, light_spawn, light_stats, light_update, transparent_sort_check},
        cull::{cull_set_enabled, cull_stats, hiz_census, occlusion_probe, occlusion_set_enabled},
        draw::{draw_stats, drawlist_verify_set},
        gap::{gap_census, gap_set, gap_stats},
        lod::{lod_census, lod_set_enabled, lod_set_params},
        render_settings::{bloom_set, set_render_mode, smaa_set},
        residency::{residency_set, residency_stats},
        shadow::{shadow_set, shadow_stats},
    },
};

/// BRP 远程通道插件：装协议 + HTTP 传输 + 全部自定义方法。宿主 main 照插件清单
/// 惯例只 `add_plugins`，方法注册收在本插件内（协议面注册单点）。
pub struct AshRemotePlugin;

impl Plugin for AshRemotePlugin {
    fn build(&self, app: &mut App) {
        let port = brp_port();
        app.add_plugins((
            RemotePlugin::default()
                .with_method_main("ash_renderer/scene_tree", scene_tree)
                .with_method_main("ash_renderer/set_transform", set_transform)
                .with_method_main("ash_renderer/set_camera", set_camera)
                .with_method_main("ash_renderer/spawn_primitive", spawn_primitive)
                .with_method_main("ash_renderer/spawn_shape", spawn_shape)
                .with_method_main("ash_renderer/spawn_lod", spawn_lod)
                .with_method_main("ash_renderer/generate_scene", generate_scene)
                .with_method_main("ash_renderer/generate_chunks", generate_chunks_cmd)
                .with_method_main("ash_renderer/chunk_census", chunk_census)
                .with_method_main("ash_renderer/chunk_despawn", chunk_despawn)
                .with_method_main("ash_renderer/replace_handles", replace_handles)
                .with_method_main("ash_renderer/mutate_asset", mutate_asset_cmd)
                .with_method_main("ash_renderer/load_mesh", load_mesh_cmd)
                .with_method_main("ash_renderer/load_image", load_image_cmd)
                .with_method_main("ash_renderer/load_world", load_world_cmd)
                .with_method_main("ash_renderer/reload_asset", reload_asset_cmd)
                .with_method_main("ash_renderer/increment_stats", increment_stats)
                .with_method_main("ash_renderer/mouse_move", mouse_move)
                .with_method_main("ash_renderer/mouse_button", mouse_button)
                .with_method_main("ash_renderer/mouse_wheel", mouse_wheel)
                .with_method_main("ash_renderer/mouse_status", mouse_status)
                .with_method_main("ash_renderer/cull_stats", cull_stats)
                .with_method_main("ash_renderer/light_spawn", light_spawn)
                .with_method_main("ash_renderer/light_update", light_update)
                .with_method_main("ash_renderer/light_remove", light_remove)
                .with_method_main("ash_renderer/light_stats", light_stats)
                .with_method_main("ash_renderer/sun_get", sun_get)
                .with_method_main("ash_renderer/sun_set", sun_set)
                .with_method_main("ash_renderer/cluster_census", cluster_census)
                .with_method_main("ash_renderer/transparent_sort_check", transparent_sort_check)
                .with_method_main("ash_renderer/hiz_census", hiz_census)
                .with_method_main("ash_renderer/occlusion_probe", occlusion_probe)
                .with_method_main("ash_renderer/occlusion_set_enabled", occlusion_set_enabled)
                .with_method_main("ash_renderer/cull_set_enabled", cull_set_enabled)
                .with_method_main("ash_renderer/lod_census", lod_census)
                .with_method_main("ash_renderer/lod_set_enabled", lod_set_enabled)
                .with_method_main("ash_renderer/lod_set_params", lod_set_params)
                .with_method_main("ash_renderer/draw_stats", draw_stats)
                .with_method_main("ash_renderer/drawlist_verify_set", drawlist_verify_set)
                .with_method_main("ash_renderer/residency_stats", residency_stats)
                .with_method_main("ash_renderer/residency_set", residency_set)
                .with_method_main("ash_renderer/gap_stats", gap_stats)
                .with_method_main("ash_renderer/gap_census", gap_census)
                .with_method_main("ash_renderer/gap_set", gap_set)
                .with_method_main("ash_renderer/perf_stats", perf_stats)
                .with_method_main("ash_renderer/perf_samples", perf_samples)
                .with_method_main("ash_renderer/workload", workload)
                .with_method_main("ash_renderer/monitor_stats", monitor_stats)
                .with_method_main("ash_renderer/frame_sync", frame_sync_cmd)
                .with_method_main("ash_renderer/set_render_mode", set_render_mode)
                .with_method_main("ash_renderer/ring_census", ring_census)
                .with_method_main("ash_renderer/shadow_set", shadow_set)
                .with_method_main("ash_renderer/bloom_set", bloom_set)
                .with_method_main("ash_renderer/smaa_set", smaa_set)
                .with_method_main("ash_renderer/shadow_stats", shadow_stats)
                .with_method_main("ash_renderer/layout_contract", layout_contract),
            RemoteHttpPlugin::default().with_port(port),
        ));
        // render subapp 的 BRP 端口跟随主端口 +1（build 时机早于 RenderStartup，
        // 改写 HostPort 来得及）
        if let Some(mut host) = app
            .get_sub_app_mut(RenderApp)
            .and_then(|render| render.world_mut().get_resource_mut::<HostPort>())
        {
            host.0 = port + 1;
        }
        info!(
            "BRP（{}）: http://127.0.0.1:{port}（render subapp :{}）",
            if is_dev_instance() { "开发实例" } else { "正式版" },
            port + 1
        );
    }
}

/// 正式版（publish 出去的 exe）的 BRP 默认端口，render subapp 用 +1。和开发
/// 实例的默认端口错开，自动测试按默认端口连接时不会打到正在玩的正式版。
pub const PUBLISHED_PORT: u16 = 15710;

/// BRP 主端口（render subapp 跟随 +1，所以 65535 不可用）。环境变量
/// `ASH_BRP_PORT` 优先（值非法 warn 后忽略）；否则开发实例用默认端口，
/// 正式版用 [`PUBLISHED_PORT`]。
fn brp_port() -> u16 {
    if let Ok(v) = std::env::var("ASH_BRP_PORT") {
        match v.parse::<u16>() {
            Ok(p) if p != 0 && p != u16::MAX => return p,
            _ => warn!("ASH_BRP_PORT='{v}' 不是合法端口，忽略"),
        }
    }
    if is_dev_instance() { DEFAULT_PORT } else { PUBLISHED_PORT }
}

/// exe 是否从 cargo 的构建产物目录（`target/<profile>/` 及其子目录）启动。
/// 同一个 exe 拷到别处就判为正式版：OUT_DIR 在编译期写死为
/// `target/<profile>/build/<pkg>-<hash>/out`，运行时拿 exe 实际位置去比。
/// 判不出来时按正式版处理——开发测试连不上会立刻暴露，比误连正式版安全。
fn is_dev_instance() -> bool {
    let Some(profile_dir) = std::path::Path::new(env!("OUT_DIR")).ancestors().nth(3) else {
        return false;
    };
    let (Ok(exe), Ok(profile_dir)) = (
        std::env::current_exe().and_then(std::fs::canonicalize),
        std::fs::canonicalize(profile_dir),
    ) else {
        return false;
    };
    exe.starts_with(profile_dir)
}
