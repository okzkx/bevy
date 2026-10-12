//! 资产事件读侧与失效联动（4.2.1/4.2.2）+ 卸载回收读侧（4.4.1）：发现侧的
//! 第二条入口——对象粒度的五类变更走 [`update_ledger`] 的 Changed/Removed
//! 通道，资产粒度的内容变化与卸载走本模块的 `AssetEvent` 通道。
//!
//! 机制（见段讲解篇《AssetId变更语义》）：当前上传链"按身份查驻留、查无才传"
//! 对同 `AssetId` 内容变化天然失明（驻留命中即跳过）。本模块把
//! `AssetEvent::Modified` 翻译成两件事：
//!
//! - **失效凭据**（[`AssetInvalidations`]）：mesh/image 身份进失效集，
//!   flush_uploads 的去重判定从"查无才传"放宽为"查无**或失效**才传"；
//!   重传走"新区间/新图 → 原子换条目"（swap-on-commit），驻留不出现空洞，
//!   过渡帧引用者照画旧内容（热重载语义）。
//! - **关联重挂脏标**：引用失效资产的账本行重新进盘点。关联方式 = 重扫
//!   账本行（O(行数) 零维护）——行数量级小且 Modified 帧稀疏，反向索引
//!   的维护成本买不回什么。
//!   材质没有驻留内容（参数走每帧 UBO），`Modified(material)` 只做行级
//!   联动（贴图槽可能换指）；`Modified(image)` 经"材质五槽"反向找引用者。
//!
//! 卸载回收（4.4.1）：`Removed`（资产离开容器——强引用掉零的确定性释放或
//! 显式 remove）翻译成**待回收身份**（[`AssetRetirements`]），flush_uploads
//! 开头消费：驻留条目出账（网格区间/旧图进停尸房，描述符槽进待回收账），
//! 排空后才真正销毁/复用（在飞安全，4.4.2）。`Unused`（引用掉零但资产仍被
//! 容器持有）计数 + 出清失败记录（4.5 F8：未到货失败的资产只发 `Unused`
//! 不发 `Removed`，不出清则失败账只增不减）。材质 `Removed` 无驻留内容，
//! 只出清失败记录。账本行持强柄（`InstanceRow` 的 Handle）：强引用归零的
//! 释放路径必然已无行引用；**显式 `Assets::remove` 可与行引用并存**（4.5 F9
//! 订正：原"能发 Removed 必无行引用"表述只适用强引用归零路径）——同身份
//! 重插的重新驱动靠下方 `Added` 联动兜底。
//!
//! 事件纪律（判定线"事件不误伤"）：`Modified` 触发失效 + 复活（mutate 修
//! 数据重试放行，4.5 F6 的复活前提）、`Removed` 触发回收、`Unused` 计数 +
//! 失败出清、`Added` 无条件参与复活/重挂（4.5 F9：同身份卸载重插后行仍
//! "干净"，无联动则驻留被回收而无新上传驱动；无条件重挂的代价 = 初始加载期
//! 已脏行被重复标脏，幂等无成本）；`LoadedWithDependencies` 只计数（容器级
//! 语义，见 asset_readiness 模块说明）。
//!
//! 失败记账（4.5 F4 单入口）：三类 [`AssetLoadFailedEvent`] 的 failed 记账
//! 并入本系统（原独立 track_asset_failures 与本系统无排序约束，复活清旗标
//! 与重挂读旗标分属两系统，调度顺序不同结果不同，CPU 复现 dirty=1/0）。
//! 系统内固定次序 = 先失败记账、后 AssetEvent 复活/重挂，同帧"失败+复活"对
//! 确定性裁决。
//!
//! 时序：本系统挂 PostUpdate、账本对账之后——同帧失效集与脏行在 Last 被
//! flush_uploads 消费。BRP 写值发生在 RemoteLast（Last 之后），事件经
//! Messages 双缓冲下一帧才可读，故"改内容 → 次帧重传上屏"（与 4.1 的
//! spawn/despawn 次帧生效同拍）。
//!
//! 零 Vulkan 代码：失效集与回收账都是 ECS 侧事实，GPU 侧动作全在 flush_uploads。

use std::collections::HashSet;

use bevy::{
    asset::{AssetEvent, AssetLoadFailedEvent},
    ecs::system::SystemParam,
    image::Image,
    pbr::StandardMaterial,
    prelude::*,
};

use ash_macros::system;

use super::ledger::{IncrementStats, InstanceLedger};

