// SMAA 1x 第一 pass：亮度边缘检测（输出 edges RG）。
// 移植口径与公共段见 common/smaa_common.wgsl。
// #import smaa_common

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
