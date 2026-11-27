//! 屏幕空间反射（10.3）：合成后的 HDR 上做世界空间光线步进，为光滑金属/湿地面
//! 补当帧场景内反射；命中失败回退 IBL。
//!
//! 段内四件事，顺序即数据流：
//! 1. 拷贝（`copy_pipeline`）：合成写完的 HDR → `hdr_copy`（全屏 pass，只采样 b2）。
//! 2. 步进（`ssr_pass`）：半分辨率 compute，读 `hdr_copy`/深度/法线/材质，写半分辨率
//!    反射图（rgb = 命中色，a = 权重 w）。
//! 3. 合成（`composite_pipeline`）：全分辨率 pass，`HDR = hdr_copy + w·(命中色 − 原本的
//!    IBL 高光)`；w = 0 时是加 0 的 IEEE 恒等。
//! 4. 目标（`targets`）：半分辨率反射图（storage 写 + sampled 读）与每槽 `hdr_copy`。
//!
//! 关档（[`ReflectionSettings::enabled`] 为 `false`）时三段全跳过（`None`），既不拷贝也不
//! 开销一次全屏 pass；因此本段没有 dummy 采样集，合成管线也不进主合成 pass 的集序。
//!
//! | 文件 | 内容 |
//! |---|---|
//! | `mod.rs` | 设置与模块出口 |
//! | `targets.rs` | 反射图与 `hdr_copy`、描述符集、resize 重建 |
//! | `copy_pipeline.rs` | HDR 拷贝全屏管线 |
//! | `ssr_pass.rs` | 步进 compute 管线与录制束 |
//! | `composite_pipeline.rs` | 反射合成全屏管线 |

use bevy::prelude::Resource;

pub mod composite_pipeline;
pub mod frame_draw;
pub mod copy_pipeline;
pub mod ssr_pass;
pub mod targets;

/// 屏幕空间反射设置（BRP `ssr_set` 与 overlay 面板共用同一份）。
#[derive(Resource)]
pub struct ReflectionSettings {
    /// 总开关。默认关：关档时画面与没接 SSR 逐字节一致（不拷贝、不步进、不合成）。
    pub enabled: bool,
    /// 步进次数上限。首版线性步进（主计划 §5 D4）：`ssr_ms` 超 1.5 ms 预算再评估 Hi-Z。
    pub steps: u32,
    /// 射线最远步进距离（米）。世界空间步进，所以是场景尺度而非像素。
    pub max_distance_m: f32,
    /// 命中厚度（米）：射线点与场景视深之差小于它的算命中（薄物体也不至于穿过去）。
    pub thickness_m: f32,
    /// 粗糙度门控上限：`perceptualRoughness` 超过它的像素不反射（模糊面交给 IBL）。
    pub roughness_cutoff: f32,
    /// 屏幕边缘淡出宽度（uv 比例）：边缘处的命中不可信，按它线性淡出。
    pub edge_fade: f32,
    /// 反射强度：乘进权重 w（`0..=1` 再乘命中色），0 等于关档。
    pub intensity: f32,
}

impl Default for ReflectionSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            steps: 96,
            max_distance_m: 24.0,
            thickness_m: 0.35,
            roughness_cutoff: 0.35,
            edge_fade: 0.10,
            intensity: 1.0,
        }
    }
}