/// 资产失效凭据（4.2.2）：待换内容的 mesh/image 身份集。flush 的去重判定凭
/// 它放行"已驻留但要换内容"的资产；重传提交成功后由 flush 消费（未提交成功
/// 的身份保留，行保持脏标下帧重试）。材质不进此集——它没有 GPU 驻留内容，
/// 失效只体现在行级重挂脏标。
#[derive(Resource, Default)]
pub struct AssetInvalidations {
    meshes: HashSet<AssetId<Mesh>>,
    images: HashSet<AssetId<Image>>,
}

impl AssetInvalidations {
    /// 该 mesh 身份是否待换内容（flush 去重判定的放行凭据）。
    #[must_use]
    pub fn mesh_pending(&self, id: AssetId<Mesh>) -> bool {
        self.meshes.contains(&id)
    }

    /// 该贴图身份是否待换内容（flush 去重判定的放行凭据）。
    #[must_use]
    pub fn image_pending(&self, id: AssetId<Image>) -> bool {
        self.images.contains(&id)
    }

    /// 消费一个已换内容成功的 mesh 身份（flush 提交成功后调用；未成功的
    /// 身份保留，行保持脏标下帧重试）。
    pub(crate) fn consume_mesh(&mut self, id: AssetId<Mesh>) {
        self.meshes.remove(&id);
    }

    /// 消费一个已换内容成功的贴图身份（语义同 [`Self::consume_mesh`]）。
    pub(crate) fn consume_image(&mut self, id: AssetId<Image>) {
        self.images.remove(&id);
    }
}

/// 卸载回收计量（4.4.1，判定线证据口径）：事件数与出账数分开报。
#[derive(Clone, Copy, Debug, Default)]
pub struct RecycleStats {
    /// mesh `Removed` 事件累计。
    pub removed_meshes_total: u64,
    /// 贴图 `Removed` 事件累计。
    pub removed_images_total: u64,
    /// `Unused` 事件累计（mesh+image+材质；探针口径：本宿主是否触发过）。
    pub unused_events_total: u64,
    /// flush 实际出账的驻留条目累计（mesh+image，幂等去重后）。
    pub resident_reclaimed_total: u64,
    /// flush 实际归还的描述符槽累计（排空后，纹理+采样器槽）。
    pub slots_reclaimed_total: u64,
}

/// 卸载回收账面（4.4.1）：事件读侧记下的待回收 mesh/image 身份。flush_uploads
/// 开头消费（驻留条目出账 + 停尸房 + 槽待回收账 + 凭据/失败记录出清），下帧
/// 排空点才真正销毁/复用——旧内容多活一帧，无悬空。材质无驻留内容不进此账。
#[derive(Resource, Default)]
pub struct AssetRetirements {
    meshes: HashSet<AssetId<Mesh>>,
    images: HashSet<AssetId<Image>>,
    pub stats: RecycleStats,
}

impl AssetRetirements {
    /// 记一笔待回收 mesh 身份（幂等；计量按事件记，出账去重在 flush）。
    pub fn retire_mesh(&mut self, id: AssetId<Mesh>) {
        self.meshes.insert(id);
        self.stats.removed_meshes_total += 1;
    }

    /// 记一笔待回收贴图身份（语义同 [`Self::retire_mesh`]）。
    pub fn retire_image(&mut self, id: AssetId<Image>) {
        self.images.insert(id);
        self.stats.removed_images_total += 1;
    }

    /// `Unused` 事件计数（只观察不动作——本宿主是否触发是探针口径）。
    pub fn note_unused(&mut self) {
        self.stats.unused_events_total += 1;
    }

    /// 取走全部待回收 mesh 身份（flush 开头消费，账随取随清）。
    pub(crate) fn take_meshes(&mut self) -> Vec<AssetId<Mesh>> {
        self.meshes.drain().collect()
    }

    /// 取走全部待回收贴图身份（语义同 [`Self::take_meshes`]）。
    pub(crate) fn take_images(&mut self) -> Vec<AssetId<Image>> {
        self.images.drain().collect()
    }
}

/// 资产事件读侧插件：初始化 [`AssetInvalidations`] 与 [`AssetRetirements`]，
/// 把 [`propagate_asset_changes`] 挂 PostUpdate（账本对账之后、Last 上传之前）。
pub struct AshAssetEventPlugin;

impl Plugin for AshAssetEventPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AssetInvalidations>()
            .init_resource::<AssetRetirements>()
            .add_systems(PostUpdate, propagate_asset_changes.after(super::ledger::update_ledger));
    }
}

