//! overlay 绘制段：图集 CPU 镜像 → 整图重传新槽、镶嵌写 UI 顶点环，
//! `paint_overlay` 把 [`EguiFrame`] 变成 [`UiPaint`]。GPU 侧账面
//!（[`AtlasGpu`]/[`UiVertexRing`]）住 render 层，本模块只做 egui 侧产出。
//!
//! 图集路线：epaint 图集增量是 dirty-rect 局部补丁，而 uploader 图像段只支持整图
//! (0,0) 拷贝（局部拷贝要引入 graphics→transfer 反向同步），故折进 CPU 镜像 →
//! 整图重传全新 GpuImage → publish 新槽 → 旧代入 graveyard。
//! 坑：镜像必须在 Update 的 pass 出口折入（`AtlasMirror::fold`），不能随 EguiFrame
//! 跨系统携带——`TexturesDelta` Drop 时要求增量已消费，且最小化帧 Update 照跑而
//! draw_frame 整帧让路，跨系统携带会被下帧覆写丢失。
//!
//! 时序：`paint_overlay` 必须在 draw_frame 组装 FrameDraw（`upload_wait` 快照）之前
//! 调用——图集批的票据要落进本帧图形提交的等待值。

use ash::vk;
use bevy::{ecs::system::SystemParam, image::ImageSamplerDescriptor, prelude::*};

