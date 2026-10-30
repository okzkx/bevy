//! 维护 DrawList 可绘行的增量缓存。账本变更点标记待重估行，帧循环只读缓存视图。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bevy::{asset::AssetId, pbr::StandardMaterial, prelude::*};

use super::ledger::InstanceRow;

/// 可绘行的组装产物：(实例槽, 材质身份, alpha<1)。
type Drawable = (u32, AssetId<StandardMaterial>, bool);

/// 组装可绘判定闭包：行 → (槽号, 材质身份, alpha<1)；None = 暂缓。
pub(crate) type AssemblyEval<'a> = &'a mut dyn FnMut(&InstanceRow) -> Option<Drawable>;

/// 可绘行集合的增量缓存；首帧、锚开关变化或 LOD 登记换代时全量重建。
#[derive(Default)]
pub(crate) struct AssemblyCache {
    /// 首帧全量播种旗（重建后复位重估通道照常）。
    seeded: bool,
    /// 上次组装的锚开关（翻转 = 锚解析换形，全量重建）。
    last_floor_anchor: Option<bool>,
    /// 上次组装的 LOD 登记换代号（登记处 register 换组 = 锚解析变，全量重建）。
    last_lod_generation: u64,
    /// 可绘行 → 组装产物。BTreeMap 保 Entity 序——与全量走访（账本行迭代序）
    /// 的产物逐行一致。
    drawable: BTreeMap<Entity, Drawable>,
    /// 材质身份 → 可绘引用行数（材质集增量的引用计数；减到 0 出表）。
    material_refs: HashMap<AssetId<StandardMaterial>, usize>,
    /// 待重估行集（各变更点的输入侧，[`Self::update`] 消费后清空）。
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

/// 组装产物只读视图：draw_frame 在组装维护与奇偶集取走之后创建，供环写入
/// 与诊断读数全程借用——`pending_rows` 由调用方按 `账本行数 − slots.len()`
/// 现算（缓存维护后恒成立）。
pub(crate) struct AssemblyView<'a> {
    /// 可绘行实例槽号列表（Entity 序，与全量走访产物逐行一致）。
    pub slots: &'a [u32],
    /// 可绘行的去重材质身份（材质环全量重写的输入；迭代序不指定）。
    pub materials: &'a [AssetId<StandardMaterial>],
    /// alpha<1 的可绘行数。
    pub alpha_override: usize,
}

impl AssemblyCache {
    /// 标记一行待重估（幂等——重复标记零成本）。
    pub(super) fn mark_stale(&mut self, entity: Entity) {
        self.stale.insert(entity);
    }

    /// 增量维护：消费重估集，按 `eval` 维护可绘行缓存。全量重建仅在首帧播种
    /// 或换形（锚开关翻转 / LOD 登记换代）发生；其余帧只重估重估集。
    pub(super) fn update(
        &mut self,
        rows: &BTreeMap<Entity, InstanceRow>,
        eval: AssemblyEval<'_>,
        floor_anchor: bool,
        lod_generation: u64,
    ) {
        let rebuild = !self.seeded
            || self.last_floor_anchor != Some(floor_anchor)
            || self.last_lod_generation != lod_generation;
        if rebuild {
            self.drawable.clear();
            self.material_refs.clear();
            self.stale.clear();
            self.alpha_override = 0;
            for (entity, row) in rows {
                if let Some(drawable) = eval(row) {
                    self.admit(*entity, drawable);
                }
            }
            self.seeded = true;
            self.last_floor_anchor = Some(floor_anchor);
            self.last_lod_generation = lod_generation;
            self.rebuilds_total += 1;
            self.rebuild_products();
            return;
        }
        let stale = std::mem::take(&mut self.stale);
        self.drained_rows_total += stale.len() as u64;
        self.last_drain_rows = stale.len() as u64;
        let mut changed = false;
        for entity in stale {
            self.reevals_total += 1;
            // 旧可绘态先退场（引用计数/alpha 收账）；行已清 = 只退场不重估
            changed |= self.retire(entity);
            let Some(row) = rows.get(&entity) else {
                continue;
            };
            if let Some(drawable) = eval(row) {
                self.admit(entity, drawable);
                changed = true;
            }
            // 仍暂缓：可绘集无此行（pending = 行总数 − 可绘数，现算）
        }
        if changed {
            self.rebuild_products();
        }
    }

        #[must_use]
    pub(super) fn view(&self) -> AssemblyView<'_> {
        AssemblyView {
            slots: &self.slots,
            materials: &self.materials,
            alpha_override: self.alpha_override,
        }
    }

        #[must_use]
    pub(super) fn drawable_len(&self) -> usize {
        self.drawable.len()
    }

    /// 可绘行入集：材质引用计数 + alpha 计数随之进账。
    fn admit(&mut self, entity: Entity, drawable: Drawable) {
        let (_, mat, alpha) = drawable;
        *self.material_refs.entry(mat).or_insert(0) += 1;
        if alpha {
            self.alpha_override += 1;
        }
        self.drawable.insert(entity, drawable);
    }

    /// 可绘行退场：材质引用计数递减（0 出表）+ alpha 收账。返回该行原本是否可绘。
    fn retire(&mut self, entity: Entity) -> bool {
        let Some((_, mat, alpha)) = self.drawable.remove(&entity) else {
            return false;
        };
        if let Some(count) = self.material_refs.get_mut(&mat) {
            *count -= 1;
            if *count == 0 {
                self.material_refs.remove(&mat);
            }
        }
        if alpha {
            self.alpha_override -= 1;
        }
        true
    }

    /// 产物重建（可绘集变化后调用）：slots 按 Entity 序展开、materials 按
    /// 材质表展开。变化帧 O(可绘行)，稳定帧不进此路（容量复用）。
    fn rebuild_products(&mut self) {
        self.slots.clear();
        self.slots.extend(self.drawable.values().map(|(slot, _, _)| *slot));
        self.materials.clear();
        self.materials.extend(self.material_refs.keys().copied());
    }
}
