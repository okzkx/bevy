//! 场景侧机制层：禁渲染补位与采集机制——换任何场景内容都不变的那半边。
//! 零 Vulkan 代码，也不含写死的场景参数。
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`material_hook`] | 材质缝补位：补 `Assets<StandardMaterial>` 容器 + 自写三钩子 |
//! | [`collect`] | 采集机制：PostUpdate 帧末直读 primitive 三样，产 CollectedScene 快照 |
//! | [`camera_aspect`] | 宽高比补位：随窗口 resize 修正任意相机的 aspect_ratio |
//! | [`camera_control`] | 相机轨道控制：左键拖拽环绕 + 滚轮推拉，目标点由组件携带 |
//!
//! 判定线：driver（上传、帧循环）只依赖本层，业务半边（[`crate::scene::content`]）
//! 没有任何引擎侧消费者。

mod camera_aspect;
mod camera_control;
mod collect;
mod material_hook;

pub use camera_aspect::AshCameraAspectPlugin;
pub use camera_control::{AshCameraControlPlugin, CameraOrbit};
pub use collect::{AshCollectPlugin, CollectedPrimitive, CollectedScene};
pub use material_hook::AshMaterialHookPlugin;

// 排序锚点（语义 = 快照已重建完毕）：业务侧核验
// （scene::content::collect_report）用 `.after(collect_scene)` 挂链。
pub(crate) use collect::collect_scene;
