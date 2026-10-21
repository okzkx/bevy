//! 稳定槽账本与变更发现：Entity→GPU 实例槽关联的跨帧账本，
//! 增量维护，不做每帧全量快照重建。
//!
//! 三线分工里的枢纽：变更发现的输出落进账本（删除处理的信息来源），增量写入
//! （上传链）的输入来自账本脏行。机制：
//!
//! - **账本**（[`InstanceLedger`]）：在场可渲染 primitive 每实体一行（两柄 +
//!   传播后终值矩阵），BTreeMap 按 Entity 有序迭代（DrawList 顺序确定）。行随
//!   实体生命周期增删换绑，跨帧存活——`RemovedComponents<T>` 不带旧组件内容，
//!   删除处理全靠账本自己存的那行。
//! - **发现**（[`update_ledger`]，PostUpdate 排 [`TransformSystems::Propagate`]
//!   之后）：三条入口各接一类变更——
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
//! 7.9 组装增量账（[`AssemblyCache`]）：DrawList 可绘行集合的增量缓存与账本
//! 同体维护——可绘判定（实例槽 ∧ 锚驻留 ∧ 材质在容器）的全部输入变化都经过
//! 本账本的行增删/换绑/置脏/槽位交接路径，各变更点顺带标记组装重估（不做第二
//! 套变更发现）。行宇宙不变帧的组装零走访零产物重建，缓存视图由
//! [`InstanceLedger::assembly_view`] 供帧循环全程只读。
//!
//! 零 Vulkan 代码：账本只有 ECS 侧事实；GPU 足迹（池区间/贴图槽）按资产身份
//! 去驻留缓存（MeshPool/ImageCache）现查，不进账本行。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bevy::{
    asset::AssetId,
    pbr::{MeshMaterial3d, StandardMaterial},
    prelude::*,
    transform::TransformSystems,
};

use ash_macros::system;

/// 账本一行：一个可渲染 primitive 的跨帧实例数据。柄去资产容器取内容，
/// `model` 直进模型环（帧循环增量重写）；`instance_slot` 是 GPU 对象表下标
/// （flush_uploads 分配，`None` = 尚未驻留成表——对象记录/模型环内容的
/// hazard 纪律见 `vulkan::instance_table` 模块文档）。
#[derive(Clone)]
pub struct InstanceRow {
    pub mesh: Handle<Mesh>,
    pub material: Handle<StandardMaterial>,
    /// [`GlobalTransform`] 终值矩阵（PostUpdate 传播后直读，含父链）。
    pub model: Mat4,
    /// GPU 对象表实例槽（对象表下标）；None = 该行未驻留成表（不参与绘制）。
    pub instance_slot: Option<u32>,
    /// 记录身份：(mesh 身份, mesh 内容票据, 材质身份)——三者不变 = 对象表
    /// 记录内容不变（免重写免换槽）；任一变化 = 换槽重写（swap-on-commit）。
    /// mesh 内容票据取自 MeshSlot.ticket：同身份内容变化（Modified）必然
    /// 换票据，记录随身份比对失效。
    pub bound: Option<(AssetId<Mesh>, u64, AssetId<StandardMaterial>)>,
}

