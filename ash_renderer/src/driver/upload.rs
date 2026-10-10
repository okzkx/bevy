//! 上传编排(3.2.4 flush_uploads + 3.3.1 贴图段的 bevy 侧系统;4.1.4 起吃账本
//! 脏行):消费 [`InstanceLedger`] 的**待盘点行**(新增/换柄的增量,非全量快照),
//! 把未驻留的 `Assets<Mesh>` 去重转换、未驻留的贴图经材质五槽去重建图,合成
//! **一个批次**送进 GPU(网格进池、贴图进 dedicated memory)。零裸 Vulkan 调用——
//! 资源操作全部走 [`crate::vulkan`] 的 pool/uploader/images 出口。
//!
//! 时序(施工计划 §2 终态表):PostUpdate 账本对账 → **Last:准备+上传提交** →
//! 同帧图形提交(图形侧等票据=draw 挂 ticket 信号量等待,见 frames.rs)。
//! 本系统住 `Last`,由 [`crate::driver::host`] 的 `draw_frame` 链成序(先上传
//! 后画),失败两 Tier:
//! 资产未到货/转换拒绝 = Tier①(跳过重试或 warn 一次),Vulkan/账本失败 =
//! Tier②(error + `AppExit::error()` 优雅退出;提交失败不发布票据,池内
//! bump 游标随下次容量保证自然前移,不留指向未上传数据的账本行)。
//!
//! 增量纪律(4.1):发现侧只把"新增/换柄"的行放进脏集——盘点只过脏行,
//! 静态场景稳态脏集为空,**零盘点零上传**;盘点时已全驻留的行就地清标(空批
//! 不提交)。资产身份去重跨行跨帧:同一身份只有驻留缓存查无才转换上传
//! (3.2.2.2)。贴图身份从脏行的 material 柄出发去 `Assets<StandardMaterial>`
//! 解引用五槽,再按资产身份去重——FlightHelmet 实测:材质 6、贴图槽 24、
//! 去重 15。资产未到货/被拒的行保持脏标下帧重试(与全量快照时代同语义,
//! 只是重试队列从"全部在场行"收敛为"脏行")。
//!
//! 贴图上传形态(显存机制篇判定线):staging 字节 → `vkCmdCopyBufferToImage`
//! (重排由 copy 引擎完成),前后置布局屏障与跨族 release 在 uploader 图像段;
//! `ImageSampler::Default` 按官方 ImagePlugin 全局默认(linear)解析,格式角色
//! 以 `texture_descriptor.format` 的 Srgb 后缀为准,不二次推断。

use std::collections::HashSet;

use bevy::{
    app::OnAppExitSystems,
    asset::AssetId,
    ecs::system::SystemParam,
    image::Image,
    pbr::StandardMaterial,
    prelude::*,
};

use ash_macros::system;

use crate::{
    scene::InstanceLedger,
    vulkan::{
        sampler_key, FramePool, GpuImage, ImageCache, MeshPool, StagingImageCopy, UploadBatch,
        Uploader,
    },
};

/// StandardMaterial 的贴图槽清单(3.1.4 核验同款五槽;M2 口径)。
const TEXTURE_SLOTS: fn(&StandardMaterial) -> [&Option<Handle<Image>>; 5] = |m| {
    [
        &m.base_color_texture,
        &m.metallic_roughness_texture,
        &m.occlusion_texture,
        &m.normal_map_texture,
        &m.emissive_texture,
    ]
};

/// 上传编排插件:资源插入在 [`crate::driver::init`] 的初始化链完成(需要 Device),
/// 本插件只把 [`flush_uploads`] 系统挂进 `Last`(与 draw_frame 的链序由
/// host 插件与排序声明共同保证"先上传后画")。
pub struct AshUploadPlugin;

impl Plugin for AshUploadPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Last,
            flush_uploads
                .run_if(resource_exists::<MeshPool>)
                .run_if(resource_exists::<ImageCache>)
                .run_if(resource_exists::<crate::vulkan::BindlessTables>)
                .before(crate::driver::host::draw_frame)
                .before(OnAppExitSystems),
        );
    }
}