/// 六路事件 reader 束（SystemParam 参数束，与 upload.rs 的 UploadData 同款
/// 惯例：系统签名保持精简；`AssetEvent<A>`/`AssetLoadFailedEvent<A>` 的 Item
/// 随泛型而异本就 chain 不了，分开读不并链。4.5 F4 起失败事件并入此处，
/// 复活与重挂同系统单入口）。
#[derive(SystemParam)]
pub(crate) struct AssetEventReaders<'w, 's> {
    mesh_events: MessageReader<'w, 's, AssetEvent<Mesh>>,
    material_events: MessageReader<'w, 's, AssetEvent<StandardMaterial>>,
    image_events: MessageReader<'w, 's, AssetEvent<Image>>,
    mesh_failures: MessageReader<'w, 's, AssetLoadFailedEvent<Mesh>>,
    material_failures: MessageReader<'w, 's, AssetLoadFailedEvent<StandardMaterial>>,
    image_failures: MessageReader<'w, 's, AssetLoadFailedEvent<Image>>,
}

/// 事件读侧本体（4.5 F4 起单一入口）：三类资产的 `Modified` 进失效集、复活
/// 并引用行重挂脏标；`Removed` 进卸载回收账（4.4.1）+ 失败记录出清；`Unused`
/// 计数 + 失败记录出清（F8）；`Added` 无条件参与复活/重挂（F9）；三类
/// `AssetLoadFailedEvent` 进 failed 集 + 计量 + 一次可见告警。系统内固定次序
/// = 先失败记账、后 AssetEvent 复活/重挂（F4：同帧"失败+复活"对确定性裁决，
/// 不依赖两个无序系统）。无引用者的失败也计数（事件不误伤的镜像面）。
#[system]
pub(crate) fn propagate_asset_changes(
    mut readers: AssetEventReaders,
    mut invalidations: ResMut<AssetInvalidations>,
    mut retirements: ResMut<AssetRetirements>,
    mut readiness: ResMut<super::asset_readiness::AssetReadiness>,
    mut ledger: ResMut<InstanceLedger>,
    std_materials: Res<Assets<StandardMaterial>>,
) {
    let AssetEventReaders {
        ref mut mesh_events,
        ref mut material_events,
        ref mut image_events,
        ref mut mesh_failures,
        ref mut material_failures,
        ref mut image_failures,
    } = readers;
    // 失败记账先于 AssetEvent 复活判定（F4 固定次序）；帧窗口计量先归零
    //（F10：当帧字段每帧刷新，空事件帧不滞留上帧值）
    readiness.stats.failures_frame = 0;
    let mut failures = 0usize;
    for event in mesh_failures.read() {
        failures += 1;
        readiness.note_failure_mesh(event.id);
        warn!("资产加载失败 mesh {:?}（{}），出重试队列不再重试", event.id, event.error);
    }
    for event in material_failures.read() {
        failures += 1;
        readiness.note_failure_material(event.id);
        warn!("资产加载失败 材质 {:?}（{}），出重试队列不再重试", event.id, event.error);
    }
    for event in image_failures.read() {
        failures += 1;
        readiness.note_failure_image(event.id);
        warn!("资产加载失败贴图 {:?}（{}），出重试队列不再重试", event.id, event.error);
    }
    if failures > 0 {
        info!("资产失败读侧:本帧 {failures} 个身份进 failed 集，行出队由 flush 盘点裁决");
    }
    let mut modified_count = 0usize;
    let mut invalidating_count = 0usize; // mesh+image 的 Modified（进失效集，计量口径）
    let mut ignored_count = 0usize;
    // 事件总数按"收到计"取消费前的 reader 剩余量（F10：全部变体进总数）
    let asset_events_frame =
        mesh_events.len() + material_events.len() + image_events.len();
    let mut modified_materials: HashSet<AssetId<StandardMaterial>> = HashSet::new();
    // 复活联动集（4.4.3，4.5 F9 起无条件参与）：failed 身份有新内容进场
    //（文件修复后重载的 Added）或同身份重插（卸载后 `Assets::insert`）——
    // 出 failed 集 + 引用行重挂脏标。失败的行在 flush 盘点时已出队（清脏标
    // ），无联动则修复后的资产永不重试；卸载重插的行驻留已回收且行干净，
    // 无联动则新内容永不上传。
    let mut revived_meshes: HashSet<AssetId<Mesh>> = HashSet::new();
    let mut revived_images: HashSet<AssetId<Image>> = HashSet::new();

    for event in mesh_events.read() {
        match event {
            AssetEvent::Modified { id } => {
                modified_count += 1;
                invalidating_count += 1;
                invalidations.meshes.insert(*id);
                readiness.revive_mesh(*id); // F6 复活前提：mutate 修数据重试放行
            }
            AssetEvent::Removed { id } => {
                retirements.retire_mesh(*id);
                readiness.clear_failure_mesh(*id);
            }
            AssetEvent::Unused { id } => {
                retirements.note_unused();
                readiness.clear_failure_mesh(*id); // F8：Unused 不出清则失败账只增
            }
            AssetEvent::Added { id } => {
                readiness.revive_mesh(*id);
                revived_meshes.insert(*id); // F9：无条件参与重挂判定
            }
            AssetEvent::LoadedWithDependencies { .. } => ignored_count += 1,
        }
    }
    for event in material_events.read() {
        match event {
            AssetEvent::Modified { id } => {
                modified_count += 1;
                readiness.revive_material(*id);
                modified_materials.insert(*id);
            }
            AssetEvent::Removed { id } => readiness.clear_failure_material(*id),
            AssetEvent::Unused { id } => {
                retirements.note_unused();
                readiness.clear_failure_material(*id); // F8 同口径
            }
            AssetEvent::Added { id } => {
                readiness.revive_material(*id);
                modified_materials.insert(*id); // F7：材质复活也重挂引用行
            }
            AssetEvent::LoadedWithDependencies { .. } => ignored_count += 1,
        }
    }
    for event in image_events.read() {
        match event {
            AssetEvent::Modified { id } => {
                modified_count += 1;
                invalidating_count += 1;
                invalidations.images.insert(*id);
                readiness.revive_image(*id);
            }
            AssetEvent::Removed { id } => {
                retirements.retire_image(*id);
                readiness.clear_failure_image(*id);
            }
            AssetEvent::Unused { id } => {
                retirements.note_unused();
                readiness.clear_failure_image(*id); // F8 同口径
            }
            AssetEvent::Added { id } => {
                readiness.revive_image(*id);
                revived_images.insert(*id); // F9 同口径
            }
            AssetEvent::LoadedWithDependencies { .. } => ignored_count += 1,
        }
    }
    // 帧窗口统计先落账再谈早退（F10）：当帧字段每帧刷新，累计量只增，
    // 当前失效集规模（存量）另列
    {
        let stats = &mut ledger.stats;
        stats.asset_events = asset_events_frame;
        stats.asset_events_total += asset_events_frame as u64;
        stats.asset_dirty_rows = 0;
        stats.asset_invalidations_total += invalidating_count as u64;
        stats.asset_invalidations_current =
            invalidations.meshes.len() + invalidations.images.len();
    }
    if modified_count == 0
        && modified_materials.is_empty()
        && revived_meshes.is_empty()
        && revived_images.is_empty()
        && retirements.meshes.is_empty()
        && retirements.images.is_empty()
    {
        return; // 稳态零事件：早退，不付重扫账本的成本（帧统计已落）
    }

    // —— 失效联动：重扫账本行，引用了失效资产的行重挂脏标（flush 盘点后
    // 按失效凭据换内容）。材质参数变化画面自动生效（每帧 UBO 直读），重挂
    // 脏标只为贴图槽换指的重盘点；无谓的重传不会发生——flush 盘点按"查无
    // 或失效"判定，材质行重挂后若全部身份照旧驻留，就地清标零动作。
    let mut hit_rows: Vec<Entity> = Vec::new();
    for (entity, row) in ledger.rows() {
        let mesh_hit = invalidations.mesh_pending(row.mesh.id()) || revived_meshes.contains(&row.mesh.id());
        let material_hit = modified_materials.contains(&row.material.id());
        let image_hit = std_materials.get(&row.material).is_some_and(|m| {
            material_texture_slots(m)
                .any(|id| invalidations.image_pending(id) || revived_images.contains(&id))
        });
        if mesh_hit || material_hit || image_hit {
            hit_rows.push(entity);
        }
    }
    let requeued = hit_rows.len();
    for entity in hit_rows {
        ledger.mark_dirty(entity);
    }

    let stats: &mut IncrementStats = &mut ledger.stats;
    stats.asset_dirty_rows = requeued;
    info!(
        "资产事件:Modified {modified_count} / 忽略 {ignored_count},失效凭据 mesh {} + 贴图 {},\
         关联重挂脏标 {requeued} 行;卸载回收账 mesh {} + 贴图 {} 待出账,Unused 累计 {}",
        invalidations.meshes.len(),
        invalidations.images.len(),
        retirements.meshes.len(),
        retirements.images.len(),
        retirements.stats.unused_events_total,
    );
}

/// 材质五槽的资产身份（与 upload.rs 的 `TEXTURE_SLOTS` 同清单——槽清单是
/// 两处共享的口径，改动必须同步）。
fn material_texture_slots(m: &StandardMaterial) -> impl Iterator<Item = AssetId<Image>> + '_ {
    [
        &m.base_color_texture,
        &m.metallic_roughness_texture,
        &m.occlusion_texture,
        &m.normal_map_texture,
        &m.emissive_texture,
    ]
    .into_iter()
    .flatten()
    .map(|h| h.id())
}