/// DrawList 组装缓存（7.9）：可绘行集合的增量账——行宇宙不变帧零走访。
/// 判定输入全部来自账本自身的行状态变化（增删/换绑/置脏/槽位交接），各变更点
/// 标记重估；全量重建只在首帧播种与换形（锚开关翻转 / LOD 登记换代）发生。
#[derive(Default)]
pub(crate) struct AssemblyCache {
    /// 首帧全量播种旗（重建后复位重估通道照常）。
    seeded: bool,
    /// 外部请求的全量重建（预留全局失效口，当前无写入点——键比对兜底换形）。
    rebuild_all: bool,
    /// 上次组装的锚开关（翻转 = 锚解析换形，全量重建）。
    last_floor_anchor: Option<bool>,
    /// 上次组装的 LOD 登记换代号（登记处 register 换组 = 锚解析变，全量重建）。
    last_lod_generation: u64,
    /// 可绘行 → (实例槽, 材质身份, alpha<1)。BTreeMap 保 Entity 序——与全量
    /// 走访（rows() 迭代序）的产物逐行一致（判定线 2 的对照面）。
    drawable: BTreeMap<Entity, (u32, AssetId<StandardMaterial>, bool)>,
    /// 材质身份 → 可绘引用行数（材质集增量的引用计数；减到 0 出表）。
    material_refs: HashMap<AssetId<StandardMaterial>, usize>,
    /// 待重估行集（各变更点的输入侧，`assembly_update` 消费后清空）。
    stale: BTreeSet<Entity>,
    /// 产物缓存（行宇宙稳定帧零重建，容量复用）。
    slots: Vec<u32>,
    materials: Vec<AssetId<StandardMaterial>>,
    /// alpha<1 的可绘行数（不透明覆盖收账日志用，随可绘行增删增量维护）。
    alpha_override: usize,
    /// 全量重建累计（计量）。
    pub rebuilds_total: u64,
    /// 行级重估累计（计量）。
    pub reevals_total: u64,
    /// 重估标记排水累计（行次；mark 与 drain 的对账读数）。
    pub drained_rows_total: u64,
    /// 最近一次排水消费的标记行数（计量）。
    pub last_drain_rows: u64,
}

/// 组装可绘判定闭包（7.9）：行 → (槽号, 材质身份, alpha<1)；None = 暂缓。
type AssemblyEval<'a> = &'a mut dyn FnMut(&InstanceRow) -> Option<(u32, AssetId<StandardMaterial>, bool)>;

impl AssemblyCache {
    /// 产物重建（可绘集变化后调用）：slots 按 Entity 序展开、materials 按
    /// 材质表展开。变化帧 O(可绘行)，稳定帧不进此路（容量复用）。
    fn rebuild_products(&mut self) {
        self.slots.clear();
        self.slots.extend(self.drawable.values().map(|(slot, _, _)| *slot));
        self.materials.clear();
        self.materials.extend(self.material_refs.keys().copied());
    }
}

/// 材质引用计数递减（0 出表）：可绘行退场时收账。
fn release_material_ref(
    refs: &mut HashMap<AssetId<StandardMaterial>, usize>,
    mat: AssetId<StandardMaterial>,
) {
    if let Some(count) = refs.get_mut(&mat) {
        *count -= 1;
        if *count == 0 {
            refs.remove(&mat);
        }
    }
}

/// 组装产物只读视图（7.9）：draw_frame 在组装维护与奇偶集取走之后创建，
/// 供环写入与诊断读数全程借用——`pending_rows` 由调用方按 `账本行数 −
/// slots.len()` 现算（缓存维护后恒成立）。
pub(crate) struct AssemblyView<'a> {
    /// 可绘行实例槽号列表（Entity 序，与全量走访产物逐行一致）。
    pub slots: &'a [u32],
    /// 可绘行的去重材质身份（材质环全量重写的输入；迭代序不指定）。
    pub materials: &'a [AssetId<StandardMaterial>],
    /// alpha<1 的可绘行数。
    pub alpha_override: usize,
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
    /// 本帧收到的资产事件数（资产事件读侧，按收到计含忽略变体）。
    pub asset_events: usize,
    /// 本帧因资产内容变化重挂脏标的行数（资产失效联动）。
    pub asset_dirty_rows: usize,
    /// 资产事件累计。
    pub asset_events_total: u64,
    /// 资产失效累计（mesh+image 的 Modified 事件累计，只增；勿赋当前集合
    /// 长度——累计与存量混记会互相污染口径）。
    pub asset_invalidations_total: u64,
    /// 当前待换内容身份集规模（存量口径，mesh+image 失效集现值，与累计分列）。
    pub asset_invalidations_current: usize,
    /// 对象表已用实例槽数（存量口径;含停尸房/free list 在途,历史峰值语义）。
    pub object_slots: usize,
    /// 已分配材质槽数（存量口径）。
    pub material_slots: usize,
}