/// 上传状态(跨帧 `Local`):warn 去重 + "增量链首次清账"一次性收账。
#[derive(Default)]
pub(crate) struct UploadState {
    /// 转换拒绝已 warn 过的 mesh 身份(一次一资产,不刷屏)。
    warned: HashSet<AssetId<Mesh>>,
    /// 规格映射拒绝已 warn 过的贴图身份(一次一资产,不刷屏)。
    warned_images: HashSet<AssetId<Image>>,
    /// "脏集首次清空且账本非空"报一次(稳态零盘点零上传的基准证据)。
    settled_logged: bool,
}

/// flush_uploads 的只读参数束(SystemParam,与 3.1.4 CollectData 同款:官方渲染侧
/// 的 Extract 系统同样用参数束装下成排的 Res,让系统签名保持精简)。
#[derive(SystemParam)]
pub(crate) struct UploadData<'w> {
    meshes: Res<'w, Assets<Mesh>>,
    std_materials: Res<'w, Assets<StandardMaterial>>,
    image_assets: Res<'w, Assets<Image>>,
    /// 账本(独占:消费脏行、写上传计量)。
    ledger: ResMut<'w, InstanceLedger>,
    ctx: Res<'w, crate::vulkan::Context>,
    /// 帧槽:池迁移销毁旧池前的"图形最后使用"等待经此接线(4.1.1)。
    frames: Res<'w, FramePool>,
}

