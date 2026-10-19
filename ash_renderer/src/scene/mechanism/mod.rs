//! 场景侧机制层：禁渲染补位与取数机制——换任何场景内容都不变的那半边。
//! 零 Vulkan 代码，也不含写死的场景参数。
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`material_hook`] | 材质缝补位：补 `Assets<StandardMaterial>` 容器 + 自写三钩子 |
//! | [`ledger`] | 稳定槽账本：Entity→实例数据跨帧账本 + Changed/Removed 增量变更发现 |
//! | [`asset_events`] | 资产事件读侧：`Modified` → 失效凭据 + 引用行重挂脏标；`Removed` → 卸载回收账——发现侧第二条入口 |
//! | [`asset_readiness`] | 异步就绪账面：pending 身份账 + 加载失败读侧 + 失败/取消计量，"还没到货"这条时间线 |
//! | [`object_ops`] | 对象写操作封装：spawn 克隆/换柄，BRP 与将来 egui 面板共用（despawn/摘组件走官方 BRP） |
//! | [`asset_ops`] | 资产写操作封装：mutate 材质参数/贴图槽/mesh 顶点/image 像素，BRP 薄壳的可复用实现 |
//! | [`scene_gen`] | 常驻大场景生成器（5.4）：程序化批量铺实例（共享资产/规模参数化/种子复现）+ chunk 身份与整块增删 |
//! | [`camera_aspect`] | 宽高比补位：随窗口 resize 修正任意相机的 aspect_ratio |
//! | [`camera_control`] | 相机轨道控制：左键拖拽环绕 + 滚轮推拉，目标点由组件携带 |
//!
//! 判定线：driver（上传、帧循环）只依赖本层，业务半边（[`crate::scene::content`]）
//! 没有任何引擎侧消费者。

mod asset_events;
mod asset_ops;
mod asset_readiness;
mod camera_aspect;
mod camera_control;
mod ledger;
mod material_hook;
mod object_ops;
pub mod scene_gen;

pub use asset_events::{AshAssetEventPlugin, AssetInvalidations, AssetRetirements, RecycleStats};
pub use asset_readiness::{AshReadinessPlugin, AssetReadiness, ReadinessStats};
pub use asset_ops::{mutate_asset, AssetOpError, MutateAsset};
pub use camera_aspect::AshCameraAspectPlugin;
pub use camera_control::{AshCameraControlPlugin, CameraOrbit};
pub(crate) use camera_control::{apply_camera_command, CameraCommandError, CameraOrbitCommand};
pub use ledger::{AshLedgerPlugin, IncrementStats, InstanceLedger, InstanceRow};
pub use material_hook::AshMaterialHookPlugin;
pub use object_ops::{
    load_image_slot, load_mesh_rebind, replace_handles, spawn_primitive_clone,
    spawn_with_loaded_mesh,
};
pub use scene_gen::{
    chunk_census, despawn_chunk, generate_chunks, generate_instances, ChunkGenParams,
    SceneGenParams,
};
pub(crate) use object_ops::{HandleSwapReadback, ObjectOpError};

// 排序锚点（语义 = 账本已与 World 对账完毕）：业务侧核验
//（scene::content::collect_report）用 `.after(update_ledger)` 挂链。
pub(crate) use ledger::update_ledger;
