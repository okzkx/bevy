//! ECS 侧取数（step3 施工 3.1）：从 bevy 的资产容器与 World 里取渲染要用的数，
//! 全部系统不碰 Vulkan 符号。3.6.2 分层定案：分机制/业务两半——
//!
//! | 子模块 | 职责 | 施工 |
//! |---|---|---|
//! | [`mechanism`] | 机制半边：材质缝补位、采集快照、宽高比补位——换任何场景内容都不变 | 3.1.1/3.1.4/3.1.3 |
//! | [`content`] | 业务半边：FlightHelmet 进场、相机取景、灯光参数 + 施工核验 | 3.1.2/3.1.3 |
//! | `util` | 子模块公共小工具 | — |
//!
//! 机制与证据：`.agents/docs/3-静态取数链路/3.1-ECS侧取数/`

mod content;
mod mechanism;
mod util;

pub use content::SceneEntryPlugin;
pub use mechanism::{
    AshCameraAspectPlugin, AshCollectPlugin, AshMaterialHookPlugin, CollectedPrimitive,
    CollectedScene,
};
