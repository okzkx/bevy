//! ECS 侧的渲染数据维护、宿主适配、交互与场景生成能力，不含 Vulkan 代码。
//!
//! | 目录 | 内容 |
//! |---|---|
//! | [`assets`] | 跨帧账本、资产事件、驻留管理、LOD 登记与 DrawList 组装 |
//! | [`host`] | 渲染插件补位、相机策略与可见性策略 |
//! | [`interaction`] | 指针仲裁、相机控制与日光控制 |
//! | [`editing`] | 对象/资产/Transform 操作与场景树索引 |
//! | [`generation`] | 运行期场景生成、测试实体与压测工作负载 |
//!
//! 本模块重导出常用机制接口；场景专属进场参数与核验由 [`crate::scene::content`] 管理。
//! 本层依赖 `render` 与 `common`。

mod assets;
mod editing;
mod generation;
mod host;
mod interaction;

pub use assets::{
    anchor_level, gap_asset_ids, AshAssetEventPlugin, AshLedgerPlugin, AshReadinessPlugin,
    AshResidencyPlugin, AssetInvalidations, AssetReadiness, AssetRetirements, DrawListVerify,
    GapMode, GapSnapshot, IncrementStats, InstanceLedger, InstanceRow, LodDemand, LodGroups,
    LodLadderReadback, ReadinessStats, RecycleStats, ResidencySet, ResidencySettings,
    ResidencyStats, spawn_lod_ladder,
};
pub(crate) use assets::{update_ledger, AssemblyView};
pub use editing::{
    apply_transform_edit, load_image_slot, load_mesh_rebind, mutate_asset, replace_handles,
    spawn_primitive_clone, spawn_with_loaded_mesh, AssetOpError, MutateAsset, TransformEdit,
    TransformEditError, TransformEditQueue,
};
pub(crate) use editing::apply_transform_edits;
pub(crate) use editing::{EntityRow, HandleSwapReadback, ObjectOpError, SceneTreeIndex, SceneTreeNode, TREE_MAX_DEPTH};
pub use generation::{spawn_test_shape, TestShape, AshWorkloadPlugin, WorkloadSettings,
    chunk_census, despawn_chunk, generate_chunks, generate_instances, ChunkGenParams, SceneGenParams};
pub use generation::scene_gen;
pub use host::{
    AshCameraAspectPlugin, AshCameraGuardPlugin, AshMaterialHookPlugin,
    AshVisibilityExemptPlugin,
};
pub use interaction::{
    apply_sun_rotation, clamp_elevation, AshCameraControlPlugin, AshLightControlPlugin,
    CameraOrbit, PointerCapture, PointerCaptureSet, SunOrbit,
};
pub(crate) use interaction::{apply_camera_command, CameraCommandError, CameraOrbitCommand};
