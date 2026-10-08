//! 宿主桥：bevy 调度 ↔ Vulkan 资源的接插件。
//!
//! main 只做统筹（插件组装，见 `src/main.rs`），Vulkan 侧的全部编排住 driver 层：
//!
//! | 时机 | 系统 | 系统本体 | 职责 |
//! |---|---|---|---|
//! | `Startup` | `init_vulkan` | [`init`] | 串链创建全部 Vulkan 资源（**全有或全无**）；失败 → `error!` + `AppExit::error()` 优雅退出 |
//! | `Last` | `flush_uploads`（upload 模块注册，先于本表下一行） | [`super::upload`] | 上传链：快照去重 → 合批 transfer 提交，先于帧循环 |
//! | `Last` | `draw_frame.run_if(resource_exists::<Context>)` | [`frame`] | 帧循环：resize 闸门 → 等 fence → acquire → 组装 DrawList → 录制提交 → present |
//! | `Last` | `teardown_vulkan.in_set(OnAppExitSystems)` | [`init`] | AppExit 写入后、窗口销毁前反序拆除 |
//!
//! 本模块不持有 Vulkan 状态，只做接线（禁渲染补位见 build 内注释）与系统注册；
//! 创建链细节在 [`crate::vulkan`]，子模块只消费重出口类型。
//!
//! 失败策略（两 Tier，**非必要不 panic**；[`init`]/[`frame`] 各失败分支按此分流）：
//! ① 不影响运行 → warn 后丢弃继续（OUT_OF_DATE/SUBOPTIMAL 这类"重建后重试"属这层）；
//! ② 影响运行 → error 冒泡到 main 优雅退出（资源反序拆除、窗口自关、退出码 1）。
//!    帧循环里 acquire 成功之后的 Vulkan 真失败必须归②：此时同步对象已置位，
//!    warn 后继续会等一个永远无人 signal 的 fence，直接死锁（见 [`frame::draw_frame`]）。

use bevy::{
    app::OnAppExitSystems,
    image::{CompressedImageFormatSupport, CompressedImageFormats},
    prelude::*,
};

use ash_macros::system;
use crate::vulkan::Context;

mod frame;
mod init;

/// 帧循环系统经宿主桥重导出：upload 的排序引用走 `crate::driver::host::draw_frame`
///（"先上传后画"的编排契约以 host 为锚点）。
pub(crate) use frame::draw_frame;
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
            .register_asset_loader(bevy::image::ImageLoader::new(CompressedImageFormats::NONE))
            // update 节流：MAILBOX 没有 vsync 背压，默认 Continuous 会空转烧 CPU；
            // Reactive(1/60) 让空闲时 update 按主屏刷新率封顶，事件（拖拽/输入）仍即时驱动。
            .insert_resource(bevy::winit::WinitSettings {
                focused_mode: bevy::winit::UpdateMode::reactive(
                    std::time::Duration::from_secs_f64(1.0 / 60.0),
                ),
                unfocused_mode: bevy::winit::UpdateMode::reactive_low_power(
                    std::time::Duration::from_secs_f64(1.0 / 60.0),
                ),
            })
            .add_systems(Startup, (announce, init_vulkan))
            // 守卫：初始化失败时资源不插入（全有或全无），缺 Context 直接跳过，
            // 等 AppExit 走退出链——否则 Res<Context> 会在帧循环里 panic，优雅退出前功尽弃
            .add_systems(
                Last,
                draw_frame
                    .run_if(resource_exists::<Context>)
                    .before(OnAppExitSystems),
            );
        // 退出拆除：OnAppExitSystems 在 AppExit 写入之后、despawn_windows 销毁窗口
        // 之前跑——Vulkan 对象（surface）必须死在 hwnd 之前。
        app.add_systems(
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
