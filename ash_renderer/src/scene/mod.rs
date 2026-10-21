//! ECS 侧取数：从 bevy 的资产容器与 World 里取渲染要用的数，全部系统不碰
//! Vulkan 符号。分机制/业务两半：
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`mechanism`] | 机制半边：材质缝补位、稳定槽账本与变更发现（跨帧增量维护）、资产事件读侧、宽高比补位——换任何场景内容都不变 |
//! | [`content`] | 业务半边：场景内容参数（进场/取景/灯光）+ 随内容走的一次性核验 |
//! | `util` | 子模块公共小工具 |

mod content;
pub(crate) mod mechanism;
mod util;

pub use content::SceneEntryPlugin;
pub use mechanism::{
    AshAssetEventPlugin, AshCameraAspectPlugin, AshCameraControlPlugin, AshCameraGuardPlugin,
    AshLedgerPlugin,
    AshMaterialHookPlugin, AshReadinessPlugin, AshResidencyPlugin, AshVisibilityExemptPlugin,
    AshWorkloadPlugin, AssetInvalidations, AssetReadiness, AssetRetirements, IncrementStats,
    InstanceLedger,
    InstanceRow, LodDemand, LodGroups, LodLadderReadback, ReadinessStats, RecycleStats,
    ResidencySettings, ResidencyStats, WorkloadSettings, GapMode, GapSnapshot, anchor_level,
    gap_asset_ids, spawn_lod_ladder,
};
