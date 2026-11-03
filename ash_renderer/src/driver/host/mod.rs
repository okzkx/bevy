//! 宿主桥：bevy 调度 ↔ Vulkan 资源的接插件。
//!
//! main 只做统筹（插件组装，见 `src/main.rs`），Vulkan 侧的全部编排住 driver 层：
//!
//! | 时机 | 系统 | 系统本体 | 职责 |
//! |---|---|---|---|
//! | `Startup` | `init_vulkan` | [`init`] | 串链创建全部 Vulkan 资源（**全有或全无**）；失败冒泡统一通道 → 优雅退出 |
//! | `Last` | `flush_uploads`（upload 模块注册，先于本表下一行） | [`super::upload`] | 上传链：账本脏行盘点 → 合批 transfer 提交，先于帧循环 |
//! | `Last` | `draw_frame.run_if(resource_exists::<Context>)` | [`frame`] | 帧循环：处理 resize → 等 fence → acquire → 组装 DrawList → 录制提交 → present |
//! | `Last` | `teardown_vulkan.in_set(OnAppExitSystems)` | [`init`] | AppExit 写入后、窗口销毁前反序拆除 |
//!
//! 本模块不持有 Vulkan 状态，只做接线（禁渲染补位见 build 内注释）与系统注册；
//! 创建链细节在 [`crate::render`]，子模块只消费重出口类型。
//!
//! 失败处理（**非必要不 panic**）：[`init`]/[`frame`] 的系统签名 `Result<(), BevyError>`，
//! `?` 冒泡给 `tier_handler` 按 Severity 分流——不影响运行的（如 OUT_OF_DATE/SUBOPTIMAL
//! 重建后重试）warn 后继续；影响运行的优雅退出（资源反序拆除、窗口自关、退出码 1）。
//! acquire 之后的失败为何必须退出见 [`frame`] 模块文档。

use bevy::{
    app::OnAppExitSystems,
    image::{CompressedImageFormatSupport, CompressedImageFormats, ImageLoader},
    prelude::*,
    winit::{UpdateMode, WinitSettings},
};

use ash_macros::system;
use crate::render::{
    BloomSettings, Context, CullDiagnostics, CullSettings, FrameSyncSettings, IndirectStats,
    LodDiagnostics, LodSettings, OcclusionDiagnostics, OcclusionSettings, PerfLedger, RenderMode,
    ShadowSettings, SmaaSettings,
};
use crate::scene::DrawListVerify;

mod drawlist;
mod frame;
mod frame_sync;
mod init;

/// 帧循环系统经宿主桥重导出：upload 的排序引用走 `crate::driver::host::draw_frame`
///（"先上传后画"的编排契约以 host 为锚点）。
pub(crate) use frame::draw_frame;
use frame_sync::{apply_frame_sync, fallback_wait};
use init::{init_vulkan, teardown_vulkan};

/// 宿主桥插件：禁渲染补位（`CompressedImageFormatSupport` 自报）+ Vulkan 三系统进调度。
///
/// 禁渲染名单本身是统筹决策，留在 main；凡"禁了渲染之后必须有人补位"的运行期
/// 职责都归本插件。
pub struct AshHostPlugin;

impl Plugin for AshHostPlugin {
    fn build(&self, app: &mut App) {
        // 禁渲染补位：压缩纹理支持自报 + ImageLoader 注册。两者原本由渲染插件的
        // finish() 完成——禁渲染后无人做，贴图 load 会永远卡在 Loading。
        // NONE 只表示不解压缩纹理格式（basis/ktx2），png/jpeg 照常；贴图解码不依赖 GPU。
        app.insert_resource(CompressedImageFormatSupport(CompressedImageFormats::NONE))
            .register_asset_loader(ImageLoader::new(CompressedImageFormats::NONE))
            // 各 GPU 特性的开关与诊断账面 + 帧样本账：纯 ECS 侧状态，overlay
            // 缺席时 BRP 仍可用
            .init_resource::<CullSettings>()
            .init_resource::<CullDiagnostics>()
            .init_resource::<IndirectStats>()
            .init_resource::<OcclusionSettings>()
            .init_resource::<OcclusionDiagnostics>()
            .init_resource::<LodSettings>()
            .init_resource::<LodDiagnostics>()
            .init_resource::<ShadowSettings>()
            // Bloom 开关与参数：BRP bloom_set 直达
            .init_resource::<BloomSettings>()
            // SMAA 开关：BRP smaa_set 直达
            .init_resource::<SmaaSettings>()
            .init_resource::<PerfLedger>()
            // DrawList 组装对照账面（BRP 开关对照模式）
            .init_resource::<DrawListVerify>()
            // 着色模式：初值 = env ASH_RENDER_MODE；overlay 缺席时帧循环与 BRP 照常读写
            .insert_resource(RenderMode::from_env())
            // 帧率同步设置：开关默认 true（首次运行自动同步）+ 刷新率读数
            .init_resource::<FrameSyncSettings>()
            // update 节流：MAILBOX 没有 vsync 背压，默认 Continuous 会空转烧 CPU；
            // Reactive(1/60) 让空闲时 update 按主屏刷新率封顶，事件（拖拽/输入）仍即时驱动。
            // 之后由 apply_frame_sync 每帧维护，这里只是显示器信息就绪前的初值。
            .insert_resource(WinitSettings {
                focused_mode: UpdateMode::reactive(fallback_wait()),
                unfocused_mode: UpdateMode::reactive_low_power(fallback_wait()),
            })
            .add_systems(Startup, (announce, init_vulkan))
            .add_systems(Update, apply_frame_sync)
            // 守卫：初始化失败时资源不插入（全有或全无），缺 Context 直接跳过，
            // 等 AppExit 走退出链——否则 Res<Context> 缺席会让帧循环每帧报错
            .add_systems(
                Last,
                draw_frame
                    .run_if(resource_exists::<Context>)
                    .before(OnAppExitSystems),
            )
            // 退出拆除：OnAppExitSystems 在 AppExit 写入之后、despawn_windows 销毁窗口
            // 之前跑——Vulkan 对象（surface）必须死在 hwnd 之前。
            .add_systems(
                Last,
                teardown_vulkan
                    .in_set(OnAppExitSystems)
                    .run_if(|messages: Res<Messages<AppExit>>| !messages.is_empty()),
            );
    }
}

#[system]
fn announce() {
    info!("宿主壳启动：渲染族 8 插件已禁用，无 RenderApp / 无 wgpu 初始化");
}
