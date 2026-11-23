//! 环境光照（IBL）：9.1 物理天空 → 环境 cubemap → SH-L2 漫反射 + GGX 预滤波高光。
//!
//! 本模块持有**设置**（[`IblSettings`]）、GPU 资源族（[`IblEnvironment`]）与帧内的
//! 录制束（[`IblFrameDraw`]）。着色器求值在 `shaders/common/ibl.wgsl`，捕获/投影/
//! 预滤波在 `shaders/scene/ibl/`。
//!
//! 两层语义（见段施工计划 §0.1）：IBL 是新的环境基线，不透明与透明都接；SCGI（9.7）
//! 只替换"不透明、Volume 内"的漫反射项，高光与透明层永远是 IBL。
//!
//! `enabled = false` 时不录任何捕获/投影 pass，不 dispatch（零成本），合成与透明
//! 回到步骤 8 的 `albedo · ambient_color`，逐位一致——这是回归硬线。

mod environment;

use ash::{vk, Device};

use crate::render::core::binding::set_layout::DescSet;

pub use environment::half_to_f32;
pub use environment::{
    IblBakeInput, IblBakePlan, IblEnvironment, IBL_CUBE_MIPS, IBL_CUBE_SIZE, IBL_MAX_MIP,
    IBL_SH_BUFFER_SIZE, IBL_STATS_BUFFER_SIZE, IBL_STATS_FLOATS,
};
pub(crate) use environment::{
    BRDF_LUT_PIPELINE, IBL_CAPTURE_SET_DESC, IBL_DFG_SET_DESC, IBL_PREFILTER_SET_DESC,
    IBL_SET_DESC, IBL_SH_CHECK_SET_DESC, IBL_SH_PROJECT_SET_DESC, IBL_SH_REDUCE_SET_DESC,
    PREFILTER_PIPELINE, SH_CHECK_PIPELINE, SH_PROJECT_PIPELINE, SH_REDUCE_PIPELINE,
    SKY_CAPTURE_PIPELINE,
};

/// IBL 漫反射编码：SH-L2（本段恒此值；9.3 的 16 桶另立）。
pub const IBL_MODE_SH_L2: u32 = 0;

/// 环境光照设置（宿主真源；BRP `ibl_set` 与帧 UBO 同源）。
#[derive(bevy::prelude::Resource)]
pub struct IblSettings {
    /// IBL 开关：关 = 不录捕获链、合成/透明回 `albedo · ambient_color`（逐位一致）。
    /// 段内默认关（BRP 开），避免 9.2 一落地就改变所有场景的环境基线。
    pub enabled: bool,
    /// IBL 强度倍数（漫反射与高光同乘）。
    pub intensity: f32,
    /// 漫反射编码（帧 UBO `ibl_mode`；本段恒 [`IBL_MODE_SH_L2`]）。
    pub mode: u32,
}

impl Default for IblSettings {
    fn default() -> Self {
        // 默认关：段内用 BRP 打开做 A/B；9.8 验收后再拍板是否默认开。
        Self {
            enabled: false,
            intensity: 1.0,
            mode: IBL_MODE_SH_L2,
        }
    }
}

/// 一帧 IBL 的录制输入束（draw_frame 组装，record_frame 消费）。
///
/// `bake` 为 `None` = 本帧不重烘（② 段整段跳过、零屏障）；`set` 恒有——合成与透明
/// 前向的管线布局恒声明 group 4，即使 IBL 关也要绑一个合法集。
pub struct IblFrameDraw<'a> {
    /// 本帧烘焙计划 + 资源族（None = 不录）。
    pub bake: Option<IblBakeDraw<'a>>,
    /// 合成/透明前向的 IBL 描述符集（恒绑）。
    pub set: DescSet,
}

impl IblFrameDraw<'_> {
    /// ② 段：录烘焙链（捕获 → SH 投影/归约/自检 → 预滤波；DFG LUT 只首次）。
    pub fn record_bake(&self, device: &Device, cb: vk::CommandBuffer, slot: usize) {
        if let Some(bake) = &self.bake {
            bake.env.record_bake(device, cb, slot, bake.plan);
        }
    }
}

/// 一帧烘焙的录制束：资源族 + 计划。
pub struct IblBakeDraw<'a> {
    pub env: &'a IblEnvironment,
    pub plan: IblBakePlan,
}
