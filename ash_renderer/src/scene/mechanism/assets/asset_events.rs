//! 将 mesh、image 与 material 事件转换为失效、引用行重挂、失败记账和回收记录。
//! 本系统在 PostUpdate 的账本对账后运行；flush 在 Last 消费这些记录。
//! mesh/image 的失效凭据在上传提交成功后清除；材质变化通过引用行重挂处理。
//! 坑：同身份资产移除后重新加入时，行可能已不脏；Added 必须重挂引用行，才能重新上传。
//! 失败记账先于事件复活，以固定同帧失败与复活的处理顺序。
//! BRP 在 RemoteLast 写入，消息下一帧可读，因此修改后最早下一帧重传。

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

/// 待换内容的 mesh/image 身份；flush 成功提交新内容后清除，失败时保留供重试。
/// 材质没有驻留内容，变化通过引用行重挂处理。
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

/// 卸载回收计量（判定线证据口径）：事件数与出账数分开报。
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

/// 卸载回收账面：事件读侧记下的待回收 mesh/image 身份。flush_uploads
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

    /// 记一笔驻留管理器发起的 mesh 淘汰（非 `Removed` 事件，
    /// 不进事件累计——淘汰计数归 [`crate::scene::ResidencyStats`]，账面
    /// 出账与停尸房复用与 [`Self::retire_mesh`] 完全同路）。
    pub fn eject_mesh(&mut self, id: AssetId<Mesh>) {
        self.meshes.insert(id);
    }

    /// 记一笔待回收贴图身份（语义同 [`Self::retire_mesh`]）。
    pub fn retire_image(&mut self, id: AssetId<Image>) {
        self.images.insert(id);
        self.stats.removed_images_total += 1;
    }

    /// 记一笔驻留管理器发起的贴图淘汰（非 `Removed` 事件，
    /// 不进事件累计；出账与停尸房复用与 [`Self::retire_image`] 完全同路）。
    pub fn eject_image(&mut self, id: AssetId<Image>) {
        self.images.insert(id);
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

/// 三类资产的事件与加载失败事件 reader。
#[derive(SystemParam)]
pub(crate) struct AssetEventReaders<'w, 's> {
    mesh_events: MessageReader<'w, 's, AssetEvent<Mesh>>,
    material_events: MessageReader<'w, 's, AssetEvent<StandardMaterial>>,
    image_events: MessageReader<'w, 's, AssetEvent<Image>>,
    mesh_failures: MessageReader<'w, 's, AssetLoadFailedEvent<Mesh>>,
    material_failures: MessageReader<'w, 's, AssetLoadFailedEvent<StandardMaterial>>,
    image_failures: MessageReader<'w, 's, AssetLoadFailedEvent<Image>>,
}

/// 事件读侧本体（单一入口）：三类资产的 `Modified` 进失效集、复活
/// 并引用行重挂脏标；`Removed` 进卸载回收账 + 失败记录出清；`Unused`
/// 计数 + 失败记录出清；`Added` 无条件参与复活/重挂；三类
/// `AssetLoadFailedEvent` 进 failed 集 + 计量 + 一次可见告警。系统内固定次序
/// = 先失败记账、后 AssetEvent 复活/重挂（同帧"失败+复活"确定性裁决，
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
    // 先记失败，再处理复活事件；每帧重置窗口计数。
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
    // 事件总数按"收到计"取消费前的 reader 剩余量（全部变体进总数）
    let asset_events_frame =
        mesh_events.len() + material_events.len() + image_events.len();
    let mut modified_materials: HashSet<AssetId<StandardMaterial>> = HashSet::new();
    // 复活联动集（无条件参与）：failed 身份有新内容进场
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
                readiness.revive_mesh(*id); // 复活前提：mutate 修数据重试放行
            }
            AssetEvent::Removed { id } => {
                retirements.retire_mesh(*id);
                readiness.clear_failure_mesh(*id);
            }
            AssetEvent::Unused { id } => {
                retirements.note_unused();
                readiness.clear_failure_mesh(*id); // Unused 不出清则失败账只增
            }
            AssetEvent::Added { id } => {
                readiness.revive_mesh(*id);
                revived_meshes.insert(*id); // 无条件参与重挂判定
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
            AssetEvent::Removed { id } => {
                readiness.clear_failure_material(*id);
                // 材质离开容器后引用行不可绘，标记重估以移除缓存中的旧项。
                for entity in ledger.rows_with_material(*id) {
                    ledger.mark_assembly_stale(entity);
                }
            }
            AssetEvent::Unused { id } => {
                retirements.note_unused();
                readiness.clear_failure_material(*id); // 与 mesh 同口径
            }
            AssetEvent::Added { id } => {
                readiness.revive_material(*id);
                modified_materials.insert(*id); // 材质复活也重挂引用行
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
                readiness.clear_failure_image(*id); // 与 mesh 同口径
            }
            AssetEvent::Added { id } => {
                readiness.revive_image(*id);
                revived_images.insert(*id); // 与 mesh 同口径
            }
            AssetEvent::LoadedWithDependencies { .. } => ignored_count += 1,
        }
    }
    // 帧窗口统计先落账再谈早退：当帧字段每帧刷新，累计量只增，
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
