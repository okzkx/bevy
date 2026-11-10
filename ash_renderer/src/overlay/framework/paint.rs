//! overlay 绘制段：图集 CPU 镜像 → 整图重传新槽、镶嵌写 UI 顶点环，
//! `paint_overlay` 把 [`EguiFrame`] 变成 [`UiPaint`]。GPU 侧账面
//!（[`AtlasGpu`]/[`UiVertexRing`]）住 render 层，本模块只做 egui 侧产出。
//!
//! 图集路线：epaint 图集增量是 dirty-rect 局部补丁，而 uploader 图像段只支持整图
//! (0,0) 拷贝（局部拷贝要引入 graphics→transfer 反向同步），故折进 CPU 镜像 →
//! 整图重传全新 GpuImage → publish 新槽 → 旧代入 graveyard。
//! 用户贴图（非图集纹理，`Managed(n>0)`）走独立路：fold 收整图增量 → 绘制段
//! 每张建独立 GpuImage → publish 自己的槽位（[`UiDrawCall`] 逐 draw 带槽位）→
//! free 时销毁。纹理选项按 LINEAR 滤波发布（用户贴图当前只服务预览小图，够用）。
//! 坑：镜像必须在 Update 的 pass 出口折入（`AtlasMirror::fold`），不能随 EguiFrame
//! 跨系统携带——`TexturesDelta` Drop 时要求增量已消费，且最小化帧 Update 照跑而
//! draw_frame 整帧让路，跨系统携带会被下帧覆写丢失。
//!
//! 时序：`paint_overlay` 必须在 draw_frame 组装 RenderFeatures（`upload_wait` 快照）之前
//! 调用——图集批的票据要落进本帧图形提交的等待值。

use ash::vk;
use bevy::{ecs::system::SystemParam, image::ImageSamplerDescriptor, prelude::*};

use crate::{
    common::{
        error::{Result, VulkanError},
        syntax::{OnceLog, ResultTierExt},
    },
    render::{
        sampler_key, AtlasGpu, BindlessTextureTable, Context, GpuImage, ImageSpec, SlotBinding,
        StagingImageCopy, Ticket, UiDrawCall, UiPaint, UiVertex, UiVertexRing, UploadBatch,
        Uploader,
    },
};

use super::ui::{EguiFrame, EguiLayer, EguiState};

/// 字体图集的 CPU 镜像（Update 侧由 [`super::ui`] 折入，绘制段按代数差整传）。
#[derive(Resource, Default)]
pub struct AtlasMirror {
    /// 镜像本体：首个整图 delta 落位，此后 dirty-rect 折入。
    image: Option<egui::epaint::ColorImage>,
    /// 镜像代数：任何折入 +1。GPU 段"已传代数 < 镜像代数"即需整传。
    generation: u64,
    /// 用户贴图（非图集纹理）的整图增量，等绘制段上传发布成独立槽位。
    user_sets: Vec<(egui::TextureId, egui::epaint::ColorImage)>,
    /// 用户贴图的释放清单（egui 纹理句柄被丢弃），等绘制段销毁对应 GPU 资源。
    user_frees: Vec<egui::TextureId>,
    /// 局部补丁先于首个整图的告警去重（理论不发生）。
    warned_orphan: OnceLog,
}

