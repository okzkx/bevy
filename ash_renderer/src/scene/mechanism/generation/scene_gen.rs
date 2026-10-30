//! 运行期场景生成：共享资产句柄批量生成实例，或按空间块生成并管理实例。
//! 生成位置由 seed 和实例索引确定；相同参数可重现相同位置。

use crate::scene::mechanism::ObjectOpError;
use bevy::pbr::StandardMaterial;
use bevy::prelude::*;

/// 生成器参数：克隆源 + 规模 + 布局。全部显式给值（无缺省推导——
/// 复现的前提是参数完整），校验在 [`generate_instances`]。
#[derive(Debug, Clone)]
pub struct SceneGenParams {
    /// 克隆源（可渲染 primitive）：柄与基础变换的来源。
    pub source: Entity,
    /// 实例总数。
    pub count: u32,
    /// 横向列数；行数 = count 向上取整除列数。方阵铺满的调用方给
    /// `ceil(sqrt(count))`。
    pub columns: u32,
    /// 相邻实例的网格间距（世界单位）。
    pub spacing: f32,
    /// 随机种子：抖动位置的散列基。
    pub seed: u64,
    /// 位置抖动幅度（世界单位；x/z 分别偏移 [-jitter, jitter)；0 = 整齐网格）。
    pub jitter: f32,
}

/// 生成读回：计数与柄身份（写后读对账用）。柄身份 = 首个实例的
/// mesh/material 资产 id（全部克隆自同一源，批量内恒同源）。
#[derive(Debug)]
pub struct SceneGenReadback {
    pub spawned: u32,
    /// 第一个与最后一个新实体（次帧账本入行对账的锚点）。
    pub first: Entity,
    pub last: Entity,
    pub mesh: AssetId<Mesh>,
    pub material: AssetId<StandardMaterial>,
}

/// 生成网格实例并按索引编号；实体共享源资产且不带父实体。
///
/// # Errors
/// 源实体缺席/非 primitive/缺 Transform；参数为 0（count/columns）或
/// spacing 非正（协议面校验，不产出半批实体——spawn 前先全量校验）。
pub fn generate_instances(
    world: &mut World,
    p: SceneGenParams,
) -> Result<SceneGenReadback, ObjectOpError> {
    if p.count == 0 || p.columns == 0 {
        // 规模参数无效属于参数错误，不是空操作。
        return Err(ObjectOpError::InvalidParams(
            "count/columns 须 > 0（空场景直接不生成，删除走 chunk_despawn）".into(),
        ));
    }
    if p.spacing <= 0.0 || p.jitter < 0.0 {
        return Err(ObjectOpError::InvalidParams(format!(
            "spacing({}) 须 > 0、jitter({}) 须 ≥ 0",
            p.spacing, p.jitter
        )));
    }
    let Some(mesh) = world.get::<Mesh3d>(p.source).cloned() else {
        return Err(ObjectOpError::SourceNotPrimitive(p.source));
    };
    let Some(material) = world
        .get::<MeshMaterial3d<StandardMaterial>>(p.source)
        .cloned()
    else {
        return Err(ObjectOpError::SourceNotPrimitive(p.source));
    };
    let Some(base) = world.get::<Transform>(p.source).cloned() else {
        return Err(ObjectOpError::ComponentMissing("Transform", p.source));
    };
    let source_name = world
        .get::<Name>(p.source)
        .map(|n| n.as_str().to_owned())
        .unwrap_or_default();
    let first = spawn_one(world, &mesh, &material, &base, &source_name, 0, &p)?;
    let mut last = first;
    for i in 1..p.count {
        last = spawn_one(world, &mesh, &material, &base, &source_name, i, &p)?;
    }
    Ok(SceneGenReadback {
        spawned: p.count,
        first,
        last,
        mesh: mesh.0.id(),
        material: material.0.id(),
    })
}

