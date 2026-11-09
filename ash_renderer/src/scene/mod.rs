//! ECS 侧取数：从 bevy 的资产容器与 World 里取渲染要用的数，全部系统不碰
//! Vulkan 符号。分机制/业务两侧：
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | `mechanism` | 机制侧：账本与资产驻留、宿主补位、输入交互、对象/资产编辑、场景生成——换任何场景内容都不变 |
//! | `content` | 业务侧：场景内容参数（进场/取景/灯光）+ 随内容走的一次性核验 |
//!
//! 两个子模块都私有；scene 外部只走本模块根上的重出口（`crate::scene::X`）。

mod content;
mod mechanism;

// content
pub use content::SceneEntryPlugin;

// mechanism/assets
pub use mechanism::{
    anchor_level, spawn_lod_ladder, AshAssetEventPlugin, AshLedgerPlugin, AshReadinessPlugin,
    AshResidencyPlugin, AssetInvalidations, AssetReadiness, AssetRetirements, DrawListVerify,
    GapMode, GapSnapshot, InstanceLedger, InstanceRow, LodDemand, LodGroups, ResidencySet,
    ResidencySettings, ResidencyStats,
};
// mechanism/editing
pub use mechanism::{
    apply_transform_edit, load_image_slot, load_mesh_rebind, mutate_asset, replace_handles,
    spawn_primitive_clone, spawn_with_loaded_mesh, AssetOpError, MutateAsset, TransformEdit,
    TransformEditError, TransformEditQueue,
};
pub(crate) use mechanism::{
    apply_transform_edits, EntityRow, HandleSwapReadback, ObjectOpError, SceneTreeIndex,
    SceneTreeNode, TREE_MAX_DEPTH,
};

// mechanism/generation
pub use mechanism::{
    chunk_census, despawn_chunk, generate_chunks, generate_instances, spawn_test_shape,
    AshWorkloadPlugin, ChunkGenParams, SceneChunk, SceneGenParams, TestShape, WorkloadSettings,
};

// mechanism/host
pub use mechanism::{
    AshCameraAspectPlugin, AshCameraGuardPlugin, AshMaterialHookPlugin, AshVisibilityExemptPlugin,
};

// mechanism/interaction
pub(crate) use mechanism::{apply_camera_command, CameraCommandError, CameraOrbitCommand};
pub use mechanism::{
    apply_sun_rotation, clamp_elevation, AshCameraControlPlugin, AshLightControlPlugin,
    PointerCapture, PointerCaptureSet, SunOrbit,
};
