//! 上传编排(3.2.4 flush_uploads 的 bevy 侧系统):消费 [`CollectedScene`] 快照,
//! 把未驻留的 `Assets<Mesh>` 去重转换后合批送进 GPU 大池。零裸 Vulkan 调用——
//! 资源操作全部走 [`crate::vulkan`] 的 pool/uploader 出口。
//!
//! 时序(施工计划 §2 终态表):PostUpdate 采集 → **Last:准备+上传提交** →
//! 同帧图形提交(图形等票据是 3.4 接 draw 时的事)。本系统住 `Last`,由
//! [`crate::host`] 与 `draw_frame` 链成序(先上传后画),失败两 Tier:
//! 资产未到货/转换拒绝 = Tier①(跳过重试或 warn 一次),Vulkan/账本失败 =
//! Tier②(error + `AppExit::error()` 优雅退出;提交失败不发布票据,池内
//! bump 游标随下次容量保证自然前移,不留指向未上传数据的账本行)。
//!
//! 去重纪律:每帧快照都会带着同一批 mesh 柄来——只有"驻留缓存查无此身份"
//! 的才转换上传(3.2.2.2);重复快照零重复上传,批次日志可证。

use std::collections::HashSet;

use bevy::{app::OnAppExitSystems, prelude::*};

use crate::{
    scene::CollectedScene,
    vulkan::{MeshPool, UploadBatch, Uploader},
};

/// 上传编排插件:资源插入在 [`crate::host`] 的初始化链完成(需要 Device),
/// 本插件只把 [`flush_uploads`] 系统挂进 `Last`(与 draw_frame 的链序由
/// host 定义,保证"先上传后画")。
pub struct AshUploadPlugin;

impl Plugin for AshUploadPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Last,
            flush_uploads
                .run_if(resource_exists::<MeshPool>)
                .before(crate::host::draw_frame)
                .before(OnAppExitSystems),
        );
    }
}

/// 上传状态(跨帧 `Local`):warn 去重 + "全部驻留"一次性收账。
#[derive(Default)]
pub(crate) struct UploadState {
    /// 转换拒绝已 warn 过的资产身份(一次一资产,不刷屏)。
    warned: HashSet<AssetId<Mesh>>,
    /// "静态资产全部驻留"报过一次即歇。
    settled_logged: bool,
}

