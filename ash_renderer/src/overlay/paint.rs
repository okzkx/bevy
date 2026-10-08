//! overlay 绘制半边（3.7.2）：图集 CPU 镜像 → 整图重传新槽、UI 顶点环、
//! `paint_overlay` 把 [`EguiFrame`] 变成 [`UiPaint`]。
//!
//! 图集路线（施工计划 §3.1）：epaint 图集增量是 dirty-rect 局部补丁，而 uploader
//! 图像段只支持整图 (0,0) 拷贝；扩局部区要引入 graphics→transfer 的新同步方向
//! （现有票据单向），故走"**折进 CPU 镜像 → 整图重传全新 GpuImage → publish 新槽
//! → 旧代入 graveyard（与表同寿）**"。镜像折入在 Update 的 pass 出口（`AtlasMirror::fold`），
//! 不随 EguiFrame 跨系统存放——`TexturesDelta` 的 Drop 审查要求增量被消费，且
//! 最小化帧 Update 照跑而 draw_frame 整帧让路，跨系统携带会被下帧覆写丢失。
//!
//! 顶点环（施工计划 §3.2）：UI 顶点/索引每帧整换、宿主直写 HOST_VISIBLE
//! （`BufferRole::DynamicDraw`），不走 MeshPool（池是 DEVICE_LOCAL 资产级 bump，
//! 写入走 staging+transfer——每帧整换的 UI 数据直写更诚实）。复用安全与帧 UBO
//! 同一条 fence 纪律：写入发生在 `wait_for_slot` 之后 = 本槽上一轮提交完成之后。
//!
//! 时序：`paint_overlay` 在 draw_frame 内、FrameDraw 组装（`wait_ticket` 快照）
//! **之前**调用——图集批的票据必须落进本帧图形提交的等待值。

use ash::vk;
use bevy::{ecs::system::SystemParam, image::ImageSamplerDescriptor, prelude::*};

use crate::{
    common::error::VulkanError,
    vulkan::{
        sampler_key, BindlessTables, BufferRole, Context, GpuBuffer, GpuImage, ImageSpec,
        SlotBinding, StagingImageCopy, UiDrawCall, UiPaint, UploadBatch, Uploader,
        MAX_FRAMES_IN_FLIGHT, UI_VERTEX_STRIDE,
    },
};

use super::ui::{EguiFrame, EguiState};

/// 单槽 UI 顶点容量（个数；×20B = 1.25MB/槽）。调试 UI 单帧数百万顶点不可能，
/// 超容 = Tier① 丢当帧 UI（可交互性损失不拦帧循环）。
const UI_VERTEX_CAPACITY: usize = 64 * 1024;
/// 单槽 UI 索引容量（个数；×4B = 256KB/槽）。
const UI_INDEX_CAPACITY: usize = 96 * 1024;

/// epaint 顶点的 Rust 镜像：与 overlay_draw.wgsl 的顶点输入同一契约
/// （pos vec2f + uv vec2f + color 4×u8 打包，20B 步长）。epaint 默认布局同款
/// 字段序（非 unity feature）；镜像类型切断对 epaint 字段序的依赖。
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct UiVertex {
    pos: [f32; 2],
    uv: [f32; 2],
    /// sRGBA 预乘（gamma 域）——Color32 字节原样直通 GPU。
    color: [u8; 4],
}

const _: () = assert!(std::mem::size_of::<UiVertex>() == UI_VERTEX_STRIDE as usize);

/// 字体图集的 CPU 镜像（Update 侧由 [`super::ui`] 折入，绘制半边按代数差整传）。
#[derive(Resource, Default)]
pub struct AtlasMirror {
    /// 镜像本体：首个整图 delta 落位，此后 dirty-rect 折入。
    image: Option<egui::epaint::ColorImage>,
    /// 镜像代数：任何折入 +1。GPU 半边"已传代数 < 镜像代数"即需整传。
    generation: u64,
    /// 非图集纹理（用户贴图）告警去重：显式边界，出现时跳过。
    warned_foreign: bool,
    /// 局部补丁先于首个整图的告警去重（理论不发生，施工计划 §7）。
    warned_orphan: bool,
}

