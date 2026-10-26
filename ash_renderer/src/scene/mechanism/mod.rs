//! 场景侧机制层：禁渲染补位与取数机制——换任何场景内容都不变的那半边。
//! 零 Vulkan 代码，也不含写死的场景参数。
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`material_hook`] | 材质缝补位：补 `Assets<StandardMaterial>` 容器 + 自写三钩子 |
//! | [`ledger`] | 稳定槽账本：Entity→实例数据跨帧账本 + Changed/Removed 增量变更发现 |
//! | [`assembly_cache`] | DrawList 组装增量账：可绘行集合缓存，随账本变更点标记重估 |
//! | [`drawlist_verify`] | DrawList 组装对照账面：增量 vs 全量参照的比对开关与计量（BRP 开关） |
//! | [`residency`] | 驻留管理器：显存预算账 + 相机驱动的资产请求/淘汰（LOD 档位/贴图/GPU 缺口） |
//! | [`asset_events`] | 资产事件读侧：`Modified` → 失效凭据 + 引用行重挂脏标；`Removed` → 卸载回收账——发现侧第二条入口 |
//! | [`asset_readiness`] | 异步就绪账面：pending 身份账 + 加载失败读侧 + 失败/取消计量，"还没到货"这条时间线 |
//! | [`object_ops`] | 对象写操作封装：spawn 克隆/换柄，BRP 等写入方共用（despawn/摘组件走官方 BRP） |
//! | [`asset_ops`] | 资产写操作封装：mutate 材质参数/贴图槽/mesh 顶点/image 像素，BRP 薄壳的可复用实现 |
//! | [`transform_edit`] | Transform 编辑封装：egui 拖值排队清账与 BRP `set_transform` 直调共用的唯一实现 |
//! | [`scene_tree`] | 场景层级树索引：相关性过滤 + 节点命名 + 根定位，egui 树页与 BRP `scene_tree` 共用 |
//! | [`scene_gen`] | 常驻大场景生成器：程序化批量铺实例（共享资产/规模参数化/种子复现）+ chunk 身份与整块增删 |
//! | [`test_entities`] | 测试实体方法族：五形状 + 颜色 → Lambert 材质的单个测试体（遮挡对/墙体样本载具），mesh 按形状/材质按颜色缓存 |
//! | [`lod_registry`] | LOD 组登记处：逻辑 mesh → 各档变体柄的 CPU 侧映射账（flush 盘点/draw_frame 表写/census 读数共用），LOD 阶梯 spawn 本体 |
//! | [`camera_aspect`] | 宽高比补位：随窗口 resize 修正任意相机的 aspect_ratio |
//! | [`camera_control`] | 相机轨道控制：左键拖拽环绕 + 滚轮推拉，目标点由组件携带 |
//! | [`light_control`] | 日光方向控制：中键拖拽旋转太阳方位/仰角，方向光实体携带球坐标状态 |
//! | [`pointer_capture`] | 输入仲裁窄接口：调试 UI 发布"指针在 UI 上/键盘被占用"，相机与日光控制只读 |
//! | [`visibility_exempt`] | 可见性集群豁免：移除场景实体可见性三件套，bevy PostUpdate 可见性集群整体失明（零消费者治理） |
//! | [`workload`] | 变更形态注入：BRP 开关控制的每帧旋转工作源（压测矩阵的少量变化/全动态形态） |
//!
//! 判定线：driver（上传、帧循环）、overlay、remote 只依赖本层，业务半边
//!（[`crate::scene::content`]）没有任何引擎侧消费者；本层只依赖 render 与
//! common。

mod assembly_cache;
mod asset_events;
mod asset_ops;
mod asset_readiness;
mod camera_aspect;
mod camera_guard;
mod camera_control;
mod drawlist_verify;
mod ledger;
mod light_control;
mod lod_registry;
mod residency;
mod material_hook;
mod object_ops;
mod pointer_capture;
pub mod scene_gen;
mod scene_tree;
pub mod test_entities;
mod transform_edit;
mod visibility_exempt;
mod workload;

pub use asset_events::{AshAssetEventPlugin, AssetInvalidations, AssetRetirements, RecycleStats};
pub use asset_readiness::{AshReadinessPlugin, AssetReadiness, ReadinessStats};
pub use asset_ops::{mutate_asset, AssetOpError, MutateAsset};
pub use camera_aspect::AshCameraAspectPlugin;
pub use camera_guard::AshCameraGuardPlugin;
pub use camera_control::{AshCameraControlPlugin, CameraOrbit};
pub(crate) use camera_control::{apply_camera_command, CameraCommandError, CameraOrbitCommand};
pub use light_control::{apply_sun_rotation, clamp_elevation, AshLightControlPlugin, SunOrbit};
pub use ledger::{
    AshLedgerPlugin, IncrementStats, InstanceLedger, InstanceRow,
};
pub(crate) use ledger::AssemblyView;
pub use lod_registry::{spawn_lod_ladder, LodGroups, LodLadderReadback};
pub use residency::{
    gap_asset_ids, anchor_level, AshResidencyPlugin, GapMode, GapSnapshot, LodDemand,
    ResidencySettings, ResidencyStats,
};
pub use visibility_exempt::AshVisibilityExemptPlugin;
pub use workload::{AshWorkloadPlugin, WorkloadSettings};
pub use material_hook::AshMaterialHookPlugin;
pub use object_ops::{
    load_image_slot, load_mesh_rebind, replace_handles, spawn_primitive_clone,
    spawn_with_loaded_mesh,
};
pub use scene_gen::{
    chunk_census, despawn_chunk, generate_chunks, generate_instances, ChunkGenParams,
    SceneGenParams,
};
pub use test_entities::{spawn_test_shape, TestShape};
pub(crate) use object_ops::{HandleSwapReadback, ObjectOpError};
pub use drawlist_verify::DrawListVerify;
pub use pointer_capture::{PointerCapture, PointerCaptureSet};
pub use residency::ResidencySet;
pub(crate) use scene_tree::{EntityRow, SceneTreeIndex, SceneTreeNode, TREE_MAX_DEPTH};
pub(crate) use transform_edit::{
    apply_transform_edit, apply_transform_edits, TransformEdit, TransformEditError,
    TransformEditQueue,
};

// 排序锚点（语义 = 账本已与 World 对账完毕）：业务侧核验
//（scene::content::collect_report）用 `.after(update_ledger)` 挂链。
pub(crate) use ledger::update_ledger;
