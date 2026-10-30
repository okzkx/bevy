//! 按资产身份维护异步资产的 pending 与 failed 账。
//! 命中 `Assets::get` 即视为就绪；pending 身份失去全部脏行引用时取消。
//! 加载失败或转换拒绝会停止重试，直到 Added/Modified 事件复活身份。
//! flush 独占 pending 写入；统一资产事件系统独占 failed 写入。

use std::collections::{HashMap, HashSet};

use bevy::{image::Image, pbr::StandardMaterial, prelude::*};

/// 异步就绪计量（判定线证据口径，与 [`super::ledger::IncrementStats`] 分列）：
/// pending/等待/到货/失败/取消分开报，不把"零上传"说成"零账面成本"。
#[derive(Clone, Copy, Debug, Default)]
pub struct ReadinessStats {
    /// 尚未到货的 mesh 身份数。
    pub pending_meshes: usize,
    /// 尚未到货的贴图身份数。
    pub pending_images: usize,
    /// 等待帧数累计（每帧每 pending 身份 +1；平均口径 = total / arrivals）。
    pub wait_frames_total: u64,
    /// 最近一次到货身份的等待帧数（单资产口径）。
    pub last_arrival_wait: u64,
    /// 到货累计（pending 身份成功转换上传、驻留登记的次数）。
    pub arrivals_total: u64,
    /// 加载失败事件累计（[`AssetLoadFailedEvent`] 收到数，含无人等待的身份）。
    pub failures_total: u64,
    /// 本帧失败数（同 [`Self::failures_total`] 的帧窗口）。
    pub failures_frame: usize,
    /// 转换/规格拒绝身份累计（flush 内确定性拒绝，warn 一次后不再重试）。
    pub rejects_total: u64,
    /// 取消累计（pending 身份失去全部引用行：despawn / 换柄）。
    pub cancelled_total: u64,
    /// 因失败腿出队的账本行累计（清脏标，不复活）。
    pub failed_rows_cleared_total: u64,
}

/// 异步就绪账面：pending 身份集（身份 → 已等待帧数）+ failed 身份集
/// （加载失败 ∪ 转换拒绝，不再重试）+ 失败读侧计量。flush_uploads 独占写
/// pending 侧（mark/arrive/expel/reconcile），failed 侧（note_failure/revive/
/// clear）由统一事件读侧单点写（单入口，无跨系统排序问题）。
#[derive(Resource, Default)]
pub struct AssetReadiness {
    pending_meshes: HashMap<AssetId<Mesh>, u64>,
    pending_images: HashMap<AssetId<Image>, u64>,
    failed_meshes: HashSet<AssetId<Mesh>>,
    failed_images: HashSet<AssetId<Image>>,
    failed_materials: HashSet<AssetId<StandardMaterial>>,
    pub stats: ReadinessStats,
}

impl AssetReadiness {
    /// 该 mesh 身份是否已失败（加载失败或转换拒绝，flush 盘点跳过重试）。
    #[must_use]
    pub fn mesh_failed(&self, id: AssetId<Mesh>) -> bool {
        self.failed_meshes.contains(&id)
    }

    /// 该贴图身份是否已失败（语义同 [`Self::mesh_failed`]）。
    #[must_use]
    pub fn image_failed(&self, id: AssetId<Image>) -> bool {
        self.failed_images.contains(&id)
    }

    /// 该材质身份是否已失败（材质无驻留内容，失败只体现为行级出队）。
    #[must_use]
    pub fn material_failed(&self, id: AssetId<StandardMaterial>) -> bool {
        self.failed_materials.contains(&id)
    }

    /// 加载失败记账（失败读侧调用）：进 failed 集（幂等），计量按事件数累计。
    pub fn note_failure_mesh(&mut self, id: AssetId<Mesh>) {
        self.failed_meshes.insert(id);
        self.stats.failures_total += 1;
        self.stats.failures_frame += 1;
    }

    /// 贴图加载失败记账（语义同 [`Self::note_failure_mesh`]）。
    pub fn note_failure_image(&mut self, id: AssetId<Image>) {
        self.failed_images.insert(id);
        self.stats.failures_total += 1;
        self.stats.failures_frame += 1;
    }

    /// 材质加载失败记账（语义同 [`Self::note_failure_mesh`]）。
    pub fn note_failure_material(&mut self, id: AssetId<StandardMaterial>) {
        self.failed_materials.insert(id);
        self.stats.failures_total += 1;
        self.stats.failures_frame += 1;
    }

    /// 身份复活（失败读侧收到 `Added`/`Modified` 时调用）：移出 failed 集，
    /// 后续重试放行（文件修复后重载、mutate 修数据）。幂等。
    pub fn revive_mesh(&mut self, id: AssetId<Mesh>) {
        self.failed_meshes.remove(&id);
    }

