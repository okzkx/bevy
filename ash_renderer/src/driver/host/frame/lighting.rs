//! 灯光收集的帧侧单元：方向光查询（LightQuery）与本帧光照输入（FrameLight）。
//! 光源分簇（Cluster）在 [`super::cluster`]，Cluster 光照本体在
//! [`crate::render::passes::lighting`]。

use bevy::{light::GlobalAmbientLight, prelude::*};

use crate::render::{RenderMode, PLACEHOLDER_DIR_TO_LIGHT};

use super::{frame_logs::FrameLogs, scene::FrameView};

/// 方向光查询（取第一盏，方向 = 实体 `back()`）。
pub(super) type LightQuery<'w, 's> =
    Query<'w, 's, (&'static GlobalTransform, &'static DirectionalLight)>;

/// 本帧光照输入（帧 UBO 与级联拟合同源）。
pub(super) struct FrameLight {
    /// 实体 back()（forward 取负，"N·L 就绪"）；无灯 = 合法方向占位。
    pub(super) dir_to_light: [f32; 4],
    /// linear × illuminance；无灯 = 直射项归零。
    pub(super) light_color: [f32; 4],
    /// linear × brightness；两种来源均无 = 环境项归零。
    pub(super) ambient_color: [f32; 4],
}

impl FrameLight {
    /// 方向光取第一盏（多灯告警一次，多灯支持未实现）；环境光相机覆盖优先。
    pub(super) fn gather(
        lights: &LightQuery,
        view: &FrameView,
        global_ambient: Option<&GlobalAmbientLight>,
        render_mode: RenderMode,
        ledger_empty: bool,
        logs: &mut FrameLogs,
    ) -> Self {
        let light_count = lights.iter().count();
        let (dir_to_light, light_color) = match lights.iter().next() {
            Some((light_tf, light)) => {
                let back = light_tf.back();
                let [r, g, b, a] = LinearRgba::from(light.color).to_f32_array();
                (
                    [back.x, back.y, back.z, 0.0],
                    [
                        r * light.illuminance,
                        g * light.illuminance,
                        b * light.illuminance,
                        a,
                    ],
                )
            }
            None => (PLACEHOLDER_DIR_TO_LIGHT, [0.0; 4]),
        };
        if light_count != 1 && !ledger_empty {
            logs.warned_light_count.log(|| {
                warn!("方向光 {light_count} 盏（取第一盏/无灯直射项归零），多灯支持未实现")
            });
        }
        let ambient = view
            .ambient_override
            .map(|a| (a.color, a.brightness))
            .or(global_ambient.map(|g| (g.color, g.brightness)));
        let ambient_color = match ambient {
            Some((color, brightness)) => LinearRgba::from(color)
                .to_f32_array()
                .map(|c| c * brightness),
            None => {
                logs.warned_no_ambient.log(|| {
                    warn!("环境光缺席：GlobalAmbientLight 资源与相机 AmbientLight 均无，环境项归零")
                });
                [0.0; 4]
            }
        };
        if light_count > 0 {
            logs.lights_logged.log(|| {
                info!(
                    "灯光 UBO 汇总：方向光 {light_count} 盏（dir_to_light = back()，light = {}）；\
                 环境 = {}（来源 {}）；材质模式 = {}（ASH_RENDER_MODE）",
                    fmt_vec4(light_color),
                    fmt_vec4(ambient_color),
                    if view.ambient_override.is_some() {
                        "相机 AmbientLight"
                    } else {
                        "GlobalAmbientLight"
                    },
                    render_mode.label(),
                )
            });
        }
        Self {
            dir_to_light,
            light_color,
            ambient_color,
        }
    }
}

/// 灯光日志用的 vec4 格式。
fn fmt_vec4(v: [f32; 4]) -> String {
    format!("({:.1}, {:.1}, {:.1}, {:.1})", v[0], v[1], v[2], v[3])
}