/// 稳定槽账本：在场可渲染 primitive 的 Entity→实例数据关联（跨帧存活），
/// 加"待上传盘点"脏集与增量计量。GPU 足迹不进行：按资产身份去
/// [`crate::vulkan::MeshPool`]/[`crate::vulkan::ImageCache`] 现查。
#[derive(Resource, Default)]
pub struct InstanceLedger {
    rows: BTreeMap<Entity, InstanceRow>,
    /// 新增/换柄置位，flush_uploads 盘点（驻留或登记重试）后清。
    dirty: BTreeSet<Entity>,
    /// 行删除/换绑换下的实例槽（纯 ECS 侧事实的整数账）：flush_uploads
    /// 消费进对象表停尸房（排空后才复用）。update_ledger 不能触 Vulkan，
    /// 以此账交接——桥接纪律与停尸房同款（旧内容多活一帧，无悬空）。
    pub(crate) freed_slots: Vec<u32>,
    /// 模型环待写标记，按帧槽奇偶两套：改动行的 model 要写进**两个帧槽**
    /// 各自的环（draw_frame 只消费本帧奇偶的那套，另一套下帧消费）——
    /// 每个改动行两次写入后两个环都新鲜，静态场景零写入。
    model_dirty: [BTreeSet<Entity>; 2],
    /// LOD 变体表待写标记（6.2），按帧槽奇偶两套：行换槽/LOD 组档位资产
    /// 到货时置位，draw_frame 消费本帧奇偶集（写该槽的 LOD 表条目进本帧
    /// 槽环）——纪律与 model_dirty 相同（两帧后两个环都新鲜）。
    lod_dirty: [BTreeSet<Entity>; 2],
    /// DrawList 组装缓存（7.9）：可绘行集合增量账，见 [`AssemblyCache`]。
    assembly: AssemblyCache,
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

    /// 重挂一行脏标（资产失效联动：资产内容变化波及的引用者重新进盘点；
    /// 发现侧第二条入口，与 update_ledger 的 Changed/Removed 通道并轨同一脏集）。
    /// 组装联动（7.9）：置脏行的可绘输入可能随重传翻面，一并标记重估。
    pub(crate) fn mark_dirty(&mut self, entity: Entity) {
        self.dirty.insert(entity);
        self.assembly.stale.insert(entity);
    }

    /// 落一行实例槽与记录身份（flush_uploads 换槽时调用），返回旧槽
    /// （调用方送停尸房）。行已清 = None（脏标清空路径不会调到这里）。
    pub(crate) fn set_instance_slot(
        &mut self,
        entity: Entity,
        slot: u32,
        bound: (AssetId<Mesh>, u64, AssetId<StandardMaterial>),
    ) -> Option<u32> {
        let old = self
            .rows
            .get_mut(&entity)
            .and_then(|row| row.instance_slot.replace(slot));
        if let Some(row) = self.rows.get_mut(&entity) {
            row.bound = Some(bound);
        }
        // 槽位交接 = 组装可绘判定输入变化（7.9）
        self.mark_assembly_stale(entity);
        old
    }