/// 铺一个实例：第 `i` 行列 = `(i % columns, i / columns)`，位置加种子抖动
///（纯函数散列，见模块注释）。克隆 = 共享柄，不产生新资产。
fn spawn_one(
    world: &mut World,
    mesh: &Mesh3d,
    material: &MeshMaterial3d<StandardMaterial>,
    base: &Transform,
    source_name: &str,
    i: u32,
    p: &SceneGenParams,
) -> Result<Entity, ObjectOpError> {
    // 索引很大时转换为 f32 可能失去整数精度。
    let col = (i % p.columns) as f32;
    let row = (i / p.columns) as f32;
    let mut translation = base.translation + Vec3::new(col * p.spacing, 0.0, row * p.spacing);
    if p.jitter > 0.0 {
        translation.x += (hash01(p.seed, i, 0) - 0.5) * 2.0 * p.jitter;
        translation.z += (hash01(p.seed, i, 1) - 0.5) * 2.0 * p.jitter;
    }
    let mut transform = *base;
    transform.translation = translation;
    let name = format!("{source_name}#gen{i:05}");
    let entity = world
        .spawn((mesh.clone(), material.clone(), transform, Name::new(name)))
        .id();
    Ok(entity)
}

/// 按 seed、实例索引和通道生成 [0,1) 的确定性散列值。
fn hash01(seed: u64, index: u32, channel: u64) -> f32 {
    let mut z = seed
        ^ u64::from(index).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ channel.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // 取高 24 位并缩放到 [0,1)。
    (z >> 40) as f32 / (1u64 << 24) as f32
}

/// 生成期空间归属；chunk 根标识一块，实例作为其子实体。
/// 成员关系不随实体移动更新；chunk 用于 CPU 侧盘点与整块增删。
#[derive(Component, Debug, Clone, Copy)]
pub struct SceneChunk {
    /// 生成期编号（cz × 每轴块数 + cx）；census/despawn 按 id 寻址。
    pub id: u32,
    /// 块原点（世界坐标，根实体的 translation）。
    pub origin: Vec3,
    /// 块边长（列数 × spacing；相邻根实体原点相距 extent）。
    pub extent: f32,
}

/// chunk 化生成参数：每轴 `chunk_grid` 块、每块 `instances_per_chunk` 实例。
#[derive(Debug, Clone)]
pub struct ChunkGenParams {
    /// 克隆源（可渲染 primitive）：柄与基础变换（旋转/缩放）的来源。
    pub source: Entity,
    /// 每轴 chunk 数（总块数 = chunk_grid²）。
    pub chunk_grid: u32,
    /// 每 chunk 实例数。
    pub instances_per_chunk: u32,
    /// 相邻实例的网格间距（世界单位）。
    pub spacing: f32,
    /// 随机种子：抖动位置的散列基。
    pub seed: u64,
    /// 位置抖动幅度（世界单位；x/z 分别偏移 [-jitter, jitter)；0 = 整齐网格）。
    pub jitter: f32,
}

/// chunk 化生成读回：块/实例计数与首尾根实体（census 对账锚点）。
#[derive(Debug)]
pub struct ChunkGenReadback {
    pub chunks: u32,
    pub instances: u32,
    pub first_root: Entity,
    pub last_root: Entity,
    pub mesh: AssetId<Mesh>,
    pub material: AssetId<StandardMaterial>,
}