use crate::{
    common::{
        error::{Result, VulkanError},
        syntax::{OnceLog, ResultTierExt},
    },
    render::{
        sampler_key, AtlasGpu, BindlessTextureTable, Context, GpuImage, ImageSpec, StagingImageCopy,
        UiDrawCall, UiPaint, UiVertex, UiVertexRing, UploadBatch, Uploader,
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
    /// 非图集纹理（用户贴图）告警去重。
    warned_foreign: OnceLog,
    /// 局部补丁先于首个整图的告警去重（理论不发生）。
    warned_orphan: OnceLog,
}

impl AtlasMirror {
    /// 折入一帧的纹理增量（pass 出口调用，纯 CPU）。`Managed(0)` = 字体图集；
    /// 其余/User 纹理不实现（warn 一次跳过）；free 清单无对应资源（图集不释放，
    /// 用户贴图未建），忽略。折完 `clear`——Drop 审查要求增量被消费。
    pub(crate) fn fold(&mut self, delta: &mut egui::TexturesDelta) {
        for (id, deltas) in &delta.set {
            if *id != egui::TextureId::Managed(0) {
                self.warned_foreign
                    .log(|| warn!("egui 纹理 {id:?} 非字体图集（用户贴图不实现，显式边界），增量跳过"));
                continue;
            }
            for d in deltas {
                // ImageData 仅 Color 一变体
                let egui::epaint::ImageData::Color(patch) = &d.image;
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
        delta.clear();
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
    pub(crate) mirror: Res<'w, AtlasMirror>,
    pub(crate) gpu: ResMut<'w, AtlasGpu>,
    pub(crate) ring: ResMut<'w, UiVertexRing>,
}

/// paint_overlay 的跨帧状态（draw_frame 的 Local）：一次性收账/告警去重 + 镶嵌缓存。
#[derive(Default)]
pub(crate) struct OverlayLogState {
    warned_overflow: OnceLog,
    warned_callback: OnceLog,
    warned_no_atlas: OnceLog,
    painted_logged: OnceLog,
    /// 两层镶嵌缓存：(shapes 代数, 镶嵌产物)——代数未变的帧直接复用，按 pass
    /// 代数判等零比较成本（逐形状比较会占 tessellate 一半成本）。
    panel_meshes: (u64, Vec<egui::epaint::ClippedPrimitive>),
    gizmo_meshes: (u64, Vec<egui::epaint::ClippedPrimitive>),
}

/// overlay 绘制段：图集整传（有新代时）→ 镶嵌 → 顶点环写入 → [`UiPaint`]。
/// 在 draw_frame 内、FrameDraw 组装（upload_wait 快照）之前调用。
/// 失败 = 绘制链状态已不可信（图集建图/提交/发布、顶点环写入），按致命错误冒泡。
#[expect(
    clippy::too_many_arguments,
    reason = "bevy 系统侧拆出的独立函数：资源逐项传入是 draw_frame 依赖注入的延续，非函数设计味道"
)]
pub(crate) fn paint_overlay(
    ctx: &Context,
    egui: &EguiState,
    frame: &mut EguiFrame,
    mirror: &AtlasMirror,
    gpu: &mut AtlasGpu,
    ring: &mut UiVertexRing,
    tables: &mut BindlessTextureTable,
    uploader: &mut Uploader,
    slot: usize,
    screen_px: vk::Extent2D,
    log: &mut OverlayLogState,
) -> Result<Option<UiPaint>, BevyError> {
    paint_overlay_steps(ctx, egui, frame, mirror, gpu, ring, tables, uploader, slot, screen_px, log)
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
    mirror: &AtlasMirror,
    gpu: &mut AtlasGpu,
    ring: &mut UiVertexRing,
    tables: &mut BindlessTextureTable,
    uploader: &mut Uploader,
    slot: usize,
    screen_px: vk::Extent2D,
    log: &mut OverlayLogState,
) -> Result<Option<UiPaint>> {
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
            }],
            ..Default::default()
        };
        // staging 非空的批次必有提交（uploader 契约），None 是内部矛盾
        let ticket = uploader
            .submit_batch(batch)?
            .ok_or_else(|| VulkanError::Init("图集批未产生提交（staging 非空，内部矛盾）".into()))?;
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
    // —— 3) 摊平：面板层先、gizmo 层后（gizmo 压顶）——
    let mut stream = UiStream::default();
    for meshes in [&log.panel_meshes.1, &log.gizmo_meshes.1] {
        stream.append(meshes, ppp, screen_px, &mut log.warned_callback);
    }
    if stream.draws.is_empty() {
        return Ok(None); // 本帧无可画内容（如首个 pass 前的空帧）
    }
    // 图集 GPU 代必须在场（shapes 非空 → 文本已栅格化 → 首帧已整传；防御分支）
    let Some(atlas) = gpu.current_slots() else {
        log.warned_no_atlas
            .log(|| warn!("图集 GPU 代缺席（镜像从未折入整图增量），当帧 UI 丢弃"));
        return Ok(None);
    };
    // —— 4) 容量判定（超容量丢当帧 UI，帧循环照常）+ 顶点环写入 ——
    let UiStream { vertices, indices, draws } = stream;
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
            "paint_overlay 统计：draws {}（顶点 {} / 索引 {}），图集槽 {}/{}，\
             screen {:.0}×{:.0}pt",
            draws.len(),
            vertices.len(),
            indices.len(),
            atlas.texture,
            atlas.sampler,
            frame.screen_points.x,
            frame.screen_points.y,
        );
    });
    Ok(Some(UiPaint {
        atlas_texture: atlas.texture,
        atlas_sampler: atlas.sampler,
        // 坑：screen_size 必须与顶点 pos 同域 = points（tessellate 不按 ppp 缩放顶点）；
        // 给物理像素会让 NDC 除偏大、UI 整体 ×ppp 缩小
        screen_pt: [
            screen_px.width as f32 / ppp,
            screen_px.height as f32 / ppp,
        ],
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
    /// 追加一层：逐 clip 的 mesh 接到流尾，每个 clip 产一条 draw。
    fn append(
        &mut self,
        meshes: &[egui::epaint::ClippedPrimitive],
        ppp: f32,
        screen_px: vk::Extent2D,
        warned_callback: &mut OnceLog,
    ) {
        for clip in meshes {
            let mesh = match &clip.primitive {
                egui::epaint::Primitive::Mesh(mesh) => mesh,
                // 回调图元（PaintCallback）不实现：调试窗口不用，显式边界
                egui::epaint::Primitive::Callback(_) => {
                    warned_callback.log(|| warn!("egui 回调图元（PaintCallback）不实现，该 clip 跳过"));
                    continue;
                }
            };
            if mesh.texture_id != egui::TextureId::Managed(0) {
                // 用户贴图（fold 已按同边界告警过，这里静默跳过）
                continue;
            }
            if mesh.indices.is_empty() || mesh.vertices.is_empty() {
                continue;
            }
            self.draws.push(UiDrawCall {
                vertex_base: self.vertices.len() as u32,
                first_index: self.indices.len() as u32,
                index_count: mesh.indices.len() as u32,
                scissor: scissor_pixels(clip.clip_rect, ppp, screen_px),
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