    /// 取走全部待交接实例槽（flush 开头消费，账随取随清）。
    pub(crate) fn take_freed_slots(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.freed_slots)
    }

    /// 标记一行模型待写：进**两套**奇偶集（两个帧槽的环都要补写）。
    pub(crate) fn mark_model_dirty(&mut self, entity: Entity) {
        self.model_dirty[0].insert(entity);
        self.model_dirty[1].insert(entity);
    }

    /// 取走一套奇偶集的待写实体（draw_frame 按本帧奇偶消费，取走即清）。
    pub(crate) fn take_model_dirty(&mut self, frame: usize) -> Vec<Entity> {
        let set = &mut self.model_dirty[frame % 2];
        let out: Vec<Entity> = set.iter().copied().collect();
        set.clear();
        out
    }

    /// 标记一行 LOD 变体表待写（6.2）：进**两套**奇偶集（两个帧槽的环都要
    /// 补写——模型环同款纪律）。触发点 = 行换槽（flush_uploads）与 LOD 组
    /// 档位资产到货（同处）。
    pub(crate) fn mark_lod_dirty(&mut self, entity: Entity) {
        self.lod_dirty[0].insert(entity);
        self.lod_dirty[1].insert(entity);
    }

    /// 取走一套奇偶集的 LOD 待写实体（draw_frame 按本帧奇偶消费，取走即清）。
    pub(crate) fn take_lod_dirty(&mut self, frame: usize) -> Vec<Entity> {
        let set = &mut self.lod_dirty[frame % 2];
        let out: Vec<Entity> = set.iter().copied().collect();
        set.clear();
        out
    }

    /// 全部引用给定 mesh 身份的行（6.2：LOD 组档位资产到货时反查引用行
    /// 重挂 LOD 脏标；O(rows) 走访，触发频率 = 档位资产提交，稀疏事件）。
    pub(crate) fn rows_with_mesh(&self, id: AssetId<Mesh>) -> Vec<Entity> {
        self.rows
            .iter()
            .filter(|(_, row)| row.mesh.id() == id)
            .map(|(entity, _)| *entity)
            .collect()
    }

    /// 全部引用给定材质身份的行（7.9：材质资产离容器时反查引用行重估组装；
    /// O(rows) 走访，触发频率 = 资产移除，稀疏事件）。
    pub(crate) fn rows_with_material(&self, id: AssetId<StandardMaterial>) -> Vec<Entity> {
        self.rows
            .iter()
            .filter(|(_, row)| row.material.id() == id)
            .map(|(entity, _)| *entity)
            .collect()
    }

    /// 标记一行待组装重估（7.9 变更点钩子：行增删/换绑、`mark_dirty`、
    /// 槽位交接、驻留出账反查各调用点；幂等——重复标记零成本）。
    pub(crate) fn mark_assembly_stale(&mut self, entity: Entity) {
        self.assembly.stale.insert(entity);
    }

    /// DrawList 组装增量维护（7.9）：消费重估集，按 `eval`（可绘判定闭包，
    /// 由帧循环注入 pool/材质容器/LOD 组的读取）维护可绘行缓存。全量重建
    /// 仅在首帧播种或换形（锚开关翻转 / LOD 登记换代）发生；其余帧只重估
    /// 重估集——行宇宙不变帧零走访、产物零重建（容量复用）。
    pub(crate) fn assembly_update(
        &mut self,
        eval: AssemblyEval<'_>,
        floor_anchor: bool,
        lod_generation: u64,
    ) {
        let rebuild = !self.assembly.seeded
            || self.assembly.rebuild_all
            || self.assembly.last_floor_anchor != Some(floor_anchor)
            || self.assembly.last_lod_generation != lod_generation;
        if rebuild {
            self.assembly.drawable.clear();
            self.assembly.material_refs.clear();
            self.assembly.stale.clear();
            self.assembly.alpha_override = 0;
            for (entity, row) in &self.rows {
                if let Some((slot, mat, alpha)) = eval(row) {
                    *self.assembly.material_refs.entry(mat).or_insert(0) += 1;
                    if alpha {
                        self.assembly.alpha_override += 1;
                    }
                    self.assembly.drawable.insert(*entity, (slot, mat, alpha));
                }
            }
            self.assembly.seeded = true;
            self.assembly.rebuild_all = false;
            self.assembly.last_floor_anchor = Some(floor_anchor);
            self.assembly.last_lod_generation = lod_generation;
            self.assembly.rebuilds_total += 1;
            self.assembly.rebuild_products();
            return;
        }
        let stale: Vec<Entity> = self.assembly.stale.iter().copied().collect();
        self.assembly.stale.clear();
        self.assembly.drained_rows_total += stale.len() as u64;
        self.assembly.last_drain_rows = stale.len() as u64;
        let mut changed = false;
        for entity in stale {
            self.assembly.reevals_total += 1;
            // 旧可绘态先退场（引用计数/alpha 收账）；行已清 = 只退场不重估
            if let Some((_, mat, alpha)) = self.assembly.drawable.remove(&entity) {
                changed = true;
                release_material_ref(&mut self.assembly.material_refs, mat);
                if alpha {
                    self.assembly.alpha_override -= 1;
                }
            }
            let Some(row) = self.rows.get(&entity) else {
                continue;
            };
            if let Some((slot, mat, alpha)) = eval(row) {
                *self.assembly.material_refs.entry(mat).or_insert(0) += 1;
                if alpha {
                    self.assembly.alpha_override += 1;
                }
                self.assembly.drawable.insert(entity, (slot, mat, alpha));
                changed = true;
            }
            // 仍暂缓：可绘集无此行（pending = 行总数 − 可绘数，现算）
        }
        if changed {
            self.assembly.rebuild_products();
        }
    }

    /// 组装产物只读视图（draw_frame 组装段之后全程借用；与 `&mut` 消费点
    /// （奇偶集取走）在取视图之前收口，借用纪律见 draw_frame）。
    #[must_use]
    pub(crate) fn assembly_view(&self) -> AssemblyView<'_> {
        AssemblyView {
            slots: &self.assembly.slots,
            materials: &self.assembly.materials,
            alpha_override: self.assembly.alpha_override,
        }
    }

    /// 组装缓存只读访问（BRP 读数面：重建/重估计量）。
    #[must_use]
    pub(crate) fn assembly(&self) -> &AssemblyCache {
        &self.assembly
    }

    /// 暂缓行数（行总数 − 可绘数；缓存维护后恒与全量走访的暂缓口径一致）。
    #[must_use]
    pub(crate) fn assembly_pending(&self) -> usize {
        self.rows.len() - self.assembly.drawable.len()
    }
}

