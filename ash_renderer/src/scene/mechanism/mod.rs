//! 场景侧机制层：禁渲染补位与取数机制——换任何场景内容都不变的那半边。
//! 零 Vulkan 代码，也不含写死的场景参数。
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`material_hook`] | 材质缝补位：补 `Assets<StandardMaterial>` 容器 + 自写三钩子 |
//! | [`ledger`] | 稳定槽账本：Entity→实例数据跨帧账本 + Changed/Removed 变更发现（4.1，取代 M2 每帧全量快照） |
//! | [`object_ops`] | 对象写操作封装（4.1.5）：spawn 克隆/换柄，BRP 与将来 egui 面板共用（despawn/摘组件走官方 BRP） |
//! | [`camera_aspect`] | 宽高比补位：随窗口 resize 修正任意相机的 aspect_ratio |
//! | [`camera_control`] | 相机轨道控制：左键拖拽环绕 + 滚轮推拉，目标点由组件携带 |
//!
//! 判定线：driver（上传、帧循环）只依赖本层，业务半边（[`crate::scene::content`]）
//! 没有任何引擎侧消费者。

mod camera_aspect;
mod camera_control;
mod ledger;
mod material_hook;
mod object_ops;

pub use camera_aspect::AshCameraAspectPlugin;
pub use camera_control::{AshCameraControlPlugin, CameraOrbit};
pub(crate) use camera_control::{apply_camera_command, CameraCommandError, CameraOrbitCommand};
pub use ledger::{AshLedgerPlugin, IncrementStats, InstanceLedger, InstanceRow};
pub use material_hook::AshMaterialHookPlugin;
pub use object_ops::{replace_handles, spawn_primitive_clone};
pub(crate) use object_ops::{HandleSwapReadback, ObjectOpError};

// 排序锚点（语义 = 账本已与 World 对账完毕）：业务侧核验
//（scene::content::collect_report）用 `.after(update_ledger)` 挂链。
pub(crate) use ledger::update_ledger;
