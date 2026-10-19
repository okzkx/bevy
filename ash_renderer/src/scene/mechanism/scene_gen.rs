//! 常驻大场景生成器（5.4.1/5.4.2）：程序化铺大量实例——共享既有资产的柄、
//! 规模参数化、随机种子固定可复现，走 4.1 账本链路（批量 spawn 后
//! Changed 通道自动入行，驻留资产零重传）；chunk 身份（空间网格，待决 2）
//! 与整块增删（5.4.2）。
//!
//! 定位：5.1～5.3 的 FlightHelmet 小场景靠 BRP 手工逐个 spawn；5.4 起场景
//! 由本生成器铺。形态钉明（待决 1）：运行期 spawn——不生成 glTF 走加载链，
//! spawn 克隆与 [`crate::scene::mechanism::spawn_primitive_clone`] 同语义
//!（柄复制 = 共享资产），账本/上传/GPU 表零新通道；BRP 薄壳在
//! [`crate::remote::generate`]，egui 面板将来共用本层。
//!
//! 复现纪律：位置抖动是纯函数——`f(seed, index)`（splitmix64 终态散列），
//! 不持顺序状态，同参数重跑逐实例同位。生成器本身规模无关：count/spacing
//! 只是参数，10 万档（5.5）与 4096 验证档（5.4 默认，待决 7）同一代码路径。

use super::ObjectOpError;
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
    /// 位置抖动幅度（世界单位，[0, jitter) 均匀偏移 x/z）；0 = 整齐网格。
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

/// 批量克隆铺实例：网格 + 种子抖动定位，Name 编号便于树内辨认。
/// 柄复制 = 共享资产（驻留缓存按 AssetId 去重 → 存量资产零上传），
/// 无父独立成树（chunk 组织是 5.4.2 的层，本函数不管）。
///
/// # Errors
/// 源实体缺席/非 primitive/缺 Transform；参数为 0（count/columns）或
/// spacing 非正（协议面校验，不产出半批实体——spawn 前先全量校验）。
pub fn generate_instances(
    world: &mut World,
    p: SceneGenParams,
) -> Result<SceneGenReadback, ObjectOpError> {
    if p.count == 0 || p.columns == 0 {
        return Err(ObjectOpError::NothingToDo);
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
    // u32→f32 用 as 转换（u32 精度上界 2^24 内无损，生成器网格坐标远低于）
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

/// 纯函数位置散列：splitmix64 终态，输出 [0,1)。`channel` 区分 x/z 两条流
///——同 (seed,i) 不同 channel 独立。无顺序状态 = 同参数重跑同位。
fn hash01(seed: u64, index: u32, channel: u64) -> f32 {
    let mut z = seed
        ^ u64::from(index).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ channel.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // 取高 24 位归一化（低位的乘法痕迹更重，高 24 位即浮点均匀性的常用口径）
    (z >> 40) as f32 / (1u64 << 24) as f32
}

// ===== 5.4.2 chunk 身份与整块增删 =====

/// chunk 身份组件：生成期空间归属，挂在 chunk 根实体上、实例为其子实体。
///
/// 两个钉明（5.4 计划待决 3/8）：
/// - **身份落在实体组件**：Bevy 侧可查（census/despawn 按 id 寻址）、整块
///   despawn 复用 despawn 树递归语义（4.1 整树清账的批量形态）；
/// - **chunk 不进 GPU 侧**：GPU 不知道 chunk（不进对象表/标记环/命令环），
///   本段剔除仍逐槽；chunk 是 CPU 侧组织与增删粒度。
///
/// 归属不随运行期更新：对象被 set_transform 移出块边界仍是生成块的成员
///（chunk = 生成期组织，运行期归属更新语义另立账）。
#[derive(Component, Debug, Clone, Copy)]
pub struct SceneChunk {
    /// 生成期编号（cz × 每轴块数 + cx）；census/despawn 按 id 寻址。
    pub id: u32,
    /// 块原点（世界坐标，根实体的 translation）。
    pub origin: Vec3,
    /// 块边长（成员网格边数 × spacing；相邻块原点相距 extent，块间不重叠）。
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
    /// 位置抖动幅度；0 = 整齐网格。
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

/// chunk 化批量生成：空间网格分块（块边界即空间边界，判定线 1 的
/// "块边界可观察"），块内小网格铺实例，整块 = 一个根实体 + 子树。
/// 抖动散列用跨块平坦 index——同参数重跑逐实例同位。
///
/// # Errors
/// 源实体缺席/非 primitive/缺 Transform；参数为 0 或 spacing 非正。
pub fn generate_chunks(
    world: &mut World,
    p: ChunkGenParams,
) -> Result<ChunkGenReadback, ObjectOpError> {
    if p.chunk_grid == 0 || p.instances_per_chunk == 0 {
        return Err(ObjectOpError::NothingToDo);
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
    // 块内实例网格：列数 = ceil(sqrt(ipc))，块边长 = 列数 × spacing
    let cols = (p.instances_per_chunk as f32).sqrt().ceil() as u32;
    let extent = cols as f32 * p.spacing;
    let total_chunks = p.chunk_grid * p.chunk_grid;
    let total_instances = total_chunks * p.instances_per_chunk;
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
                // 实例位置由生成器定（块原点经父链传播，局部坐标不含它）
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
    Ok(ChunkGenReadback {
        chunks: total_chunks,
        instances: total_instances,
        first_root: first_root.expect("chunk_grid>0 保证至少一块"),
        last_root: last_root.expect("同上"),
        mesh: mesh.0.id(),
        material: material.0.id(),
    })
}

/// chunk 盘点行：census 的逐块读数（判定线 1 的块边界读数口）。
#[derive(Debug)]
pub struct ChunkCensusRow {
    pub id: u32,
    pub entity: Entity,
    pub origin: Vec3,
    pub extent: f32,
    /// 子树内可渲染 primitive 数（chunk 成员；非 primitive 子实体不计入）。
    pub members: usize,
}

/// chunk 盘点：在场块清单（id/实体/原点/边长/成员数）。**归属读数 =
/// 生成期账**（SceneChunk 组件原样报出，不按当前位置重算）。
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

/// 整块删除：按 id 找根 → 根连子树递归 despawn（官方递归语义）。每个
/// primitive 子实体的组件移除各自发 Removed 事件，账本逐行清账——4.1
/// 整树清账的批量形态，增量链路天然覆盖（判定线 2 的走账前提）。
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