/// flush_uploads 本体:账本脏行 → 去重(mesh 直读 + 贴图经材质五槽)→ 转换/建图
/// → 容量保证(可能触发维护迁移)→ 合批提交 → 驻留登记。空批次(无新资产)不提交。
#[system]
pub(crate) fn flush_uploads(
    data: UploadData,
    mut pool: ResMut<MeshPool>,
    mut image_cache: ResMut<ImageCache>,
    mut tables: ResMut<crate::vulkan::BindlessTables>,
    mut uploader: ResMut<Uploader>,
    mut state: Local<UploadState>,
    mut exit: MessageWriter<AppExit>,
) {
    let UploadData {
        ref meshes,
        ref std_materials,
        ref image_assets,
        mut ledger,
        ref ctx,
        ref frames,
    } = data;
    // —— 1) 脏行盘点(4.1.4:上传清单 = 账本脏行):每行查其资产身份的驻留
    // 状态——全驻留的行就地清标;有未驻留身份的行保持脏标(重试),身份进本批。
    // mesh:脏行直读;贴图:material 柄 → 五槽解引用(材质未到货 = 该行保持脏,
    // 下帧重试,与 mesh 异步到货同口径)。
    let mut seen = HashSet::new();
    let mut fresh: Vec<(AssetId<Mesh>, Handle<Mesh>)> = Vec::new();
    let mut img_seen = HashSet::new();
    let mut fresh_images: Vec<(AssetId<Image>, Handle<Image>)> = Vec::new();
    let mut materials_pending = 0usize;
    let mut settled_rows: Vec<Entity> = Vec::new();
    let dirty_entities: Vec<Entity> = ledger.dirty_entities().collect();
    for entity in dirty_entities {
        let Some(row) = ledger.row(entity) else {
            // 行已清(同帧先摘后挂等时序):脏标一并清,不留悬空脏标
            settled_rows.push(entity);
            continue;
        };
        let mut complete = true;
        let mesh_id = row.mesh.id();
        if pool.resident(mesh_id).is_none() {
            complete = false;
            if seen.insert(mesh_id) {
                fresh.push((mesh_id, row.mesh.clone()));
            }
        }
        match std_materials.get(&row.material) {
            Some(material) => {
                for handle in TEXTURE_SLOTS(material).into_iter().flatten() {
                    let id = handle.id();
                    if image_cache.resident(id).is_none() {
                        complete = false;
                        if img_seen.insert(id) {
                            fresh_images.push((id, handle.clone()));
                        }
                    }
                }
            }
            None => {
                // 材质未到货：整行保持脏标下帧重试
                complete = false;
                materials_pending += 1;
            }
        }
        if complete {
            settled_rows.push(entity);
        }
    }
    // 清标不依赖后续步骤成败:行"全驻留"是账本事实,与提交无关
    for entity in settled_rows {
        ledger.clear_dirty(entity);
    }
    // —— 1.5) 收账:脏集清空且账本非空 = 全部资产驻留,报一次(稳态基准证据:
    // 之后每帧盘点 0 行、上传 0B,处理成本只剩 DrawList 读账本)。放空批
    // early-return 之前——清空脏集的那帧多半就是空批帧,放在后面永远打不出。
    if ledger.dirty_count() == 0 && !ledger.is_empty() && !state.settled_logged {
        state.settled_logged = true;
        info!(
            "增量链首次清账:账本 {} 行全部资产驻留,脏集空——稳态帧零盘点零上传(4.1 判定线基准)",
            ledger.len()
        );
    }
    // —— 2) 转换/规格映射(Tier①:未到货跳过重试,拒绝 warn 一次)——
    let mut converted: Vec<(AssetId<Mesh>, crate::vulkan::ConvertedMesh)> = Vec::new();
    let mut pending = 0usize;
    for (id, handle) in fresh {
        match meshes.get(&handle) {
            None => pending += 1, // 异步加载未到货,下帧脏行重试
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
    let mut image_specs: Vec<(AssetId<Image>, Handle<Image>, crate::vulkan::ImageSpec)> =
        Vec::new();
    let mut pending_images = 0usize;
    for (id, handle) in fresh_images {
        match image_assets.get(&handle) {
            None => pending_images += 1, // 异步加载未到货,下帧脏行重试
            Some(image) => match crate::vulkan::image_spec(image) {
                Ok(spec) => image_specs.push((id, handle, spec)),
                Err(e) => {
                    if state.warned_images.insert(id) {
                        warn!("贴图 {id:?} 规格拒绝({e}),本步不采样该资产,帧循环照常");
                    }
                }
            },
        }
    }
    if converted.is_empty() && image_specs.is_empty() {
        return; // 空批次不提交(3.2.4.1):未到货/全拒绝/全已驻留都走这里
    }
    // —— 3) 容量保证(初次懒建或维护迁移,等待/迁移在池内分账;迁移销毁旧池
    // 前经 frames.wait_all_inflight 等"图形最后使用",4.1.1)——
    let (vertex_bytes, index_bytes) = converted.iter().fold((0u64, 0u64), |(v, i), (_, c)| {
        (v + c.vertices.len() as u64, i + c.indices.len() as u64)
    });
    if let Err(e) = pool.ensure_capacity(
        &mut uploader,
        vertex_bytes,
        index_bytes,
        &mut || frames.wait_all_inflight(),
    ) {
        error!("池容量保证失败,上传链无法继续,优雅退出: {e}");
        exit.write(AppExit::error());
        return;
    }
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
    // —— 4a) 建图(Tier②):每张贴图 VkImage+dedicated memory+view+sampler——
    // 创建先于排批(拷贝目标必须已存在);提交失败走 Tier② 退出,局部变量随
    // 返回 Drop,不留半建对象。跨族设备:CONCURRENT 双族(3.4 定案,与池同款)
    let image_sharing: Vec<u32> = if ctx.transfer_queue_family_index != ctx.queue_family_index {
        vec![ctx.transfer_queue_family_index, ctx.queue_family_index]
    } else {
        Vec::new()
    };
    let mut gpu_images: Vec<(AssetId<Image>, GpuImage)> = Vec::with_capacity(image_specs.len());
    for (id, _, spec) in &image_specs {
        match GpuImage::create(
            &ctx.device,
            &ctx.instance,
            ctx.physical_device,
            ctx.memory_contract(),
            spec,
            &image_sharing,
        ) {
            Ok(g) => gpu_images.push((*id, g)),
            Err(e) => {
                error!("贴图创建失败,上传链无法继续,优雅退出: {e}");
                exit.write(AppExit::error());
                return;
            }
        }
    }
    // —— 4b) 排批:staging 连续排布(网格两段 + 每贴图一段);分配必在
    // ensure_capacity 之后——"不写越界"的顺序契约——
    let image_bytes: u64 = image_specs
        .iter()
        .map(|(_, handle, _)| {
            image_assets
                .get(handle)
                .and_then(|img| img.data.as_ref())
                .map_or(0, Vec::len) as u64
        })
        .sum();
    let mut staging = Vec::with_capacity((vertex_bytes + index_bytes + image_bytes) as usize);
    let mut uploads = Vec::with_capacity(converted.len() * 2);
    let mut image_uploads = Vec::with_capacity(image_specs.len());
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
    for ((id, handle, spec), (_, gpu)) in image_specs.iter().zip(&gpu_images) {
        // 贴图字节直取:上一阶段已验 Some;同一次系统调用内资产不变,状态矛盾
        // 按 Tier② 报(与"容量保证后池缺席"同级,不 panic)
        let Some(image) = image_assets.get(handle) else {
            error!("贴图 {id:?} 在规格映射与排批之间消失,状态矛盾,优雅退出");
            exit.write(AppExit::error());
            return;
        };
        let Some(bytes) = image.data.as_ref() else {
            error!("贴图 {id:?} 像素数据消失,状态矛盾,优雅退出");
            exit.write(AppExit::error());
            return;
        };
        let src_offset = staging.len() as u64;
        staging.extend_from_slice(bytes);
        image_uploads.push(StagingImageCopy {
            image: gpu.image(),
            src_offset,
            width: spec.width,
            height: spec.height,
        });
    }
    // 跨族让渡已随 3.4 定案退役:图像与池一样走 CONCURRENT 双族共享,不再
    // 记录 release/待办 acquire——上传批次的图像收尾是纯布局转换(IGNORED 族),
    // 跨队列内存可见性由图形提交等票据信号量收口(见 frames.rs 提交段注释)。
    // EXCLUSIVE 的 release/acquire 成对语义保留在 image_probe 组 C 作机制实证。
    // —— 5) 一次合批 = 一个 staging 范围 + 一次 transfer 提交(3.2.4/3.3.1)——
    let batch = UploadBatch {
        staging,
        uploads,
        image_uploads,
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
            // —— 3.3.4 槽位发布:提交成功 → 分槽(采样器按功能键去重) → 驻留登记
            // 带槽位。只写 free list 的新槽,覆盖竞态在结构上不存在;采样 draw 的
            // "上传完成才可使用"由票据承担(图形提交挂 draw 的 ticket 信号量等待)。
            // 发布失败 = 槽容量耗尽,按 Tier② 冒泡(施工计划:不静默截断)
            let mut sampler_slots_hit = HashSet::new();
            for ((id, _, spec), (gid, gpu)) in image_specs.iter().zip(gpu_images) {
                debug_assert_eq!(*id, gid, "image_specs 与 gpu_images 同序构建,错位即内部矛盾");
                let slots =
                    match tables.publish(gpu.view(), gpu.sampler(), &sampler_key(&spec.sampler)) {
                        Ok(s) => s,
                        Err(e) => {
                            error!("贴图 {id:?} 槽位发布失败,上传链无法继续,优雅退出: {e}");
                            exit.write(AppExit::error());
                            return;
                        }
                    };
                sampler_slots_hit.insert(slots.sampler);
                image_cache.commit(*id, gpu, ticket, slots);
            }
            let srgb_count = image_specs
                .iter()
                .filter(|(_, _, spec)| spec.srgb_role())
                .count();
            // 4.1.4 计量:上传侧累计进账本(判定线的字节/批次口径)
            let batch_bytes = vertex_bytes + index_bytes + image_bytes;
            let dirty_after = ledger.dirty_count();
            let stats = &mut ledger.stats;
            stats.upload_batches_total += 1;
            stats.upload_bytes_total += batch_bytes;
            stats.last_batch_bytes = batch_bytes;
            stats.dirty_rows = dirty_after;
            info!(
                "上传批次 #{ticket}:mesh {planned_count} 个(顶点 {vertex_bytes}B / 索引 {index_bytes}B)\
                 + 贴图 {} 张({image_bytes}B,sRGB 角色 {srgb_count} / 线性 {});\
                 槽位发布:纹理槽 {}/{}(含 fallback 0 号)、采样器槽 {} 种(本批涉及 {new_sampler});\
                 未到货跳过 mesh {pending} / 贴图 {pending_images}(材质缺 {materials_pending});\
                 已驻留 mesh {} / 贴图 {}",
                image_specs.len(),
                image_specs.len() - srgb_count,
                tables.used_texture_slots(),
                tables.capacity(),
                tables.used_sampler_slots(),
                pool.resident_count(),
                image_cache.resident_count(),
                new_sampler = sampler_slots_hit.len(),
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
    // —— 6) 收账:脏集清空且账本非空 = 全部资产驻留,报一次(稳态基准证据:
    // 之后每帧盘点 0 行、上传 0B,处理成本只剩 DrawList 读账本)
    if ledger.dirty_count() == 0 && !ledger.is_empty() && !state.settled_logged {
        state.settled_logged = true;
        info!(
            "增量链首次清账:账本 {} 行全部资产驻留,脏集空——稳态帧零盘点零上传(4.1 判定线基准)",
            ledger.len()
        );
    }
}