impl AtlasMirror {
    /// 折入一帧的纹理增量（pass 出口调用，纯 CPU）。`Managed(0)` = 字体图集；
    /// 其余/User 纹理不实现（warn 一次跳过）；free 清单无对应资源（图集不释放，
    /// 用户贴图未建），忽略。折完 `clear`——Drop 审查要求增量被消费。
    pub(crate) fn fold(&mut self, delta: &mut egui::TexturesDelta) {
        for (id, deltas) in &delta.set {
            if *id != egui::TextureId::Managed(0) {
                if !self.warned_foreign {
                    self.warned_foreign = true;
                    warn!("egui 纹理 {id:?} 非字体图集（用户贴图不实现，显式边界），增量跳过");
                }
                continue;
            }
            for d in deltas {
                // ImageData 0.36 仅 Color 一变体（施工计划 §2.1）
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
                        None => {
                            if !self.warned_orphan {
                                self.warned_orphan = true;
                                warn!(
                                    "图集局部补丁先于首个整图增量（镜像缺席），本补丁跳过——\
                                     下个整图增量自愈"
                                );
                            }
                        }
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

/// 图集 GPU 侧：当前代资源 + 退役代 graveyard。
#[derive(Resource, Default)]
pub struct AtlasGpu {
    /// 当前在用的图集代（最近一次整传）。
    current: Option<AtlasSlot>,
    /// 退役代：整传后旧 GpuImage 进来，与 BindlessTables 同寿——槽位持有的是
    /// view/sampler 句柄借用，本体必须活到表拆除（teardown 序：表先、本体后）。
    graveyard: Vec<GpuImage>,
    /// 已整传的镜像代数。
    uploaded_generation: u64,
}

/// 一代图集的 GPU 资源与 bindless 槽位。
struct AtlasSlot {
    image: GpuImage,
    slots: SlotBinding,
}

impl AtlasGpu {
    /// 已整传的镜像代数（调试窗口统计行）。
    pub(crate) fn generation(&self) -> u64 {
        self.uploaded_generation
    }

    /// graveyard 累计张数（调试窗口统计行）。
    pub(crate) fn graveyard_len(&self) -> usize {
        self.graveyard.len()
    }
}

/// UI 顶点环：每帧槽一对固定容量顶点/索引 buffer（`MAX_FRAMES_IN_FLIGHT` 组），
/// 宿主按帧直写。写入安全契约 = 调用方（draw_frame）先过 `wait_for_slot`。
#[derive(Resource)]
pub struct UiVertexRing {
    slots: Vec<UiRingSlot>,
}

struct UiRingSlot {
    vertex: GpuBuffer,
    index: GpuBuffer,
}

impl UiVertexRing {
    /// 建环：每帧槽一对 buffer（创建失败全有或全无，`GpuBuffer::create` 自回收）。
    ///
    /// # Errors
    /// 任一 buffer 创建/分配/绑定/映射失败（Tier②，初始化链冒泡）。
    pub fn new(ctx: &Context) -> Result<Self, VulkanError> {
        let mut slots = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            slots.push(UiRingSlot {
                vertex: GpuBuffer::create(
                    &ctx.device,
                    ctx.memory_contract(),
                    (UI_VERTEX_CAPACITY * std::mem::size_of::<UiVertex>()) as u64,
                    BufferRole::DynamicDraw,
                )?,
                index: GpuBuffer::create(
                    &ctx.device,
                    ctx.memory_contract(),
                    (UI_INDEX_CAPACITY * std::mem::size_of::<u32>()) as u64,
                    BufferRole::DynamicDraw,
                )?,
            });
        }
        Ok(Self { slots })
    }

    /// 本帧槽顶点/索引 buffer 句柄（录制段绑定用）。
    pub(crate) fn buffers(&self, slot: usize) -> (vk::Buffer, vk::Buffer) {
        let s = &self.slots[slot];
        (s.vertex.buffer(), s.index.buffer())
    }

    /// 写本帧槽：整帧 UI 顶点/索引从头直写（offset 0）。容量在调用方先判。
    ///
    /// # Errors
    /// 底层写/flush 失败（Tier② 冒泡）。
    pub(crate) fn write(
        &mut self,
        slot: usize,
        vertices: &[UiVertex],
        indices: &[u32],
    ) -> Result<(), VulkanError> {
        // # Safety:UiVertex/u32 均 repr 紧排、无填充（上方 const 断言 20B），
        // 字节视图只在 write 调用内存活
        let vbytes = unsafe {
            std::slice::from_raw_parts(vertices.as_ptr().cast(), std::mem::size_of_val(vertices))
        };
        let ibytes = unsafe {
            std::slice::from_raw_parts(indices.as_ptr().cast(), std::mem::size_of_val(indices))
        };
        let s = &mut self.slots[slot];
        s.vertex.write(0, vbytes)?;
        s.index.write(0, ibytes)
    }

    /// 单槽顶点容量（个数）。
    pub(crate) fn vertex_capacity(&self) -> usize {
        UI_VERTEX_CAPACITY
    }

    /// 单槽索引容量（个数）。
    pub(crate) fn index_capacity(&self) -> usize {
        UI_INDEX_CAPACITY
    }
}

/// paint_overlay 的资源束（SystemParam，与 UploadData 同款式样）。
#[derive(SystemParam)]
pub(crate) struct UiDrawData<'w> {
    pub(crate) egui: Res<'w, EguiState>,
    pub(crate) frame: ResMut<'w, EguiFrame>,
    pub(crate) mirror: Res<'w, AtlasMirror>,
    pub(crate) gpu: ResMut<'w, AtlasGpu>,
    pub(crate) ring: ResMut<'w, UiVertexRing>,
}

/// paint_overlay 的一次性收账/告警去重（draw_frame 的 Local）。
#[derive(Default)]
pub(crate) struct OverlayLogState {
    pub warned_overflow: bool,
    pub warned_callback: bool,
    pub warned_no_atlas: bool,
    pub painted_logged: bool,
}

/// overlay 绘制半边本体：图集整传（有新代时）→ 镶嵌 → 顶点环写入 → [`UiPaint`]。
/// 在 draw_frame 内、FrameDraw 组装（wait_ticket 快照）之前调用。
///
/// # Errors
/// 图集建图/提交/发布、顶点环写入失败（Tier②：绘制链状态已不可信，冒泡退出）。
#[expect(
    clippy::too_many_arguments,
    reason = "bevy 系统侧拆出的半边函数：资源逐项传入是 draw_frame 依赖注入的延续，非函数设计味道"
)]
pub(crate) fn paint_overlay(
    ctx: &Context,
    egui: &EguiState,
    frame: &mut EguiFrame,
    mirror: &AtlasMirror,
    gpu: &mut AtlasGpu,
    ring: &mut UiVertexRing,
    tables: &mut BindlessTables,
    uploader: &mut Uploader,
    slot: usize,
    screen_px: vk::Extent2D,
    log: &mut OverlayLogState,
) -> Result<Option<UiPaint>, VulkanError> {
    // —— 1) 图集整传：镜像代数领先已传代数 → 全新 GpuImage + 独立批 + publish 新槽 ——
    let (mirror_image, mirror_generation) = mirror.snapshot();
    if gpu.uploaded_generation < mirror_generation {
        let Some(image) = mirror_image else {
            // 折入只增代数不建镜像的路径不存在（整图增量才落位）；防御性按内部
            // 矛盾处理，不 panic
            return Err(VulkanError::Init(
                "图集镜像代数领先但本体缺席（内部矛盾）".into(),
            ));
        };
        // 图集是 gamma 域 RGBA8、LINEAR 滤波（egui-wgpu 同款判据，施工计划 §2.2）；
        // 非 sRGB 格式 = 采样不解码，色彩约定见 overlay_draw.wgsl
        let spec = ImageSpec {
            width: image.size[0] as u32,
            height: image.size[1] as u32,
            format: vk::Format::R8G8B8A8_UNORM,
            sampler: ImageSamplerDescriptor::linear(),
        };
        // 图像与池同款 CONCURRENT 双族（transfer 写 + graphics 读，3.4 定案）
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
            }],
            ..Default::default()
        };
        match uploader.submit_batch(batch)? {
            Some(ticket) => {
                // 槽位发布在提交成功后（失败不留指向未上传数据的槽，与 flush_uploads
                // 同纪律）；只写新槽，UAB 覆盖竞态结构上不存在
                let slots = tables.publish(
                    gpu_image.view(),
                    gpu_image.sampler(),
                    &sampler_key(&spec.sampler),
                )?;
                if let Some(old) = gpu.current.replace(AtlasSlot {
                    image: gpu_image,
                    slots,
                }) {
                    gpu.graveyard.push(old.image);
                }
                gpu.uploaded_generation = mirror_generation;
                info!(
                    "图集整传 #{ticket}：{}×{}（第 {mirror_generation} 代），纹理槽 {}/{}；\
                     旧代入 graveyard（累计 {} 张，与表同寿）",
                    spec.width,
                    spec.height,
                    slots.texture,
                    tables.capacity(),
                    gpu.graveyard.len(),
                );
            }
            // staging 非空的批次必有提交（uploader 契约）；此处为内部矛盾
            None => {
                return Err(VulkanError::Init(
                    "图集批未产生提交（staging 非空，内部矛盾）".into(),
                ));
            }
        }
    }
    // —— 2) 镶嵌：shapes（未镶嵌）→ ClippedPrimitive（本帧消费，mem::take 后
    // 下个 Update 重填）——
    if frame.shapes.is_empty() {
        return Ok(None); // 本帧无 UI（首个 pass 前的空帧）
    }
    let clipped = egui
        .ctx
        .tessellate(std::mem::take(&mut frame.shapes), frame.pixels_per_point);
    // —— 3) 摊平：逐 clip 的 mesh → 连续顶点/索引流 + draw 引脚 ——
    let mut vertices: Vec<UiVertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut draws: Vec<UiDrawCall> = Vec::new();
    for clip in clipped {
        let mesh = match clip.primitive {
            egui::epaint::Primitive::Mesh(mesh) => mesh,
            // 回调图元（PaintCallback）不实现：调试窗口不用，显式边界
            egui::epaint::Primitive::Callback(_) => {
                if !log.warned_callback {
                    log.warned_callback = true;
                    warn!("egui 回调图元（PaintCallback）不实现，该 clip 跳过");
                }
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
        let draw = UiDrawCall {
            vertex_base: vertices.len() as u32,
            first_index: indices.len() as u32,
            index_count: mesh.indices.len() as u32,
            scissor: scissor_pixels(clip.clip_rect, frame.pixels_per_point, screen_px),
        };
        vertices.extend(mesh.vertices.iter().map(|v| UiVertex {
            pos: [v.pos.x, v.pos.y],
            uv: [v.uv.x, v.uv.y],
            color: v.color.to_array(),
        }));
        indices.extend_from_slice(&mesh.indices);
        draws.push(draw);
    }
    if draws.is_empty() {
        return Ok(None);
    }
    // 图集 GPU 代必须在场（shapes 非空 → 文本已栅格化 → 首帧已整传；防御分支）
    let Some(atlas) = &gpu.current else {
        if !log.warned_no_atlas {
            log.warned_no_atlas = true;
            warn!("图集 GPU 代缺席（镜像从未折入整图增量），当帧 UI 丢弃");
        }
        return Ok(None);
    };
    // —— 4) 容量判定（Tier①）+ 顶点环写入 ——
    if vertices.len() > ring.vertex_capacity() || indices.len() > ring.index_capacity() {
        if !log.warned_overflow {
            log.warned_overflow = true;
            warn!(
                "UI 顶点/索引超环容量（顶点 {}/{}、索引 {}/{}），当帧 UI 丢弃（Tier①，帧循环照常）",
                vertices.len(),
                ring.vertex_capacity(),
                indices.len(),
                ring.index_capacity(),
            );
        }
        return Ok(None);
    }
    ring.write(slot, &vertices, &indices)?;
    if !log.painted_logged {
        log.painted_logged = true;
        info!(
            "paint_overlay 收账：draws {}（顶点 {} / 索引 {}），图集槽 {}/{}，\
             screen {:.0}×{:.0}pt",
            draws.len(),
            vertices.len(),
            indices.len(),
            atlas.slots.texture,
            atlas.slots.sampler,
            frame.screen_points.x,
            frame.screen_points.y,
        );
    }
    Ok(Some(UiPaint {
        atlas_texture: atlas.slots.texture,
        atlas_sampler: atlas.slots.sampler,
        // push 的 screen_size 与顶点 pos 同域 = points：tessellate 不按 ppp 缩放
        // 顶点（ppp 只进字形栅格化/像素取整/圆半径），换算成像素是渲染目标的
        // 职责——egui-wgpu 0.36.2 同款：uniform 字段即 screen_size_in_points
        //（= size_in_pixels / ppp）。给物理像素会让 NDC 除偏大、UI 整体 ×ppp 缩小
        screen_pt: [
            screen_px.width as f32 / frame.pixels_per_point,
            screen_px.height as f32 / frame.pixels_per_point,
        ],
        draws,
    }))
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
