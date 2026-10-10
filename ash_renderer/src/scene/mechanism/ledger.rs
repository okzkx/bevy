//! 稳定槽账本与变更发现（4.1.2/4.1.3）：Entity→GPU 实例槽关联的跨帧账本，
//! 取代 M2 的每帧全量快照重建（[`crate::scene::mechanism::collect`] 退役）。
//!
//! 三线分工里的枢纽：变更发现的输出落进账本（删除处理的信息来源），增量写入
//! （上传链）的输入来自账本脏行。机制：
//!
//! - **账本**（[`InstanceLedger`]）：在场可渲染 primitive 每实体一行（两柄 +
//!   传播后终值矩阵），BTreeMap 按 Entity 有序迭代（DrawList 顺序确定）。行随
//!   实体生命周期增删换绑，跨帧存活——`RemovedComponents<T>` 不带旧组件内容，
//!   删除处理全靠账本自己存的那行（路线图步骤 4 边界）。
//! - **发现**（[`update_ledger`]，PostUpdate 排 [`TransformSystems::Propagate`]
//!   之后，同 3.1.4 的采集时机）：三条入口各接一类变更——
//!   ① 移除：`RemovedComponents<Mesh3d>` / `RemovedComponents<MeshMaterial3d>`
//!   清行。despawn 递归删子树走组件移除路径（bundle/remove.rs 写
//!   `removed_components`），每个 primitive 各发一次，**整树清账免费拿到**；
//!   ② 新增/换绑：`Or<(Changed<Mesh3d>, Changed<MeshMaterial3d>)>`（插入也置
//!   changed tick，故新增不需要 Added）；③ 矩阵：`Changed<GlobalTransform>`——
//!   传播用 `set_if_neq`（bevy_transform/src/systems.rs:719），静态场景恒不触发。
//!
//! 计量（判定线证据口径）：**扫描数**（tick 比较的候选实体数 = 三样俱全实体
//! 总数，O(匹配实体) 是 Changed 语义的已知成本）与**处理数**（真实动手的行）
//! 分开报——不把"零上传"说成"零 CPU 每对象成本"。
//!
//! 零 Vulkan 代码：账本只有 ECS 侧事实；GPU 足迹（池区间/贴图槽）按资产身份
//! 去驻留缓存（MeshPool/ImageCache）现查，不进账本行。

use std::collections::{BTreeMap, BTreeSet};

use bevy::{
    pbr::{MeshMaterial3d, StandardMaterial},
    prelude::*,
    transform::TransformSystems,
};

use ash_macros::system;

/// 账本一行：一个可渲染 primitive 的跨帧实例数据。柄去资产容器取内容，
/// `model` 直进 push constant（与 3.1.4 CollectedPrimitive 同形，只是寿命
/// 从"每帧重建"变为"跨帧维护"）。
#[derive(Clone)]
pub struct InstanceRow {
    pub mesh: Handle<Mesh>,
    pub material: Handle<StandardMaterial>,
    /// [`GlobalTransform`] 终值矩阵（PostUpdate 传播后直读，含父链）。
    pub model: Mat4,
}

/// 增量计量：发现侧逐帧刷新，上传侧累计（BRP `increment_stats` 的读数源）。
#[derive(Clone, Copy, Debug, Default)]
pub struct IncrementStats {
    /// 扫描数：tick 比较的候选实体数（三样俱全实体总数，逐帧）。
    pub scan_candidates: usize,
    /// 本帧新入账行数。
    pub added_rows: usize,
    /// 本帧清账行数（含 despawn 连带）。
    pub removed_rows: usize,
    /// 本帧换柄重绑行数。
    pub rebound_rows: usize,
    /// 本帧矩阵更新行数。
    pub transform_updates: usize,
    /// 本帧真实动手的总行数（增+清+换绑+矩阵，判定线"处理数"）。
    pub processed_rows: usize,
    /// 入账行累计（BRP 读数的增量对账用：操作前后差值 = 该操作入行数）。
    pub added_rows_total: u64,
    /// 清账行累计（despawn/摘组件证据）。
    pub removed_rows_total: u64,
    /// 换绑行累计。
    pub rebound_rows_total: u64,
    /// 矩阵更新行累计（移动判定线：拖一次只 +1）。
    pub transform_updates_total: u64,
    /// flush 后仍在待盘点集的行数（资产未到货/被拒的重试队列）。
    pub dirty_rows: usize,
    /// 上传批次累计（个）。
    pub upload_batches_total: u64,
    /// 上传字节累计（staging 实发）。
    pub upload_bytes_total: u64,
    /// 最近一批字节。
    pub last_batch_bytes: u64,
}

/// 稳定槽账本：在场可渲染 primitive 的 Entity→实例数据关联（跨帧存活），
/// 加"待上传盘点"脏集与增量计量。GPU 足迹不进行：按资产身份去
/// [`crate::vulkan::MeshPool`]/[`crate::vulkan::ImageCache`] 现查。
#[derive(Resource, Default)]
pub struct InstanceLedger {
    rows: BTreeMap<Entity, InstanceRow>,
    /// 新增/换柄置位，flush_uploads 盘点（驻留或登记重试）后清。
    dirty: BTreeSet<Entity>,
    pub stats: IncrementStats,
}

impl InstanceLedger {
    /// 行数（在场可渲染 primitive 数）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// 账本是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 一行的只读访问。
    #[must_use]
    pub fn row(&self, entity: Entity) -> Option<&InstanceRow> {
        self.rows.get(&entity)
    }

    /// 全部行，按 Entity 有序（DrawList 组装的迭代序）。
    pub fn rows(&self) -> impl Iterator<Item = (Entity, &InstanceRow)> {
        self.rows.iter().map(|(e, r)| (*e, r))
    }

