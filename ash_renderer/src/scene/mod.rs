//! ECS 侧取数：从 bevy 的资产容器与 World 里取渲染要用的数，全部系统不碰
//! Vulkan 符号。分机制/业务两半：
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`mechanism`] | 机制半边：材质缝补位、稳定槽账本与变更发现（4.1 起增量维护，取代每帧全量快照）、宽高比补位——换任何场景内容都不变 |
//! | [`content`] | 业务半边：场景内容参数（进场/取景/灯光）+ 随内容走的一次性核验 |
//! | `util` | 子模块公共小工具 |

mod content;
pub(crate) mod mechanism;
mod util;

pub use content::SceneEntryPlugin;
pub use mechanism::{
    AshCameraAspectPlugin, AshCameraControlPlugin, AshLedgerPlugin, AshMaterialHookPlugin,
    IncrementStats, InstanceLedger, InstanceRow,
};
