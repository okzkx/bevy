// SMAA 1x 公共段（官方移植：源 = bevy_anti_alias smaa.wesl @ 0.20-dev，MIT，
// (C) 2013 Jorge Jimenez et al.——上游版权头见仓库原文件）。条件编译展开口径：
// SMAA_PRESET_HIGH + corner/diag 全开 + 无 predication/reprojection。
// 接线差异：绑定随 ash_renderer 约定重排（三 pass 的 set0 均为 b0 输入纹理 /
// b1 SmaaInfo / b2+ 采样器与 LUT），每 pass 单文件单入口（vs_main/fs_main）。

// SmaaInfo UBO（32B = SMAA_INFO_SIZE，src/render/pipeline/smaa.rs；宿主每帧写）。
struct SmaaInfo {
    // (1/w, 1/h, w, h)
    rt_metrics: vec4<f32>,
    // x = SMAA 开关（0/1）——关 = neighborhood 混合权重清零直通
    flags: vec4<f32>,
}

const SMAA_THRESHOLD: f32 = 0.1;
const SMAA_MAX_SEARCH_STEPS: u32 = 16u;
const SMAA_MAX_SEARCH_STEPS_DIAG: u32 = 8u;
const SMAA_CORNER_ROUNDING: u32 = 25u;
const SMAA_CORNER_ROUNDING_NORM: f32 = f32(SMAA_CORNER_ROUNDING) / 100.0;
const SMAA_LOCAL_CONTRAST_ADAPTATION_FACTOR: f32 = 2.0;
const SMAA_AREATEX_MAX_DISTANCE: f32 = 16.0;
const SMAA_AREATEX_MAX_DISTANCE_DIAG: f32 = 20.0;
const SMAA_AREATEX_PIXEL_SIZE: vec2<f32> = (1.0 / vec2<f32>(160.0, 560.0));
const SMAA_AREATEX_SUBTEX_SIZE: f32 = (1.0 / 7.0);
const SMAA_SEARCHTEX_SIZE: vec2<f32> = vec2(66.0, 33.0);
const SMAA_SEARCHTEX_PACKED_SIZE: vec2<f32> = vec2(64.0, 16.0);

struct VertexVaryings {
    clip_coord: vec2<f32>,
    tex_coord: vec2<f32>,
}

// 全屏三角形：vertex_index 0/1/2 → uv (0,0)/(0,2)/(2,0)，clip 覆盖视口。
fn calculate_vertex_varyings(vertex_index: u32) -> VertexVaryings {
    let uv = vec2<f32>(f32(vertex_index >> 1u), f32(vertex_index & 1u)) * 2.0;
    let clip_position = vec2<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0));

    return VertexVaryings(clip_position, uv);
}