impl AtlasMirror {
    /// 折入一帧的纹理增量（pass 出口调用，纯 CPU）。`Managed(0)` = 字体图集；
    /// 其余 = 用户贴图：整图增量收进 [`Self::user_sets`]、free 收进
    /// [`Self::user_frees`]，由绘制段上传发布成独立槽位（局部补丁不实现——
    /// `load_texture` 产的纹理只走整图，warn 一次跳过）。折完 `clear`——
    /// Drop 审查要求增量被消费。
    pub(crate) fn fold(&mut self, delta: &mut egui::TexturesDelta) {
        for (id, deltas) in &delta.set {
            for d in deltas {
                // ImageData 仅 Color 一变体
                let egui::epaint::ImageData::Color(patch) = &d.image;
                if *id != egui::TextureId::Managed(0) {
                    match d.pos {
                        None => self.user_sets.push((*id, (**patch).clone())),
                        Some(_) => warn!("egui 用户贴图 {id:?} 局部补丁不实现，增量跳过"),
                    }
                    continue;
                }
                match d.pos {
                    None => {
                        // 整图：全新图集落位（增长/重建）
                        self.image = Some((**patch).clone());
                        self.generation += 1;
                    }
                    Some([x0, y0]) => match &mut self.image {
                        Some(mirror) => {
                            blit(mirror, patch, x0, y0);
                            self.generation += 1;
                        }
                        None => self.warned_orphan.log(|| {
                            warn!(
                                "图集局部补丁先于首个整图增量（镜像缺席），本补丁跳过——\
                                 下个整图增量自愈"
                            );
                        }),
                    },
                }
            }
        }
        self.user_frees.extend(delta.free.drain());
        delta.clear();
    }

    /// 取走积累的用户贴图增量（绘制段每帧调用；图集本体走代数差整传，不经此路）。
    pub(crate) fn take_user_deltas(
        &mut self,
    ) -> (
        Vec<(egui::TextureId, egui::epaint::ColorImage)>,
        Vec<egui::TextureId>,
    ) {
        (
            std::mem::take(&mut self.user_sets),
            std::mem::take(&mut self.user_frees),
        )
    }

    /// 镜像本体与代数（整传判定与日志的证据口）。
    pub(crate) fn snapshot(&self) -> (Option<&egui::epaint::ColorImage>, u64) {
        (self.image.as_ref(), self.generation)
    }
}

/// dirty-rect 折入：patch 行主序拷进 mirror 的 (x0,y0) 起点，越界部分按行交集
/// 裁剪（egui 保证在界内，防御性裁剪不 panic）。
fn blit(
    mirror: &mut egui::epaint::ColorImage,
    patch: &egui::epaint::ColorImage,
    x0: usize,
    y0: usize,
) {
    let [mw, mh] = mirror.size;
    let [pw, ph] = patch.size;
    let copy_w = pw.min(mw.saturating_sub(x0));
    for y in 0..ph {
        let my = y0 + y;
        if my >= mh {
            break;
        }
        let src = &patch.pixels[y * pw..y * pw + copy_w];
        mirror.pixels[my * mw + x0..my * mw + x0 + copy_w].copy_from_slice(src);
    }
}

/// paint_overlay 的资源束（draw_frame 的系统参数）。
#[derive(SystemParam)]
pub(crate) struct UiDrawData<'w> {
    pub(crate) egui: Res<'w, EguiState>,
    pub(crate) frame: ResMut<'w, EguiFrame>,
    pub(crate) mirror: ResMut<'w, AtlasMirror>,
    pub(crate) gpu: ResMut<'w, AtlasGpu>,
    pub(crate) ring: ResMut<'w, UiVertexRing>,
}

/// paint_overlay 的跨帧状态（draw_frame 的 Local）：一次性收账/告警去重 + 镶嵌缓存。
#[derive(Default)]
pub(crate) struct OverlayLogState {
    warned_overflow: OnceLog,
    warned_callback: OnceLog,
    warned_no_atlas: OnceLog,
    /// 用户贴图槽缺席/不支持的 id（槽未就绪只跳当帧 clip，非内部矛盾）
    warned_user_tex: OnceLog,
    /// 首次用户贴图整传收账
    user_uploaded: OnceLog,
    painted_logged: OnceLog,
    /// 两层镶嵌缓存：(shapes 代数, 镶嵌产物)——代数未变的帧直接复用，按 pass
    /// 代数判等零比较成本（逐形状比较会占 tessellate 一半成本）。
    panel_meshes: (u64, Vec<egui::epaint::ClippedPrimitive>),
    gizmo_meshes: (u64, Vec<egui::epaint::ClippedPrimitive>),
}

