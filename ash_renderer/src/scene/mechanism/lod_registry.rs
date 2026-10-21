//! LOD 组登记处（6.2.2）：逻辑 mesh → 各档变体柄的 CPU 侧映射账——flush
//! 上传链按它把档位资产一并盘点上传，draw_frame 的 LOD 表写入按它取各档
//! 引脚，BRP `lod_census` 按它报组信息。纯 CPU 侧账本，零 Vulkan 代码
//!（引脚现查 [`MeshPool`]，与 4.1"GPU 足迹不进账本"同纪律）。
//!
//! 形状口径（6.2.1 钉明）：LOD 阶梯仅支持 sphere（ico 细分参数面
//! 4/3/2/1 四档）——其余四形状无细分参数，扩形状 = 加 builder 参数映射，
//! 无结构代价。基准档（level 0）复用 [`TestShapeAssets`] 的形状缓存柄
//!（同参零新资产），level 1.. 按形状缓存进本资源——重复 spawn 阶梯共享
//! 柄零重传（4.1 资产经济同款）。
//!
//! 异步边界（6.2 范围钉明）：阶梯档位是程序化网格、随实体首次盘点同步
//! 进容器即时上传；异步到货档位（load_mesh 注册组）的重试通道不归本段
//! ——6.3 驻留管理器接管"请求 → 上传"后由其账面驱动（本登记处届时只
//! 供查表）。

use std::collections::HashMap;

use bevy::prelude::*;

use super::{ObjectOpError, TestShape, spawn_test_shape};
use crate::vulkan::MAX_LOD_LEVELS;

/// LOD 组登记处：基准 mesh 身份 → 各档变体柄（level 0 = 基准自身），加
/// level → base 反查（档位资产到货时重挂引用行 LOD 脏标用）与变体网格
/// 缓存（形状 × 档位 → 柄，同参共享零新资产）。
///
/// 换代号（7.9）：每次 `register` 递增——DrawList 组装的锚解析以
/// `levels_of` 为输入，换组 = 锚解析变，组装缓存凭换代号触发全量重建
///（消费端 [`crate::scene::InstanceLedger`] 比对上次组装的换代号）。
#[derive(Resource, Default)]
pub struct LodGroups {
    by_base: HashMap<AssetId<Mesh>, [Option<Handle<Mesh>>; MAX_LOD_LEVELS as usize]>,
    level_to_base: HashMap<AssetId<Mesh>, AssetId<Mesh>>,
    variants: HashMap<(TestShape, u32), Handle<Mesh>>,
    generation: u64,
}

impl LodGroups {
    /// 登记换代号（每次 register 递增；组装缓存换形触发键）。
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 登记一组 LOD：`levels[0]` = 基准柄自身（与实体 Mesh3d 同柄），
    /// `levels[1..]` = 渐粗变体；不足 [`MAX_LOD_LEVELS`] 的尾档留 None
    ///（哨兵，消费端向更细档回退）。同基准重复登记以新组为准（换组语义）。
    pub fn register(&mut self, levels: [Option<Handle<Mesh>>; MAX_LOD_LEVELS as usize]) {
        let Some(base) = levels[0].as_ref().map(Handle::id) else {
            return; // 无基准 = 空组，不登记（调用方保证 level 0 必有）
        };
        let mut group: [Option<Handle<Mesh>>; MAX_LOD_LEVELS as usize] = Default::default();
        for (level, handle) in levels.into_iter().enumerate() {
            if let Some(handle) = handle {
                self.level_to_base.insert(handle.id(), base);
                group[level] = Some(handle);
            }
        }
        self.by_base.insert(base, group);
        self.generation += 1;
    }

    /// 基准 mesh 的各档变体柄（查无 = 该 mesh 不在 LOD 组内——非阶梯实体，
    /// 消费端全部走哨兵回退）。
    #[must_use]
    pub fn levels_of(&self, id: AssetId<Mesh>) -> Option<&[Option<Handle<Mesh>>; MAX_LOD_LEVELS as usize]> {
        self.by_base.get(&id)
    }

    /// 档位资产 → 所属基准身份（档位到货重挂引用行的反查口）。
    #[must_use]
    pub fn base_of(&self, id: AssetId<Mesh>) -> Option<AssetId<Mesh>> {
        self.level_to_base.get(&id).copied()
    }

    /// 变体缓存命中（查缓存只读借用，构 mesh 与登记各自独占借用——World
    /// 借用不并存）。
    #[must_use]
    pub fn cached_variant(&self, shape: TestShape, level: u32) -> Option<Handle<Mesh>> {
        self.variants.get(&(shape, level)).cloned()
    }

    /// 登记档位变体缓存（构造后调用；同键覆盖以新柄为准）。
    pub fn insert_variant(&mut self, shape: TestShape, level: u32, handle: Handle<Mesh>) {
        self.variants.insert((shape, level), handle);
    }
}

