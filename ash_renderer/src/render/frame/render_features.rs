//! 一帧渲染功能的录制输入集合：一个字段一个功能束，字段按 `record_frame_steps` 的九段
//! 消费时序排列。场景资源与规格不在这里，见 [`SceneResources`](super::scene_resources::SceneResources)。

use super::frame_pool::UploadWait;
use crate::render::core::binding::set_layout::DescSet;
use crate::render::passes::ambient::ssao_pass::SsaoFrameDraw;
use crate::render::passes::gi::GiFrameDraw;
use crate::render::passes::ibl::IblFrameDraw;
use crate::render::passes::lighting::cluster::ClusterFrameDraw;
use crate::render::passes::overlay::overlay_pipeline::UiDraw;
use crate::render::passes::post::post_chain::PostChainDraw;
use crate::render::passes::ray_tracing::RayTracingFrameDraw;
use crate::render::passes::reflection::frame_draw::ReflectionFrameDraw;
use crate::render::passes::shadow::shadow_frame::ShadowFrameDraw;
use crate::render::passes::transparent::TransparentFrame;
use crate::render::passes::visibility::cull::CullDraw;
use crate::render::passes::visibility::hiz_frame::HizFrameDraw;
use crate::render::passes::visibility::indirect::IndirectDraw;
use crate::render::passes::visibility::lod::LodDraw;
use crate::render::stats::perf::GpuTimer;

/// 一帧渲染功能的录制输入集合（draw_frame 组装，`record_frame` 消费）：一个字段一个功能束；
/// 场景资源与规格走 [`SceneResources`] 参数，不混进功能清单。
/// `None` 的可选段整段跳过。字段按 `record_frame_steps` 的九段消费时序排列（① 帧首 → … → ⑨ 提交）。
pub struct RenderFeatures<'a> {
    // —— ① 帧首：计时 + 进场屏障（G-buffer 进场屏障读 res.gbuffer）——
    /// GPU 帧计时：帧首 reset + 原点，段界打点见 [`TimestampPoint`](crate::render::stats::perf::TimestampPoint)；None = 队列族不支持
    /// timestamp。
    pub timer: Option<&'a GpuTimer>,
    // —— ② compute 前置（录制顺序即依赖序）——
    /// 视锥剔除与阴影视锥标记（两者都关闭或空场为 None）。
    pub frustum_cull: Option<CullDraw>,
    /// Hi-Z：② 遮挡测试（门控 Option）+ ⑥ 金字塔归约（每帧都录）。
    pub hiz: HizFrameDraw<'a>,
    /// GPU LOD 选档（关闭/空场为 None；关闭时 host 已把档位环填 0）。屏障在 LodDraw::record 内。
    pub lod: Option<LodDraw>,
    /// indirect 命令组装（空场为 None）。命令写 → 绘制读的屏障在 IndirectDraw::record 内。
    pub indirect: Option<IndirectDraw>,
    /// shadow：命令组装 + depth pass（同门 Option）与恒绑 set/深度图。
    pub shadow: ShadowFrameDraw,
    /// 光源分簇：② build dispatch（空场 None）与恒绑 set（collect/合成/透明前向）。
    pub cluster: ClusterFrameDraw,
    /// 透明层：收集/排序/组装 dispatch + 透明绘制（空场为 None）。
    pub transparent: Option<TransparentFrame>,
    /// 环境光照：② 段烘焙（脏帧才录）与 ⑤ 段绑进合成/透明前向的 IBL 集。
    pub ibl: IblFrameDraw<'a>,
    /// 求交器（9.4）：② 段 BLAS 待建命令 + TLAS 构建 + 调试图；光线求交关/未就绪为 None，
    /// 整段跳过（光线求交关 = 与 9.3 逐位一致）。
    pub ray_tracing: Option<RayTracingFrameDraw<'a>>,
    /// 表面缓存（9.5）：④.5 段回收三件 + 屏幕播种，⑧ 段 patch 调试视图；GI 关为 None，
    /// 整段跳过（GI 关 = 与 9.3 逐位一致）。
    pub gi: Option<GiFrameDraw>,
    /// 环境光遮蔽（10.2）：④.6 段两段 dispatch（半分辨率 AO → 全分辨率 AO）；关档为 None，
    /// 整段跳过（零命令零屏障），合成段仍绑 1×1 白 dummy 采样集乘 1.0。
    pub ssao: Option<SsaoFrameDraw>,
    /// 10.2 合成 pass 第 7 集：SSAO 全分辨率 AO；关档绑 1×1 白图（乘 1.0 恒等），
    /// 所以无论开关都恒绑，pipeline layout 不变。
    pub ssao_set: DescSet,
    /// 屏幕空间反射（10.3）：④.7 段「HDR 拷贝 → 半分辨率步进 → 反光合成」，插在合成与
    /// 透明层之间；关档或非 Lambert 调试模式为 None，整段跳过（零命令零屏障）。
    pub reflection: Option<ReflectionFrameDraw>,
    /// 9.7.6 合成 pass 第 6 集：SCGI 全分辨率辐照度（`frame.scgi.x == 0` 时着色器不读它，
    /// 但集恒绑——pipeline layout 恒定，"关 = 基线"才不用重建管线）。
    pub scgi_set: DescSet,
    // —— ⑦⑧ 后链 + UI ——
    /// ⑦⑧ 后链束：Bloom → tonemap → SMAA → neighborhood（全部来自 `post`）。
    pub post: PostChainDraw,
    /// 本帧 UI 绘制束（无 UI 为 None）：overlay 线 + 绘制产物 + 顶点/索引环 buffer。
    pub ui: Option<UiDraw<'a>>,
    // —— ⑨ 提交 ——
    /// 图形提交对上传队列的跨队列等待（备料时刻快照，见 [`UploadWait`]）。
    pub upload_wait: UploadWait,
}