/// overlay 绘制段：用户贴图增量消化 → 图集整传（有新代时）→ 镶嵌 → 顶点环写入
/// → [`UiPaint`]。在 draw_frame 内、RenderFeatures 组装（upload_wait 快照）之前调用。
/// 失败 = 绘制链状态已不可信（图集建图/提交/发布、顶点环写入），按致命错误冒泡。
#[expect(
    clippy::too_many_arguments,
    reason = "bevy 系统侧拆出的独立函数：资源逐项传入是 draw_frame 依赖注入的延续，非函数设计味道"
)]
pub(crate) fn paint_overlay(
    ctx: &Context,
    egui: &EguiState,
    frame: &mut EguiFrame,
    mirror: &mut AtlasMirror,
    gpu: &mut AtlasGpu,
    ring: &mut UiVertexRing,
    tables: &mut BindlessTextureTable,
    uploader: &mut Uploader,
    slot: usize,
    screen_px: vk::Extent2D,
    log: &mut OverlayLogState,
) -> Result<Option<UiPaint>, BevyError> {
    paint_overlay_steps(
        ctx, egui, frame, mirror, gpu, ring, tables, uploader, slot, screen_px, log,
    )
    .or_fatal("overlay 绘制段失败，渲染链无法继续")
}

#[expect(
    clippy::too_many_arguments,
    reason = "同 paint_overlay：参数表是原函数体，不为消提示而拆分"
)]
fn paint_overlay_steps(
    ctx: &Context,
    egui: &EguiState,
    frame: &mut EguiFrame,
    mirror: &mut AtlasMirror,
    gpu: &mut AtlasGpu,
    ring: &mut UiVertexRing,
    tables: &mut BindlessTextureTable,
    uploader: &mut Uploader,
    slot: usize,
    screen_px: vk::Extent2D,
    log: &mut OverlayLogState,
) -> Result<Option<UiPaint>> {
    // —— 0) 用户贴图增量：新整图各建一张独立 GpuImage 发布成独立槽位（键 =
    // egui Managed 序号），free 的销毁进 graveyard。票据与图集同路，落进本帧
    // upload_wait 快照。
    let (user_sets, user_frees) = mirror.take_user_deltas();
    for id in user_frees {
        if let Some(key) = managed_key(id, log) {
            gpu.free_user(tables, key);
        }
    }
    for (id, image) in user_sets {
        let Some(key) = managed_key(id, log) else {
            continue;
        };
        let (gpu_image, ticket, slots) = upload_user_texture(ctx, tables, uploader, &image)?;
        gpu.install_user(tables, key, gpu_image, ticket, slots);
        log.user_uploaded.log(|| {
            info!(
                "用户贴图整传 #{ticket}：{}×{}，纹理槽 {}/{}",
                image.size[0], image.size[1], slots.texture, slots.sampler
            );
        });
    }
    // —— 1) 图集整传：镜像代数领先已传代数 → 全新 GpuImage + 独立批 + publish 新槽 ——
    let (mirror_image, mirror_generation) = mirror.snapshot();
    if gpu.generation() < mirror_generation {
        // 只有整图增量会落位镜像并加代，本体缺席是内部矛盾
        let image = mirror_image
            .ok_or_else(|| VulkanError::Init("图集镜像代数领先但本体缺席（内部矛盾）".into()))?;
        // 图集是 gamma 域 RGBA8、LINEAR 滤波；非 sRGB 格式 = 采样不解码，
        // 色彩约定见 overlay_draw.wgsl
        let spec = ImageSpec {
            width: image.size[0] as u32,
            height: image.size[1] as u32,
            format: vk::Format::R8G8B8A8_UNORM,
            // 图集无 mip 需求，单级
            mip_levels: 1,
            sampler: ImageSamplerDescriptor::linear(),
        };
        // 图像与池同款 CONCURRENT 双族（transfer 写 + graphics 读）
        let sharing: Vec<u32> = if ctx.transfer_queue_family_index != ctx.queue_family_index {
            vec![ctx.transfer_queue_family_index, ctx.queue_family_index]
        } else {
            Vec::new()
        };
        let gpu_image = GpuImage::create(
            &ctx.device,
            &ctx.instance,
            ctx.physical_device,
            ctx.memory_contract(),
            &spec,
            &sharing,
        )?;
        // 镜像像素字节直取：ColorImage 紧密行主序 RGBA8，与 staging 期望一致
        const _: () = assert!(std::mem::size_of::<egui::Color32>() == 4);
        let staging = unsafe {
            std::slice::from_raw_parts(image.pixels.as_ptr().cast(), image.pixels.len() * 4)
        }
        .to_vec();
        let batch = UploadBatch {
            staging,
            image_uploads: vec![StagingImageCopy {
                image: gpu_image.image(),
                src_offset: 0,
                width: spec.width,
                height: spec.height,
                mip_levels: 1,
                format: spec.format,
            }],
            ..Default::default()
        };
        // staging 非空的批次必有提交（uploader 契约），None 是内部矛盾
        let ticket = uploader.submit_batch(batch)?.ok_or_else(|| {
            VulkanError::Init("图集批未产生提交（staging 非空，内部矛盾）".into())
        })?;
        // 槽位发布在提交成功后（失败不留指向未上传数据的槽）；只写新槽（不覆盖在飞消费的旧槽）
        let slots = tables.publish(gpu_image.view(), &sampler_key(&spec.sampler))?;
        gpu.install(tables, gpu_image, ticket, slots, mirror_generation);
        info!(
            "图集整传 #{ticket}：{}×{}（第 {mirror_generation} 代），纹理槽 {}/{}；\
             旧代入 graveyard（当前 {} 张待排空）",
            spec.width,
            spec.height,
            slots.texture,
            tables.capacity(),
            gpu.graveyard_len(),
        );
    }
    // —— 2) 镶嵌：代数未变的层直接复用缓存产物（pass 帧取走当帧 shapes 重镶嵌）——
    let ppp = frame.pixels_per_point;
    tessellate_layer(&egui.ctx, &mut frame.panel, &mut log.panel_meshes, ppp);
    tessellate_layer(&egui.ctx, &mut frame.gizmo, &mut log.gizmo_meshes, ppp);
    // —— 3) 摊平：面板层先、gizmo 层后（gizmo 压顶）。逐 clip 解析纹理槽位：
    // Managed(0) 走图集，其余查用户贴图表；图集代缺席 = 字体 draw 无处采样 ——
    let atlas = gpu.current_slots();
    if atlas.is_none() {
        log.warned_no_atlas
            .log(|| warn!("图集 GPU 代缺席（镜像从未折入整图增量），字体 draw 丢弃"));
    }
    let mut stream = UiStream::default();
    for meshes in [&log.panel_meshes.1, &log.gizmo_meshes.1] {
        stream.append(
            meshes,
            ppp,
            screen_px,
            atlas,
            gpu,
            &mut log.warned_callback,
            &mut log.warned_user_tex,
        );
    }
    if stream.draws.is_empty() {
        return Ok(None); // 本帧无可画内容（如首个 pass 前的空帧）
    }
    // —— 4) 容量判定（超容量丢当帧 UI，帧循环照常）+ 顶点环写入 ——
    let UiStream {
        vertices,
        indices,
        draws,
    } = stream;
    if vertices.len() > ring.vertex_capacity() || indices.len() > ring.index_capacity() {
        log.warned_overflow.log(|| {
            warn!(
                "UI 顶点/索引超环容量（顶点 {}/{}、索引 {}/{}），当帧 UI 丢弃（帧循环照常）",
                vertices.len(),
                ring.vertex_capacity(),
                indices.len(),
                ring.index_capacity(),
            );
        });
        return Ok(None);
    }
    ring.write(slot, &vertices, &indices)?;
    log.painted_logged.log(|| {
        info!(
            "paint_overlay 统计：draws {}（顶点 {} / 索引 {}），图集槽 {:?}，\
             screen {:.0}×{:.0}pt",
            draws.len(),
            vertices.len(),
            indices.len(),
            atlas.map(|s| (s.texture, s.sampler)),
            frame.screen_points.x,
            frame.screen_points.y,
        );
    });
    Ok(Some(UiPaint {
        // 坑：screen_size 必须与顶点 pos 同域 = points（tessellate 不按 ppp 缩放顶点）；
        // 给物理像素会让 NDC 除偏大、UI 整体 ×ppp 缩小
        screen_pt: [screen_px.width as f32 / ppp, screen_px.height as f32 / ppp],
        draws,
    }))
}