/// 档位细分阶梯（6.2 钉明）：sphere = ico 细分 4/3/2/1（三角形数约
/// 1280/320/80/20，四档覆盖"近清远粗"的可观测跨度）。level 0 与
/// [`TestShapeAssets`] 的形状缓存同参数（ico 4）——基准柄复用零重复资产。
fn build_lod_mesh(shape: TestShape, level: u32) -> Result<Mesh, ObjectOpError> {
    match shape {
        TestShape::Sphere => {
            let subdivisions = match level {
                0 => 4,
                1 => 3,
                2 => 2,
                3 => 1,
                _ => return Err(ObjectOpError::InvalidParams(format!("LOD 档位 {level} 越界（0..{MAX_LOD_LEVELS}）"))),
            };
            Sphere::new(0.5)
                .mesh()
                .ico(subdivisions)
                .map_err(|e| ObjectOpError::InvalidParams(format!("球体细分 {subdivisions} 失败：{e}")))
        }
        _ => Err(ObjectOpError::InvalidParams(
            "LOD 阶梯仅支持 sphere（ico 细分参数面）；其余形状无细分参数，扩形状 = 加 builder 映射（6.2.1 钉明）"
                .into(),
        )),
    }
}

/// LOD 阶梯读回：实体号与各档资产身份（对账锚点，BRP `spawn_lod` 返回值）。
#[derive(Debug)]
pub struct LodLadderReadback {
    pub entity: Entity,
    /// 各档 mesh 资产身份（level 0 = 基准，与实体 Mesh3d 同身份）。
    pub levels: Vec<AssetId<Mesh>>,
}

/// spawn 一个 LOD 阶梯测试体：基准档走 [`spawn_test_shape`]（复用形状/
/// 材质缓存），level 1.. 档位网格按形状缓存构造并登记进 [`LodGroups`]。
/// 账本经 Added 通道自动入行；档位资产随该行首次盘点一并上传（upload
/// 链的 LOD 组盘点段）。
///
/// # Errors
/// 形状不支持阶梯 / mesh 构造失败（细分越限——单位口径下实际不可达）。
pub fn spawn_lod_ladder(
    world: &mut World,
    shape: TestShape,
    color: [f32; 3],
    translation: Option<Vec3>,
    scale: Option<Vec3>,
    name: Option<&str>,
    unique: bool,
) -> Result<LodLadderReadback, ObjectOpError> {
    // 阶梯仅 sphere（四形状无细分参数面，形状口径见模块注释）
    if shape != TestShape::Sphere {
        return Err(ObjectOpError::InvalidParams(
            "LOD 阶梯仅支持 sphere（ico 细分参数面 4/3/2/1）".into(),
        ));
    }
    let readback = spawn_test_shape(world, shape, color, translation, None, scale, name, unique)?;
    let base_handle = world
        .get::<Mesh3d>(readback.entity)
        .map(|m| m.0.clone())
        .ok_or_else(|| ObjectOpError::InvalidParams("spawn 返回实体缺 Mesh3d——状态矛盾".into()))?;
    let mut levels: [Option<Handle<Mesh>>; MAX_LOD_LEVELS as usize] = Default::default();
    levels[0] = Some(base_handle.clone());
    for level in 1..MAX_LOD_LEVELS {
        // 借用逐段独占：查缓存（只读）→ 构造 + 入容器（Assets 独占）→
        // 登记缓存（LodGroups 独占）——World 双可变借用不并存
        // unique：变体同样绕缓存直造（每阶梯独立四档资产，6.3 压测前提）
        let handle = match world.resource::<LodGroups>().cached_variant(shape, level) {
            Some(handle) if !unique => handle,
            _ => {
                let mesh = build_lod_mesh(shape, level)?;
                let handle = world.resource_mut::<Assets<Mesh>>().add(mesh);
                if !unique {
                    world
                        .resource_mut::<LodGroups>()
                        .insert_variant(shape, level, handle.clone());
                }
                handle
            }
        };
        levels[level as usize] = Some(handle);
    }
    let registered: Vec<AssetId<Mesh>> = {
        let mut groups = world.resource_mut::<LodGroups>();
        let ids: Vec<AssetId<Mesh>> = levels.iter().flatten().map(|handle| handle.id()).collect();
        groups.register(levels);
        ids
    };
    Ok(LodLadderReadback {
        entity: readback.entity,
        levels: registered,
    })
}

#[cfg(test)]
mod tests {
    use super::{LodGroups, MAX_LOD_LEVELS};
    use bevy::prelude::*;

    /// 登记映射的往返：levels_of 按基准身份取全档、base_of 反查逐档成立、
    /// 未登记身份查无（消费端哨兵回退的判定前提）。
    #[test]
    fn register_and_lookup_roundtrip() {
        let mut meshes = Assets::<Mesh>::default();
        let sphere = |subdiv: u32| {
            Sphere::new(0.5)
                .mesh()
                .ico(subdiv)
                .expect("细分上限内不失败")
        };
        let handles: Vec<Handle<Mesh>> = (0..MAX_LOD_LEVELS)
            .map(|_| meshes.add(sphere(4)))
            .collect();
        let base_id = handles[0].id();
        let mut groups = LodGroups::default();
        let mut arr: [Option<Handle<Mesh>>; MAX_LOD_LEVELS as usize] = Default::default();
        for (i, h) in handles.iter().enumerate() {
            arr[i] = Some(h.clone());
        }
        groups.register(arr);
        let levels = groups.levels_of(base_id).expect("基准身份必命中");
        assert_eq!(levels[0].as_ref().map(bevy::asset::Handle::id), Some(base_id));
        for handle in &handles {
            assert_eq!(groups.base_of(handle.id()), Some(base_id), "逐档反查归基准");
        }
        // 未登记身份查无（非阶梯实体走哨兵回退的前提）
        let other = meshes.add(sphere(4));
        assert!(groups.levels_of(other.id()).is_none());
        assert!(groups.base_of(other.id()).is_none());
    }
}