    /// 贴图身份复活（语义同 [`Self::revive_mesh`]）。
    pub fn revive_image(&mut self, id: AssetId<Image>) {
        self.failed_images.remove(&id);
    }

    /// 材质身份复活（语义同 [`Self::revive_mesh`]）。
    pub fn revive_material(&mut self, id: AssetId<StandardMaterial>) {
        self.failed_materials.remove(&id);
    }

    /// 失败记录出清（资产卸载联动）：身份随资产卸载出账——与 revive 的
    /// 区别 = 不是"重试放行"而是账面出清（资产已不在容器，旧失败记录是僵尸
    /// 账；同路径重载若再失败会重新进集，`Added` 事件 revive 双保险）。幂等。
    pub fn clear_failure_mesh(&mut self, id: AssetId<Mesh>) {
        self.failed_meshes.remove(&id);
    }

    /// 贴图失败记录出清（语义同 [`Self::clear_failure_mesh`]）。
    pub fn clear_failure_image(&mut self, id: AssetId<Image>) {
        self.failed_images.remove(&id);
    }

    /// 材质失败记录出清（语义同 [`Self::clear_failure_mesh`]）。
    pub fn clear_failure_material(&mut self, id: AssetId<StandardMaterial>) {
        self.failed_materials.remove(&id);
    }

    /// pending 记账（flush 转换段查无时调用）：新身份入账等待 0 帧，旧身份
    /// 保留已等待帧数（跨帧累计等待）。
    pub fn mark_pending_mesh(&mut self, id: AssetId<Mesh>) {
        self.pending_meshes.entry(id).or_insert(0);
    }

    /// 贴图 pending 记账（语义同 [`Self::mark_pending_mesh`]）。
    pub fn mark_pending_image(&mut self, id: AssetId<Image>) {
        self.pending_images.entry(id).or_insert(0);
    }

    /// 到货销账（flush 驻留登记成功后调用）：pending 身份离开账面，等待帧数
    /// 计入到货计量。非 pending 身份（稳态重传等）无账可销，静默无动作。
    pub fn arrive_mesh(&mut self, id: AssetId<Mesh>) {
        if let Some(waited) = self.pending_meshes.remove(&id) {
            self.stats.arrivals_total += 1;
            self.stats.last_arrival_wait = waited;
        }
    }

    /// 贴图到货销账（语义同 [`Self::arrive_mesh`]）。
    pub fn arrive_image(&mut self, id: AssetId<Image>) {
        if let Some(waited) = self.pending_images.remove(&id) {
            self.stats.arrivals_total += 1;
            self.stats.last_arrival_wait = waited;
        }
    }

    /// 拒绝销账（flush 转换拒绝时调用）：pending 记录作废，身份进 failed 集
    /// 不再重试。调用方按身份去重（warn 一次语义），计量按新身份累计。
    pub fn expel_mesh(&mut self, id: AssetId<Mesh>) {
        self.pending_meshes.remove(&id);
        if self.failed_meshes.insert(id) {
            self.stats.rejects_total += 1;
        }
    }

    /// 贴图拒绝销账（语义同 [`Self::expel_mesh`]）。
    pub fn expel_image(&mut self, id: AssetId<Image>) {
        self.pending_images.remove(&id);
        if self.failed_images.insert(id) {
            self.stats.rejects_total += 1;
        }
    }

    /// 失败腿出队的行数记账（flush 盘点裁决后调用，只累计不操作行）。
    pub fn note_failed_rows_cleared(&mut self, rows: usize) {
        self.stats.failed_rows_cleared_total += rows as u64;
    }

    /// 帧末对账（flush 收尾调用，含空批帧）：当帧仍被脏行引用的 pending 身份
    /// 等待 +1；失去全部引用的身份出账计取消（despawn/换柄的"到货时人已不在"）。
    pub fn reconcile(
        &mut self,
        marked_meshes: &HashSet<AssetId<Mesh>>,
        marked_images: &HashSet<AssetId<Image>>,
    ) {
        self.pending_meshes.retain(|id, waited| {
            if marked_meshes.contains(id) {
                *waited += 1;
                self.stats.wait_frames_total += 1;
                true
            } else {
                self.stats.cancelled_total += 1;
                false
            }
        });
        self.pending_images.retain(|id, waited| {
            if marked_images.contains(id) {
                *waited += 1;
                self.stats.wait_frames_total += 1;
                true
            } else {
                self.stats.cancelled_total += 1;
                false
            }
        });
        self.stats.pending_meshes = self.pending_meshes.len();
        self.stats.pending_images = self.pending_images.len();
    }
}

/// 初始化就绪账；failed 侧事件由 [`super::asset_events::AshAssetEventPlugin`] 接线。
pub struct AshReadinessPlugin;

impl Plugin for AshReadinessPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AssetReadiness>();
    }
}