/// 单层镶嵌（按代数缓存）：shapes 代数领先缓存才重镶嵌（并取走 shapes），
/// 否则跳过帧原样复用。镶嵌与 ppp 绑定——ppp 变化必经窗口尺寸变化，而尺寸
/// 变化在 Update 侧强制两层 pass 重跑（代数 +1），故代数相等蕴含 ppp 相等。
fn tessellate_layer(
    ctx: &egui::Context,
    layer: &mut EguiLayer,
    stored: &mut (u64, Vec<egui::epaint::ClippedPrimitive>),
    ppp: f32,
) {
    if layer.generation != stored.0 {
        let meshes = ctx.tessellate(std::mem::take(&mut layer.shapes), ppp);
        *stored = (layer.generation, meshes);
    }
}

/// 摊平后的连续顶点/索引流 + 逐 clip draw。
#[derive(Default)]
struct UiStream {
    vertices: Vec<UiVertex>,
    indices: Vec<u32>,
    draws: Vec<UiDrawCall>,
}

impl UiStream {
    /// 追加一层：逐 clip 的 mesh 接到流尾，每个 clip 产一条 draw（自带纹理/采样器
    /// 槽位：Managed(0) = 图集，Managed(n) 查用户贴图表，缺席/User = 跳过该 clip）。
    #[expect(
        clippy::too_many_arguments,
        reason = "两个告警去重字段必须从 log 单独可变借出（panel_meshes 同时在被只读遍历），合束反成借用冲突"
    )]
    fn append(
        &mut self,
        meshes: &[egui::epaint::ClippedPrimitive],
        ppp: f32,
        screen_px: vk::Extent2D,
        atlas: Option<SlotBinding>,
        gpu: &AtlasGpu,
        warned_callback: &mut OnceLog,
        warned_user_tex: &mut OnceLog,
    ) {
        for clip in meshes {
            let mesh = match &clip.primitive {
                egui::epaint::Primitive::Mesh(mesh) => mesh,
                // 回调图元（PaintCallback）不实现：调试窗口不用，显式边界
                egui::epaint::Primitive::Callback(_) => {
                    warned_callback
                        .log(|| warn!("egui 回调图元（PaintCallback）不实现，该 clip 跳过"));
                    continue;
                }
            };
            let slots = match mesh.texture_id {
                egui::TextureId::Managed(0) => atlas,
                egui::TextureId::Managed(n) => {
                    let slots = gpu.user_slots(n);
                    if slots.is_none() {
                        // 新纹理整传在同一绘制段早于此处，缺席只在镶嵌缓存复用的
                        // 跨帧窗口出现（纹理已 free）
                        warned_user_tex.log(|| warn!("用户贴图槽 {n} 未就绪，该 clip 本帧跳过"));
                    }
                    slots
                }
                // User id 只会来自外部集成的原生纹理句柄，本集成从不产出
                egui::TextureId::User(_) => {
                    warned_user_tex.log(|| warn!("egui User 纹理 id 不实现，该 clip 跳过"));
                    None
                }
            };
            let Some(slots) = slots else {
                continue;
            };
            if mesh.indices.is_empty() || mesh.vertices.is_empty() {
                continue;
            }
            self.draws.push(UiDrawCall {
                vertex_base: self.vertices.len() as u32,
                first_index: self.indices.len() as u32,
                index_count: mesh.indices.len() as u32,
                scissor: scissor_pixels(clip.clip_rect, ppp, screen_px),
                texture: slots.texture,
                sampler: slots.sampler,
            });
            self.vertices.extend(mesh.vertices.iter().map(|v| UiVertex {
                pos: [v.pos.x, v.pos.y],
                uv: [v.uv.x, v.uv.y],
                color: v.color.to_array(),
            }));
            self.indices.extend_from_slice(&mesh.indices);
        }
    }
}

