//! 一帧 shadow 的录制输入束：②③ 同门绘制 + ⑤ 恒绑资源。

use ash::{vk, Device};

use crate::render::core::binding::set_layout::DescSet;
use crate::render::core::record::barrier::ImageBarrier;
use crate::render::passes::shadow::cascades::SHADOW_CASCADES;
use crate::render::passes::shadow::shadow_build::ShadowBuildDraw;
use crate::render::passes::shadow::shadow_map::ShadowWriteDraw;

/// 一帧 shadow 的录制输入（draw_frame 组装，`record_frame` 消费）。
pub struct ShadowFrameDraw {
    /// 命令组装 + depth pass（关闭/空场为 None，两者同门）。
    pub passes: Option<ShadowPasses>,
    /// shadow 描述符集（合成 pass 总是绑；关闭时 shader 内不采样）。
    pub set: DescSet,
    /// 级联深度图。shadow 描述符总是绑定，所以每帧都录布局屏障，哪怕 pass 跳过。
    pub images: [vk::Image; SHADOW_CASCADES],
}

/// 同门的两段：② 命令组装 dispatch 与 ③ 逐级联 depth 绘制。
pub struct ShadowPasses {
    pub build: ShadowBuildDraw,
    pub write: ShadowWriteDraw,
}

impl ShadowFrameDraw {
    /// ② 段：shadow 命令组装（pass 跳过时不录）。屏障在 [`ShadowBuildDraw::record`] 内。
    pub fn record_build(&self, device: &Device, cb: vk::CommandBuffer, instance_set: DescSet) {
        if let Some(passes) = &self.passes {
            passes.build.record(device, cb, instance_set, self.set);
        }
    }

    /// ③ 段：进场屏障 → 逐级联 depth 绘制 → 出场屏障。进出屏障每帧都录，pass 跳过也一样：
    /// 合成 pass 的 shadow 描述符总是绑定，布局必须始终是 SHADER_READ_ONLY。UNDEFINED 作
    /// oldLayout 从任何状态都合法（每帧 CLEAR 重写）。
    pub fn record_depth(&self, device: &Device, cb: vk::CommandBuffer) {
        unsafe {
            // 进场：→ DEPTH_ATTACHMENT。深度图跨帧共享（不分帧槽）：等上一帧的片元
            // 采样读完再覆写（WAR）
            for &image in &self.images {
                ImageBarrier::discard_to_depth_attachment(image, vk::PipelineStageFlags::FRAGMENT_SHADER)
                    .record(device, cb);
            }
            if let Some(passes) = &self.passes {
                passes.write.record(device, cb, self.set);
            }
            // 出场：→ SHADER_READ_ONLY 供合成采样（同样每帧都录）
            for &image in &self.images {
                ImageBarrier::depth_attachment_to_sampled(image).record(device, cb);
            }
        }
    }
}
