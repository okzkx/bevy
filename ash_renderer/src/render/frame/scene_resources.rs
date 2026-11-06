//! ④⑤ 不透明场景链的资源与规格：draw_frame 每帧备料（管线/描述符集/buffer 按 slot 取），
//! `record_frame` 以 `res` 参数消费。功能门控不在这里，见
//! [`RenderFeatures`](super::render_features::RenderFeatures)。

use ash::vk;

use crate::common::error::{Result, VulkanError};
use crate::render::core::record::graphics::PipelineRef;
use crate::render::core::binding::set_layout::DescSet;
use crate::render::GBufferTargets;

/// ④⑤ 不透明场景链的资源与规格（draw_frame 备料，`record_frame` 消费）：场景管线与三集、
/// 网格 buffer 与命令环、G-buffer 目标与合成管线。
pub struct SceneResources<'a> {
    /// 场景管线（G-buffer pass；bindless/帧 UBO/实例表三集）。
    pub pipeline: PipelineRef,
    /// bindless 纹理表，整帧绑一次（场景管线 set0 槽）。
    pub bindless_set: DescSet,
    /// 本帧槽的每帧 UBO（场景管线 set1 槽）。
    pub frame_set: DescSet,
    /// 本帧槽的实例表，对象表 + 本帧各环（场景管线 set2 槽；② 段 compute dispatch 复绑）。
    pub instance_set: DescSet,
    pub vertex_buffer: Option<vk::Buffer>,
    pub index_buffer: Option<vk::Buffer>,
    /// 在场行数（= GPU 侧命令条数；0 = 空场，场景绘制整段跳过）。
    pub indirect_count: u32,
    /// 本帧槽命令环（build_indirect 每帧写）。
    pub indirect_buffer: vk::Buffer,
    /// G-buffer 附件 + HDR 中间目标，以及合成/tonemap 用的采样集（① 进场屏障即读）。
    pub gbuffer: &'a GBufferTargets,
    /// 合成管线（G-buffer 光照累积 → HDR；全屏，无深度/混合）。
    pub composite: PipelineRef,
}

impl SceneResources<'_> {
    /// 顶点/索引池句柄。有在场行时池必然已建，缺席说明上传与绘制状态矛盾，报错退出而不是
    /// 静默跳过整个场景。
    pub(crate) fn pool_buffers(&self) -> Result<(vk::Buffer, vk::Buffer)> {
        let vertex_buffer = self.vertex_buffer.ok_or_else(|| {
            VulkanError::Contract("在场行非空但顶点池缺席——上传链与绘制链的状态矛盾".into())
        })?;
        let index_buffer = self.index_buffer.ok_or_else(|| {
            VulkanError::Contract("在场行非空但索引池缺席——上传链与绘制链的状态矛盾".into())
        })?;
        Ok((vertex_buffer, index_buffer))
    }
}