/// egui 纹理 id → 用户贴图表键：只认 Managed 序号（图集 Managed(0) 不走此路）。
/// egui User 纹理 id 只会来自外部集成的原生纹理句柄，本集成从不产出，warn 一次拒绝。
fn managed_key(id: egui::TextureId, log: &mut OverlayLogState) -> Option<u64> {
    match id {
        egui::TextureId::Managed(n) => Some(n),
        egui::TextureId::User(_) => {
            log.warned_user_tex
                .log(|| warn!("egui User 纹理 id 不实现，增量拒绝"));
            None
        }
    }
}

/// 用户贴图整传：建独立 GpuImage（gamma 域 RGBA8、LINEAR、单 mip，与图集同款
/// 色彩约定）+ 提交 + 发布槽位（采样器经 key 去重，与图集共用同槽）。
fn upload_user_texture(
    ctx: &Context,
    tables: &mut BindlessTextureTable,
    uploader: &mut Uploader,
    image: &egui::epaint::ColorImage,
) -> Result<(GpuImage, Ticket, SlotBinding)> {
    let spec = ImageSpec {
        width: image.size[0] as u32,
        height: image.size[1] as u32,
        format: vk::Format::R8G8B8A8_UNORM,
        mip_levels: 1,
        sampler: ImageSamplerDescriptor::linear(),
    };
    // 图像与池同款 CONCURRENT 双族（transfer 写 + graphics 读）
    let sharing: Vec<u32> = if ctx.transfer_queue_family_index != ctx.queue_family_index {
        vec![ctx.transfer_queue_family_index, ctx.queue_family_index]
    } else {
        Vec::new()
    };
    let gpu_image = GpuImage::create(
        &ctx.device,
        &ctx.instance,
        ctx.physical_device,
        ctx.memory_contract(),
        &spec,
        &sharing,
    )?;
    // 像素字节直取：ColorImage 紧密行主序 RGBA8，与 staging 期望一致
    const _: () = assert!(std::mem::size_of::<egui::Color32>() == 4);
    let staging =
        unsafe { std::slice::from_raw_parts(image.pixels.as_ptr().cast(), image.pixels.len() * 4) }
            .to_vec();
    let batch = UploadBatch {
        staging,
        image_uploads: vec![StagingImageCopy {
            image: gpu_image.image(),
            src_offset: 0,
            width: spec.width,
            height: spec.height,
            mip_levels: 1,
            format: spec.format,
        }],
        ..Default::default()
    };
    // staging 非空的批次必有提交（uploader 契约），None 是内部矛盾
    let ticket = uploader.submit_batch(batch)?.ok_or_else(|| {
        VulkanError::Init("用户贴图批未产生提交（staging 非空，内部矛盾）".into())
    })?;
    // 槽位发布在提交成功后（失败不留指向未上传数据的槽）
    let slots = tables.publish(gpu_image.view(), &sampler_key(&spec.sampler))?;
    Ok((gpu_image, ticket, slots))
}

/// clip 矩形（points，左上原点）→ scissor（物理像素），裁剪到屏幕内。
/// 零尺寸 scissor 合法（Vulkan 语义 = 全裁）。
fn scissor_pixels(clip: egui::Rect, ppp: f32, screen: vk::Extent2D) -> [u32; 4] {
    let sx = screen.width as f32;
    let sy = screen.height as f32;
    let x = (clip.min.x * ppp).clamp(0.0, sx);
    let y = (clip.min.y * ppp).clamp(0.0, sy);
    let x1 = (clip.max.x * ppp).clamp(x, sx);
    let y1 = (clip.max.y * ppp).clamp(y, sy);
    [x as u32, y as u32, (x1 - x) as u32, (y1 - y) as u32]
}