    /// 待上传盘点的行（发现的输出、上传链的输入）。
    pub fn dirty_entities(&self) -> impl Iterator<Item = Entity> + '_ {
        self.dirty.iter().copied()
    }

    /// 待盘点行数（验收统计口）。
    #[must_use]
    pub fn dirty_count(&self) -> usize {
        self.dirty.len()
    }

    /// 清一行脏标（flush_uploads 盘点完调用）。
    pub(crate) fn clear_dirty(&mut self, entity: Entity) {
        self.dirty.remove(&entity);
    }
}

/// 账本插件：初始化 [`InstanceLedger`]，把 [`update_ledger`] 挂 PostUpdate
/// （传播之后，读到的才是含父链的本帧终值）。接线收在本插件内，main 只
/// `add_plugins`。
pub struct AshLedgerPlugin;

impl Plugin for AshLedgerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InstanceLedger>().add_systems(
            PostUpdate,
            update_ledger.after(TransformSystems::Propagate),
        );
    }
}

/// 新增/换柄入口的查询（type alias 免 clippy type_complexity，3.10 惯例）：
/// 插入也置 changed tick，故新增不需要 Added。
type ChangedAssets<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Mesh3d,
        &'static MeshMaterial3d<StandardMaterial>,
        &'static GlobalTransform,
    ),
    Or<(Changed<Mesh3d>, Changed<MeshMaterial3d<StandardMaterial>>)>,
>;

/// 矩阵入口的查询：Changed<GlobalTransform> + 三样俱全过滤（只更新已入账行）。
type MovedTransforms<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static GlobalTransform),
    (
        Changed<GlobalTransform>,
        With<Mesh3d>,
        With<MeshMaterial3d<StandardMaterial>>,
    ),
>;

/// 变更发现（每帧）：三条入口接账本。`pub(crate)` 给业务侧核验
/// （[`crate::scene::content::collect_report`]）当排序锚点用，语义 =
/// "账本已与 World 对账完毕"。
#[system]
pub(crate) fn update_ledger(
    mut ledger: ResMut<InstanceLedger>,
    mut removed_meshes: RemovedComponents<Mesh3d>,
    mut removed_materials: RemovedComponents<MeshMaterial3d<StandardMaterial>>,
    // 新增/换柄入口：插入也置 changed tick，故不需要 Added。
    assets: ChangedAssets,
    // 矩阵入口：只写行的 model（传播后才跑，终值直读）。
    moved: MovedTransforms,
    // 扫描数口径：Changed 过滤之前 tick 比较的候选全集（三样俱全实体总数）。
    candidates: Query<(), (With<Mesh3d>, With<MeshMaterial3d<StandardMaterial>>)>,
) {
    // 计量先落局部变量（stats 经 ResMut 的 DerefMut 借用会锁整个账本，
    // 不能与 rows/dirty 的修改并存），循环收口后一次性写回。
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut rebound = 0usize;
    let mut transform_updates = 0usize;
    let mut processed = 0usize;

    // —— ① 移除：组件被摘（含 despawn 递归连带）即退出可渲染集。先于新增
    // 处理，同帧"摘了又挂回"的实体由 ② 重建行，终态正确。
    for entity in removed_meshes.read().chain(removed_materials.read()) {
        if ledger.rows.remove(&entity).is_some() {
            removed += 1;
            processed += 1;
            info!(
                "账本清行 {entity:?}（Mesh3d/材质被摘或 despawn，剩 {} 行）",
                ledger.rows.len()
            );
        }
        ledger.dirty.remove(&entity);
    }

    // —— ② 新增/换绑：行不存在 = 入账，柄变化 = 换绑；两处都置脏等上传盘点。
    for (entity, mesh3d, material3d, global) in &assets {
        let new_row = InstanceRow {
            mesh: mesh3d.0.clone(),
            material: material3d.0.clone(),
            model: global.to_matrix(),
        };
        match ledger.rows.get(&entity) {
            None => {
                added += 1;
                processed += 1;
                info!(
                    "账本入行 {entity:?}（mesh {}，{} 行）",
                    new_row.mesh.id(),
                    ledger.rows.len() + 1
                );
            }
            Some(old) if old.mesh != new_row.mesh || old.material != new_row.material => {
                rebound += 1;
                processed += 1;
                info!(
                    "账本换绑 {entity:?}（mesh {} → {}）",
                    old.mesh.id(),
                    new_row.mesh.id()
                );
            }
            // 同值重复变更（重插同柄）：行重写、脏重置，不计处理数
            Some(_) => {}
        }
        ledger.dirty.insert(entity);
        ledger.rows.insert(entity, new_row);
    }

    // —— ③ 矩阵：只写 model，不进脏集（矩阵走 push constant，不过上传链）。
    for (entity, global) in &moved {
        if let Some(row) = ledger.rows.get_mut(&entity) {
            let model = global.to_matrix();
            if row.model != model {
                row.model = model;
                transform_updates += 1;
                processed += 1;
            }
        }
    }
    let dirty_len = ledger.dirty.len();
    let stats = &mut ledger.stats;
    stats.scan_candidates = candidates.iter().count();
    stats.added_rows = added;
    stats.removed_rows = removed;
    stats.rebound_rows = rebound;
    stats.transform_updates = transform_updates;
    stats.processed_rows = processed;
    stats.added_rows_total += added as u64;
    stats.removed_rows_total += removed as u64;
    stats.rebound_rows_total += rebound as u64;
    stats.transform_updates_total += transform_updates as u64;
    stats.dirty_rows = dirty_len;
}