/// flush_uploads 本体:快照 → 去重 → 转换 → 容量保证(可能触发维护)→
/// 合批提交 → 驻留登记。空批次(无新资产)不提交。
pub(crate) fn flush_uploads(
    meshes: Res<Assets<Mesh>>,
    scene: Res<CollectedScene>,
    mut pool: ResMut<MeshPool>,
    mut uploader: ResMut<Uploader>,
    mut state: Local<UploadState>,
    mut exit: MessageWriter<AppExit>,
) {
    // —— 1) 快照去重:本帧在场、未驻留的 mesh 身份(保序去重)——
    let mut seen = HashSet::new();
    let mut fresh: Vec<(AssetId<Mesh>, Handle<Mesh>)> = Vec::new();
    for row in &scene.primitives {
        let id = row.mesh.id();
        if pool.resident(id).is_some() || !seen.insert(id) {
            continue;
        }
        fresh.push((id, row.mesh.clone()));
    }
    // —— 2) 转换(Tier①:未到货跳过重试,拒绝 warn 一次)——
    let mut converted: Vec<(AssetId<Mesh>, crate::vulkan::ConvertedMesh)> = Vec::new();
    let mut pending = 0usize;
    for (id, handle) in fresh {
        match meshes.get(&handle) {
            None => pending += 1, // 异步加载未到货,下帧快照再来
            Some(mesh) => match crate::vulkan::convert_mesh(mesh) {
                Ok(c) => converted.push((id, c)),
                Err(e) => {
                    if state.warned.insert(id) {
                        warn!("mesh {id:?} 转换拒绝({e}),本步不绘制该资产,帧循环照常");
                    }
                }
            },
        }
    }
    if converted.is_empty() {
        return; // 空批次不提交(3.2.4.1):未到货/全拒绝/全已驻留都走这里
    }
    // —— 3) 容量保证(初次懒建或维护迁移,等待/迁移在池内分账)——
    let (vertex_bytes, index_bytes) = converted.iter().fold((0u64, 0u64), |(v, i), (_, c)| {
        (v + c.vertices.len() as u64, i + c.indices.len() as u64)
    });
    if let Err(e) = pool.ensure_capacity(&mut uploader, vertex_bytes, index_bytes) {
        error!("池容量保证失败,上传链无法继续,优雅退出: {e}");
        exit.write(AppExit::error());
        return;
    }
    // —— 4) 排批:staging 连续排布,每资产两段拷贝(顶点/索引)。分配必在
    // ensure_capacity 之后——"不写越界"的顺序契约——
    let Some(vertex_buffer) = pool.vertex_buffer() else {
        error!("容量保证后顶点池仍缺席,状态矛盾,优雅退出");
        exit.write(AppExit::error());
        return;
    };
    let Some(index_buffer) = pool.index_buffer() else {
        error!("容量保证后索引池仍缺席,状态矛盾,优雅退出");
        exit.write(AppExit::error());
        return;
    };
    let mut staging = Vec::with_capacity((vertex_bytes + index_bytes) as usize);
    let mut uploads = Vec::with_capacity(converted.len() * 2);
    let mut planned: Vec<(
        AssetId<Mesh>,
        crate::vulkan::ConvertedMesh,
        crate::vulkan::PoolRange,
        crate::vulkan::PoolRange,
    )> = Vec::with_capacity(converted.len());
    for (id, c) in converted {
        let (vertex, index) = pool.alloc(c.vertices.len() as u64, c.indices.len() as u64);
        let v_src = staging.len() as u64;
        staging.extend_from_slice(&c.vertices);
        uploads.push(crate::vulkan::StagingCopy {
            dst: vertex_buffer,
            src_offset: v_src,
            dst_offset: vertex.offset,
            size: vertex.size,
        });
        let i_src = staging.len() as u64;
        staging.extend_from_slice(&c.indices);
        uploads.push(crate::vulkan::StagingCopy {
            dst: index_buffer,
            src_offset: i_src,
            dst_offset: index.offset,
            size: index.size,
        });
        planned.push((id, c, vertex, index));
    }
    // —— 5) 一次合批 = 一个 staging 范围 + 一次 transfer 提交(3.2.4)——
    let batch = UploadBatch {
        staging,
        uploads,
        ..Default::default()
    };
    match uploader.submit_batch(batch) {
        Ok(Some(ticket)) => {
            // 驻留登记只在提交成功之后:失败路径不会留下指向未上传数据的账本行
            let planned_count = planned.len();
            for (id, c, vertex, index) in planned {
                if let Err(e) =
                    pool.commit(id, vertex, index, c.vertex_count, c.index_count, ticket)
                {
                    error!("驻留登记失败,上传链无法继续,优雅退出: {e}");
                    exit.write(AppExit::error());
                    return;
                }
            }
            info!(
                "上传批次 #{ticket}:资产 {planned_count} 个,顶点 {vertex_bytes}B / 索引 {index_bytes}B 进池;未到货跳过 {pending},已驻留累计 {}",
                pool.resident_count(),
            );
        }
        // staging 非空的批次必有提交;空批已在入口 return。此分支按内部契约
        // 矛盾处理(两 Tier 的 Tier②),不 panic
        Ok(None) => {
            error!("内部矛盾:staging 非空的批次未产生提交,优雅退出");
            exit.write(AppExit::error());
        }
        Err(e) => {
            error!("transfer 提交失败,上传链无法继续,优雅退出(未发布票据): {e}");
            exit.write(AppExit::error());
        }
    }
    // —— 6) 收账:快照里的身份全部驻留时报一次——
    let unique = scene
        .primitives
        .iter()
        .map(|row| row.mesh.id())
        .collect::<HashSet<_>>()
        .len();
    if unique > 0 && pool.resident_count() >= unique && !state.settled_logged {
        state.settled_logged = true;
        info!(
            "静态资产全部驻留:{unique} 个 mesh,重复快照不再上传(判定线:异步到货/重复快照零重复上传)"
        );
    }
}