/// 按网格位置生成 chunk，每块包含网格实例，并可通过根实体整块增删。
/// 抖动散列使用跨块的平坦索引，同参数重跑得到相同位置。
///
/// # Errors
/// 参数为 0、spacing 非正、jitter 为负或生成总量溢出时拒绝。
pub fn generate_chunks(
    world: &mut World,
    p: ChunkGenParams,
) -> Result<ChunkGenReadback, ObjectOpError> {
    if p.chunk_grid == 0 || p.instances_per_chunk == 0 {
        return Err(ObjectOpError::InvalidParams(
            "chunk_grid/instances_per_chunk 须 > 0".into(),
        ));
    }
    if p.spacing <= 0.0 || p.jitter < 0.0 {
        return Err(ObjectOpError::InvalidParams(format!(
            "spacing({}) 须 > 0、jitter({}) 须 ≥ 0",
            p.spacing, p.jitter
        )));
    }
    let Some(mesh) = world.get::<Mesh3d>(p.source).cloned() else {
        return Err(ObjectOpError::SourceNotPrimitive(p.source));
    };
    let Some(material) = world
        .get::<MeshMaterial3d<StandardMaterial>>(p.source)
        .cloned()
    else {
        return Err(ObjectOpError::SourceNotPrimitive(p.source));
    };
    let Some(base) = world.get::<Transform>(p.source).cloned() else {
        return Err(ObjectOpError::ComponentMissing("Transform", p.source));
    };
        // 块内按近似方阵排列。
    let cols = (p.instances_per_chunk as f32).sqrt().ceil() as u32;
    let extent = cols as f32 * p.spacing;
    // 总量溢出 u32 = 参数非法（spawn 前拒绝，不产出半批实体）
    let Some((total_chunks, total_instances)) = p
        .chunk_grid
        .checked_mul(p.chunk_grid)
        .and_then(|chunks| Some((chunks, chunks.checked_mul(p.instances_per_chunk)?)))
    else {
        return Err(ObjectOpError::InvalidParams(format!(
            "chunk_grid²×instances_per_chunk 溢出 u32（chunk_grid {}，instances_per_chunk {}）",
            p.chunk_grid, p.instances_per_chunk
        )));
    };
    let mut flat = 0u32; // 跨块平坦计数（抖动散列的 index）
    let mut first_root = None;
    let mut last_root = None;
    for cz in 0..p.chunk_grid {
        for cx in 0..p.chunk_grid {
            let id = cz * p.chunk_grid + cx;
            let origin = Vec3::new(cx as f32 * extent, 0.0, cz as f32 * extent);
            let root = world
                .spawn((
                    SceneChunk {
                        id,
                        origin,
                        extent,
                    },
                    Transform::from_translation(origin),
                    Name::new(format!("chunk{id}")),
                ))
                .id();
            first_root.get_or_insert(root);
            last_root = Some(root);
            for i in 0..p.instances_per_chunk {
                let col = (i % cols) as f32;
                let row = (i / cols) as f32;
                let mut local = Vec3::new(col * p.spacing, 0.0, row * p.spacing);
                if p.jitter > 0.0 {
                    local.x += (hash01(p.seed, flat, 0) - 0.5) * 2.0 * p.jitter;
                    local.z += (hash01(p.seed, flat, 1) - 0.5) * 2.0 * p.jitter;
                }
                flat += 1;
                let mut transform = base;
                // 坑：块原点由父级传播，子实体 translation 必须保持块内坐标。
                transform.translation = local;
                let child = world
                    .spawn((
                        mesh.clone(),
                        material.clone(),
                        transform,
                        Name::new(format!("chunk{id}/inst{flat:05}")),
                    ))
                    .id();
                world.entity_mut(root).add_child(child);
            }
        }
    }
    // chunk_grid 已校验为正，至少会生成一个根实体。
    let (Some(first_root), Some(last_root)) = (first_root, last_root) else {
        return Err(ObjectOpError::InvalidParams("chunk 生成未产出任何块".into()));
    };
    Ok(ChunkGenReadback {
        chunks: total_chunks,
        instances: total_instances,
        first_root,
        last_root,
        mesh: mesh.0.id(),
        material: material.0.id(),
    })
}

/// 单个 chunk 的盘点结果。
#[derive(Debug)]
pub struct ChunkCensusRow {
    pub id: u32,
    pub entity: Entity,
    pub origin: Vec3,
    pub extent: f32,
    /// 直接子实体中带 `Mesh3d` 的数量。
    pub members: usize,
}

/// 返回在场 chunk 清单；归属按 `SceneChunk` 记录值读取，不按当前位置重算。
#[must_use]
pub fn chunk_census(world: &mut World) -> Vec<ChunkCensusRow> {
    let mut q = world.query::<(Entity, &SceneChunk, Option<&Children>)>();
    let mut rows: Vec<ChunkCensusRow> = q
        .iter(world)
        .map(|(entity, chunk, children)| ChunkCensusRow {
            id: chunk.id,
            entity,
            origin: chunk.origin,
            extent: chunk.extent,
            members: children
                .map(|c| c.iter().filter(|e| world.get::<Mesh3d>(*e).is_some()).count())
                .unwrap_or(0),
        })
        .collect();
    rows.sort_by_key(|r| r.id);
    rows
}

/// 删除指定 chunk 根及其子树。
///
/// # Errors
/// chunk id 不在场（`ChunkMissing`）。
pub fn despawn_chunk(world: &mut World, id: u32) -> Result<Entity, ObjectOpError> {
    let mut q = world.query::<(Entity, &SceneChunk)>();
    let root = q
        .iter(world)
        .find(|(_, chunk)| chunk.id == id)
        .map(|(entity, _)| entity)
        .ok_or(ObjectOpError::ChunkMissing(id))?;
    world
        .try_despawn(root)
        .map_err(|e| ObjectOpError::InvalidParams(format!("chunk {id} despawn 失败：{e}")))?;
    Ok(root)
}
