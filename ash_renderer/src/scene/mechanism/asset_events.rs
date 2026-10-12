//! 资产事件读侧与失效联动（4.2.1/4.2.2）：发现侧的第二条入口——对象粒度的
//! 五类变更走 [`update_ledger`] 的 Changed/Removed 通道，资产粒度的内容变化
//! 走本模块的 `AssetEvent` 通道，两条并轨进同一份账本脏集。
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
//! 事件纪律（判定线"事件不误伤"）：只有 `Modified` 触发失效；`Added`/
//! `LoadedWithDependencies` 是首次驻留链的事（flush 查无即传，不需要凭据）；
//! `Removed`/`Unused` 的回收语义归 4.4——本段收到只计数，不误动作。
//!
//! 时序：本系统挂 PostUpdate、账本对账之后——同帧失效集与脏行在 Last 被
//! flush_uploads 消费。BRP 写值发生在 RemoteLast（Last 之后），事件经
//! Messages 双缓冲下一帧才可读，故"改内容 → 次帧重传上屏"（与 4.1 的
//! spawn/despawn 次帧生效同拍）。
//!
//! 零 Vulkan 代码：失效集是 ECS 侧事实，GPU 侧动作全在 flush_uploads。

use std::collections::HashSet;

use bevy::{asset::AssetEvent, image::Image, pbr::StandardMaterial, prelude::*};

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

/// 资产事件读侧插件：初始化 [`AssetInvalidations`]，把
/// [`propagate_asset_changes`] 挂 PostUpdate（账本对账之后、Last 上传之前）。
pub struct AshAssetEventPlugin;

impl Plugin for AshAssetEventPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AssetInvalidations>().add_systems(
            PostUpdate,
            propagate_asset_changes.after(super::ledger::update_ledger),
        );
    }
}

/// 事件读侧本体：三类资产的 `Modified` 进失效集 + 引用行重挂脏标 + 计量。
/// 三类 reader 各自类型化，分开处理不并链（`AssetEvent<A>` 的 Item 类型随
/// 泛型而异，chain 不了也不必 chain）。
#[system]
pub(crate) fn propagate_asset_changes(
    mut mesh_events: MessageReader<AssetEvent<Mesh>>,
    mut material_events: MessageReader<AssetEvent<StandardMaterial>>,
    mut image_events: MessageReader<AssetEvent<Image>>,
    mut invalidations: ResMut<AssetInvalidations>,
    mut ledger: ResMut<InstanceLedger>,
    std_materials: Res<Assets<StandardMaterial>>,
) {
    let mut modified_count = 0usize;
    let mut ignored_count = 0usize;
    let mut modified_materials: HashSet<AssetId<StandardMaterial>> = HashSet::new();

    for event in mesh_events.read() {
        match event {
            AssetEvent::Modified { id } => {
                modified_count += 1;
                invalidations.meshes.insert(*id);
            }
            _ => ignored_count += 1,
        }
    }
    for event in material_events.read() {
        match event {
            AssetEvent::Modified { id } => {
                modified_count += 1;
                modified_materials.insert(*id);
            }
            _ => ignored_count += 1,
        }
    }
    for event in image_events.read() {
        match event {
            AssetEvent::Modified { id } => {
                modified_count += 1;
                invalidations.images.insert(*id);
            }
            _ => ignored_count += 1,
        }
    }
    if modified_count == 0 {
        return; // 稳态零事件：早退，不付重扫账本的成本
    }

    // —— 失效联动：重扫账本行，引用了失效资产的行重挂脏标（flush 盘点后
    // 按失效凭据换内容）。材质参数变化画面自动生效（每帧 UBO 直读），重挂
    // 脏标只为贴图槽换指的重盘点；无谓的重传不会发生——flush 盘点按"查无
    // 或失效"判定，材质行重挂后若全部身份照旧驻留，就地清标零动作。
    let mut hit_rows: Vec<Entity> = Vec::new();
    for (entity, row) in ledger.rows() {
        let mesh_hit = invalidations.mesh_pending(row.mesh.id());
        let material_hit = modified_materials.contains(&row.material.id());
        let image_hit = std_materials.get(&row.material).is_some_and(|m| {
            material_texture_slots(m).any(|id| invalidations.image_pending(id))
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
    stats.asset_events = modified_count + ignored_count;
    stats.asset_events_total += (modified_count + ignored_count) as u64;
    stats.asset_dirty_rows = requeued;
    stats.asset_invalidations_total =
        (invalidations.meshes.len() + invalidations.images.len()) as u64;
    info!(
        "资产事件:Modified {modified_count} / 忽略 {ignored_count},失效凭据 mesh {} + 贴图 {},\
         关联重挂脏标 {requeued} 行",
        invalidations.meshes.len(),
        invalidations.images.len(),
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
