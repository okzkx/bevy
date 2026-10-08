//! 场景侧机制层（3.6.2 分层定案）：禁渲染补位与采集机制——换任何场景内容都
//! 不变的那半边。零 Vulkan 代码，也不含任何写死的场景参数。
//!
//! | 子模块 | 职责 | 施工 |
//! |---|---|---|
//! | [`material_hook`] | 材质缝补位：补 `Assets<StandardMaterial>` 容器 + 自写三钩子 | 3.1.1 |
//! | [`collect`] | 采集机制：PostUpdate 帧末直读 primitive 三样，产 CollectedScene 快照 | 3.1.4 |
//! | [`camera_aspect`] | 宽高比补位：随窗口 resize 修正任意相机的 aspect_ratio | 3.1.3 |
//!
//! 判定线：driver（上传、帧循环）只依赖本层，业务半边（[`crate::scene::content`]）
//! 没有任何引擎侧消费者——切线是现状里长出来的，不是设计出来的。

mod camera_aspect;
mod collect;
mod material_hook;

pub use camera_aspect::AshCameraAspectPlugin;
pub use collect::{AshCollectPlugin, CollectedPrimitive, CollectedScene};
pub use material_hook::AshMaterialHookPlugin;

// 排序锚点（语义 = 快照已重建完毕）：pub(crate) 让业务侧核验
// （scene::content::collect_report）能 .after(collect_scene) 挂链。
pub(crate) use collect::collect_scene;
