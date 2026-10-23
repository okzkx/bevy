// 8.5 SMAA（官方 1x 移植：源 = bevy_anti_alias smaa.wesl @ 0.20-dev，MIT，
// (C) 2013 Jorge Jimenez et al.——上游版权头见仓库原文件）。条件编译展开口径：
// SMAA_PRESET_HIGH + corner/diag 全开 + 无 predication/reprojection。
// 接线差异：绑定随 ash_renderer 约定重排（set0 共享：b0 输入纹理 / b1
// SmaaInfo / b2+ 采样器与 LUT），单文件单入口（vs_main/fs_main）。

struct SmaaInfo {
    rt_metrics: vec4<f32>,
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


fn calculate_vertex_varyings(vertex_index: u32) -> VertexVaryings {
    // See the explanation above for how this works
    let uv = vec2<f32>(f32(vertex_index >> 1u), f32(vertex_index & 1u)) * 2.0;
    let clip_position = vec2<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0));

    return VertexVaryings(clip_position, uv);
}

//-----------------------------------------------------------------------------
// Vertex Shaders


struct EdgeDetectionVaryings {
    @builtin(position) position: vec4<f32>,
    @location(0) offset_0: vec4<f32>,
    @location(1) offset_1: vec4<f32>,
    @location(2) offset_2: vec4<f32>,
    @location(3) tex_coord: vec2<f32>,
}


@group(0) @binding(0) var color_texture: texture_2d<f32>;
@group(0) @binding(1) var<uniform> smaa_info: SmaaInfo;
@group(0) @binding(2) var color_sampler: sampler;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> EdgeDetectionVaryings {
    let varyings = calculate_vertex_varyings(vertex_index);

    var edge_detection_varyings = EdgeDetectionVaryings();
    edge_detection_varyings.position = vec4(varyings.clip_coord, 0.0, 1.0);
    edge_detection_varyings.tex_coord = varyings.tex_coord;

    edge_detection_varyings.offset_0 = smaa_info.rt_metrics.xyxy * vec4(-1.0, 0.0, 0.0, -1.0) +
        varyings.tex_coord.xyxy;
    edge_detection_varyings.offset_1 = smaa_info.rt_metrics.xyxy * vec4(1.0, 0.0, 0.0, 1.0) +
        varyings.tex_coord.xyxy;
    edge_detection_varyings.offset_2 = smaa_info.rt_metrics.xyxy * vec4(-2.0, 0.0, 0.0, -2.0) +
        varyings.tex_coord.xyxy;

    return edge_detection_varyings;
}



@fragment
fn fs_main(in: EdgeDetectionVaryings) -> @location(0) vec4<f32> {
    // Calculate the threshold:
    // TODO: Predication.
    let threshold = vec2(SMAA_THRESHOLD);

    // Calculate luma:
    let weights = vec3(0.2126, 0.7152, 0.0722);
    let L = dot(textureSample(color_texture, color_sampler, in.tex_coord).rgb, weights);

    let Lleft = dot(textureSample(color_texture, color_sampler, in.offset_0.xy).rgb, weights);
    let Ltop  = dot(textureSample(color_texture, color_sampler, in.offset_0.zw).rgb, weights);

    // We do the usual threshold:
    var delta: vec4<f32> = vec4(abs(L - vec2(Lleft, Ltop)), 0.0, 0.0);
    var edges = step(threshold, delta.xy);

    // Then discard if there is no edge:
    if (dot(edges, vec2(1.0)) == 0.0) {
        discard;
    }

    // Calculate right and bottom deltas:
    let Lright  = dot(textureSample(color_texture, color_sampler, in.offset_1.xy).rgb, weights);
    let Lbottom = dot(textureSample(color_texture, color_sampler, in.offset_1.zw).rgb, weights);
    delta = vec4(delta.xy, abs(L - vec2(Lright, Lbottom)));

    // Calculate the maximum delta in the direct neighborhood:
    var max_delta = max(delta.xy, delta.zw);

    // Calculate left-left and top-top deltas:
    let Lleftleft = dot(textureSample(color_texture, color_sampler, in.offset_2.xy).rgb, weights);
    let Ltoptop   = dot(textureSample(color_texture, color_sampler, in.offset_2.zw).rgb, weights);
    delta = vec4(delta.xy, abs(vec2(Lleft, Ltop) - vec2(Lleftleft, Ltoptop)));

    // Calculate the final maximum delta:
    max_delta = max(max_delta.xy, delta.zw);
    let final_delta = max(max_delta.x, max_delta.y);

    // Local contrast adaptation:
    edges *= step(vec2(final_delta), SMAA_LOCAL_CONTRAST_ADAPTATION_FACTOR * delta.xy);

    return vec4(edges, 0.0, 1.0);
}



//-----------------------------------------------------------------------------
// Diagonal Search Functions