/// 账本插件：初始化 [`InstanceLedger`]，把 [`update_ledger`] 挂 PostUpdate
/// （传播之后，读到的才是含父链的本帧终值）。接线收在本插件内，main 只
/// `add_plugins`。
pub struct AshLedgerPlugin;

impl Plugin for AshLedgerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InstanceLedger>()
            // LOD 组登记处（6.2.2）：flush_uploads 以 Res 读它（档位资产盘点
            // 与到货重挂），恒在账——随账本插件初始化
            .init_resource::<super::LodGroups>()
            .add_systems(
            PostUpdate,
            update_ledger.after(TransformSystems::Propagate),
        );
    }
}

/// 新增/换柄入口的查询（type alias 免 clippy type_complexity）：
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
    // 处理，同帧"摘了又挂回"的实体由 ② 重建行，终态正确。删行携带的实例槽
    // 交接给 flush（→ 对象表停尸房）。
    for entity in removed_meshes.read().chain(removed_materials.read()) {
        if let Some(row) = ledger.rows.remove(&entity) {
            if let Some(slot) = row.instance_slot {
                ledger.freed_slots.push(slot);
            }
            removed += 1;
            processed += 1;
            // 组装联动（7.9）：行退场，重估把可绘态撤出缓存（含材质引用计数）
            ledger.mark_assembly_stale(entity);
            info!(
                "账本清行 {entity:?}（Mesh3d/材质被摘或 despawn，剩 {} 行）",
                ledger.rows.len()
            );
        }
        ledger.dirty.remove(&entity);
    }

    // —— ② 新增/换绑：行不存在 = 入账，柄变化 = 换绑；两处都置脏等上传盘点。
    // 行的实例槽/记录身份由 flush_uploads 分配（None = 未驻留成表）。
    for (entity, mesh3d, material3d, global) in &assets {
        let new_row = InstanceRow {
            mesh: mesh3d.0.clone(),
            material: material3d.0.clone(),
            model: global.to_matrix(),
            instance_slot: None,
            bound: None,
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
                // 旧实例槽随换绑交接（flush 换新槽 + 旧槽进停尸房）
                if let Some(slot) = old.instance_slot {
                    ledger.freed_slots.push(slot);
                }
            }
            // 同值重复变更（重插同柄）：行重写、脏重置，不计处理数
            Some(_) => {}
        }
        ledger.dirty.insert(entity);
        ledger.rows.insert(entity, new_row);
        // 组装联动（7.9）：入行/换绑都改可绘判定输入（槽位/柄），标记重估
        ledger.mark_assembly_stale(entity);
    }

    // —— ③ 矩阵：只写 model，不进脏集（模型走模型环，不过上传链）；
    // 待写标记进两套奇偶集（两个帧槽的环都要补写）。
    for (entity, global) in &moved {
        if let Some(row) = ledger.rows.get_mut(&entity) {
            let model = global.to_matrix();
            if row.model != model {
                row.model = model;
                ledger.mark_model_dirty(entity);
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

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::math::Mat4;

    /// 合成行：柄按真实容器生成（身份唯一、无渲染依赖），可绘判定由测试
    /// eval 闭包以 instance_slot 直接决定——缓存语义与真实判定解耦。
    struct Fixture {
        world: World,
        meshes: Assets<Mesh>,
        materials: Assets<StandardMaterial>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                world: World::default(),
                meshes: Assets::<Mesh>::default(),
                materials: Assets::<StandardMaterial>::default(),
            }
        }

        /// n 个单调递增的测试实体（升序保证 = Entity 分配序）。
        fn entities(&mut self, n: usize) -> Vec<Entity> {
            (0..n).map(|_| self.world.spawn_empty().id()).collect()
        }

        fn row(&mut self, mat: Handle<StandardMaterial>, slot: Option<u32>) -> InstanceRow {
            InstanceRow {
                mesh: self.meshes.add(Mesh::from(Sphere::new(0.5))),
                material: mat,
                model: Mat4::IDENTITY,
                instance_slot: slot,
                bound: None,
            }
        }
    }

    /// 测试 eval：槽位在 = 可绘（材质身份取自行柄，alpha 恒 false）。
    fn eval(row: &InstanceRow) -> Option<(u32, AssetId<StandardMaterial>, bool)> {
        row.instance_slot.map(|s| (s, row.material.id(), false))
    }

    fn insert_row(ledger: &mut InstanceLedger, entity: Entity, row: InstanceRow) {
        ledger.rows.insert(entity, row);
        ledger.mark_assembly_stale(entity);
    }

    #[test]
    fn assembly_seed_order_and_pending() {
        let mut fx = Fixture::new();
        let mat = fx.materials.add(StandardMaterial::default());
        let [e1, e2, e3] = {
            let mut es = fx.entities(3);
            [es.remove(0), es.remove(0), es.remove(0)]
        };
        let mut ledger = InstanceLedger::default();
        insert_row(&mut ledger, e1, fx.row(mat.clone(), Some(10)));
        insert_row(&mut ledger, e2, fx.row(mat.clone(), Some(20)));
        // 一行缺槽（暂缓）
        insert_row(&mut ledger, e3, fx.row(mat, None));
        ledger.assembly_update(&mut eval, false, 0);
        let view = ledger.assembly_view();
        // 槽号序 = 账本行序（同一个 BTreeMap 的迭代序）——与全量走访逐行一致
        let expected: Vec<u32> = ledger.rows().filter_map(|(_, r)| r.instance_slot).collect();
        assert_eq!(view.slots, expected.as_slice(), "可绘槽与账本行序一致");
        assert_eq!(ledger.len() - view.slots.len(), 1, "缺槽行 = 暂缓 1 行");
        assert_eq!(ledger.assembly().rebuilds_total, 1, "首帧全量播种");
    }

    #[test]
    fn assembly_stale_transitions() {
        let mut fx = Fixture::new();
        let mat = fx.materials.add(StandardMaterial::default());
        let e = fx.entities(1).remove(0);
        let mesh_id = fx.meshes.iter().next().map(|(id, _)| id);
        let mut ledger = InstanceLedger::default();
        insert_row(&mut ledger, e, fx.row(mat.clone(), None));
        ledger.assembly_update(&mut eval, false, 0);
        assert!(ledger.assembly_view().slots.is_empty(), "缺槽 = 不可绘");
        // 槽位到账 → 重估后进入可绘集（bound 的 mesh 身份本测试不消费，占位即可）
        ledger
            .set_instance_slot(e, 7, (mesh_id.unwrap_or_default(), 0, mat.id()));
        ledger.assembly_update(&mut eval, false, 0);
        assert_eq!(ledger.assembly_view().slots, &[7], "槽位到账进入可绘集");
        // 无变化帧：重估集为空 = 零重估，产物保持
        let reevals_before = ledger.assembly().reevals_total;
        ledger.assembly_update(&mut eval, false, 0);
        assert_eq!(ledger.assembly().reevals_total, reevals_before, "稳定帧零重估");
        assert_eq!(ledger.assembly_view().slots, &[7]);
    }

    #[test]
    fn assembly_material_refcount() {
        let mut fx = Fixture::new();
        let mat = fx.materials.add(StandardMaterial::default());
        let other = fx.materials.add(StandardMaterial::default());
        let [e1, e2, e3] = {
            let mut es = fx.entities(3);
            [es.remove(0), es.remove(0), es.remove(0)]
        };
        let mut ledger = InstanceLedger::default();
        // e1/e2 共享同一材质（引用计数 2），e3 独占 another
        insert_row(&mut ledger, e1, fx.row(mat.clone(), Some(1)));
        insert_row(&mut ledger, e2, fx.row(mat.clone(), Some(2)));
        insert_row(&mut ledger, e3, fx.row(other, Some(3)));
        ledger.assembly_update(&mut eval, false, 0);
        assert_eq!(ledger.assembly_view().materials.len(), 2);
        // 一行退场：共享材质仍留表（另一行还引用）
        ledger.rows.remove(&e1);
        ledger.mark_assembly_stale(e1);
        ledger.assembly_update(&mut eval, false, 0);
        assert_eq!(ledger.assembly_view().materials.len(), 2, "共享材质随剩余行保留");
        let expected: Vec<u32> = ledger.rows().filter_map(|(_, r)| r.instance_slot).collect();
        assert_eq!(ledger.assembly_view().slots, expected, "退场行撤出可绘集（序 = 账本行序）");
        // 共享材质的最后一行退场：该材质出表，独占材质保留
        ledger.rows.remove(&e2);
        ledger.mark_assembly_stale(e2);
        ledger.assembly_update(&mut eval, false, 0);
        assert_eq!(ledger.assembly_view().materials.len(), 1, "引用清零的材质出表");
        let expected: Vec<u32> = ledger.rows().filter_map(|(_, r)| r.instance_slot).collect();
        assert_eq!(ledger.assembly_view().slots, expected, "剩余可绘行与账本行序一致");
        assert_eq!(ledger.len() - ledger.assembly_view().slots.len(), 0, "可绘行零暂缓");
    }

    #[test]
    fn assembly_rebuild_triggers() {
        let mut fx = Fixture::new();
        let mat = fx.materials.add(StandardMaterial::default());
        let e = fx.entities(1).remove(0);
        let mut ledger = InstanceLedger::default();
        insert_row(&mut ledger, e, fx.row(mat, Some(5)));
        ledger.assembly_update(&mut eval, false, 0);
        let rebuilds = ledger.assembly().rebuilds_total;
        // 锚开关翻转 = 换形重建
        ledger.assembly_update(&mut eval, true, 0);
        assert_eq!(ledger.assembly().rebuilds_total, rebuilds + 1, "锚开关翻转触发全量重建");
        // 同参数稳定帧不重建
        ledger.assembly_update(&mut eval, true, 0);
        assert_eq!(ledger.assembly().rebuilds_total, rebuilds + 1);
        // LOD 登记换代 = 锚解析变，重建
        ledger.assembly_update(&mut eval, true, 1);
        assert_eq!(ledger.assembly().rebuilds_total, rebuilds + 2);
    }

    #[test]
    fn assembly_alpha_counter() {
        let mut fx = Fixture::new();
        let mat = fx.materials.add(StandardMaterial::default());
        let e = fx.entities(1).remove(0);
        let mut ledger = InstanceLedger::default();
        insert_row(&mut ledger, e, fx.row(mat, Some(3)));
        let mut alpha_eval = |row: &InstanceRow| row.instance_slot.map(|s| (s, row.material.id(), true));
        ledger.assembly_update(&mut alpha_eval, false, 0);
        assert_eq!(ledger.assembly_view().alpha_override, 1);
        ledger.rows.remove(&e);
        ledger.mark_assembly_stale(e);
        ledger.assembly_update(&mut alpha_eval, false, 0);
        assert_eq!(ledger.assembly_view().alpha_override, 0, "退场行 alpha 收账");
    }
}
