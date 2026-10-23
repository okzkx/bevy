//! UI 顶点环：每帧槽一对固定容量顶点/索引 buffer（宿主直写 HOST_VISIBLE，
//! `BufferRole::DynamicDraw`），以及 epaint 顶点的 GPU 侧镜像 [`UiVertex`]。
//! 生产方是 overlay 的绘制半边（`paint_overlay` 镶嵌后写入）；`draw_frame`
//! 取 [`UiVertexRing::buffers`] 交录制层绑定。
//!
//! 不走 MeshPool：池是 DEVICE_LOCAL 资产级 bump，写入走 staging+transfer——
//! 每帧整换的 UI 数据直写更诚实。复用安全与帧 UBO 同一条 fence 纪律：写入
//! 发生在 `wait_for_slot` 之后 = 本槽上一轮提交完成之后。

use ash::vk;
use bevy::prelude::*;

use crate::common::error::Result;
use crate::render::{BufferRole, Context, GpuBuffer, MAX_FRAMES_IN_FLIGHT};

use super::overlay_pipeline::UI_VERTEX_STRIDE;

/// 单槽 UI 顶点容量（个数；×20B = 1.25MB/槽）。调试 UI 单帧数百万顶点不可能，
/// 超容 = Tier① 丢当帧 UI（可交互性损失不拦帧循环）。
const UI_VERTEX_CAPACITY: usize = 64 * 1024;
/// 单槽 UI 索引容量（个数；×4B = 384KB/槽）。
const UI_INDEX_CAPACITY: usize = 96 * 1024;

/// epaint 顶点的 Rust 镜像：与 overlay_draw.wgsl 的顶点输入同一契约
/// （pos vec2f + uv vec2f + color 4×u8 打包，20B 步长）。epaint 默认布局同款
/// 字段序（非 unity feature）；镜像类型切断对 epaint 字段序的依赖。
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct UiVertex {
    pub(crate) pos: [f32; 2],
    pub(crate) uv: [f32; 2],
    /// sRGBA 预乘（gamma 域）——Color32 字节原样直通 GPU。
    pub(crate) color: [u8; 4],
}

const _: () = assert!(std::mem::size_of::<UiVertex>() == UI_VERTEX_STRIDE as usize);

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
    pub fn new(ctx: &Context) -> Result<Self> {
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
    ) -> Result<()> {
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
