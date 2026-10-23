// 8.5 SMAA（官方 1x 移植：源 = bevy_anti_alias smaa.wesl @ 0.20-dev，MIT，
// (C) 2013 Jorge Jimenez et al.——上游版权头见仓库原文件）。条件编译展开口径：
// SMAA_PRESET_HIGH + corner/diag 全开 + 无 predication/reprojection。
// 接线差异：绑定随 ash_renderer 约定重排（set0 共享：b0 输入纹理 / b1
// SmaaInfo / b2+ 采样器与 LUT），单文件单入口（vs_main/fs_main）。

struct SmaaInfo {
    rt_metrics: vec4<f32>,
    // x = SMAA 开关（0/1）——关 = 混合权重清零直通（画面回 8.4 基线）
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


fn calculate_vertex_varyings(vertex_index: u32) -> VertexVaryings {
    // See the explanation above for how this works
    let uv = vec2<f32>(f32(vertex_index >> 1u), f32(vertex_index & 1u)) * 2.0;
    let clip_position = vec2<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0));

    return VertexVaryings(clip_position, uv);
}

//-----------------------------------------------------------------------------
// Vertex Shaders


struct NeighborhoodBlendingVaryings {
    @builtin(position) position: vec4<f32>,
    @location(0) offset: vec4<f32>,
    @location(1) tex_coord: vec2<f32>,
}


@group(0) @binding(0) var color_texture: texture_2d<f32>;
@group(0) @binding(1) var<uniform> smaa_info: SmaaInfo;
@group(0) @binding(2) var color_sampler: sampler;
@group(0) @binding(3) var blend_texture: texture_2d<f32>;
@group(0) @binding(4) var blend_sampler: sampler;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32)
        -> NeighborhoodBlendingVaryings {
    let varyings = calculate_vertex_varyings(vertex_index);
    let offset = smaa_info.rt_metrics.xyxy * vec4(1.0, 0.0, 0.0, 1.0) + varyings.tex_coord.xyxy;
    return NeighborhoodBlendingVaryings(
        vec4(varyings.clip_coord, 0.0, 1.0),
        offset,
        varyings.tex_coord
    );
}


//-----------------------------------------------------------------------------
// Edge Detection Pixel Shaders (First Pass)


@fragment
fn fs_main(in: NeighborhoodBlendingVaryings) -> @location(0) vec4<f32> {
    // Fetch the blending weights for current pixel:
    // SMAA 关（flags.x=0）= 权重清零直通（写后内容与 8.4 基线一致）
    let a = vec4(
        textureSample(blend_texture, blend_sampler, in.offset.xy).a,    // Right
        textureSample(blend_texture, blend_sampler, in.offset.zw).g,    // Top
        textureSample(blend_texture, blend_sampler, in.tex_coord).zx,   // Bottom / Left
    ) * select(0.0, 1.0, smaa_info.flags.x > 0.5);

    // Is there any blending weight with a value greater than 0.0?
    if (dot(a, vec4(1.0)) < 1.0e-5) {
        let color = textureSampleLevel(color_texture, blend_sampler, in.tex_coord, 0.0);
        // TODO: Reprojection
        return color;
    } else {
        let h = max(a.x, a.z) > max(a.y, a.w);  // max(horizontal) > max(vertical)

        // Calculate the blending offsets:
        var blending_offset = vec4(0.0, a.y, 0.0, a.w);
        var blending_weight = a.yw;
        blending_offset = select(blending_offset, vec4(a.x, 0.0, a.z, 0.0), h);
        blending_weight = select(blending_weight, a.xz, h);
        blending_weight /= dot(blending_weight, vec2(1.0));

        // Calculate the texture coordinates:
        let blending_coord =
            blending_offset * vec4(smaa_info.rt_metrics.xy, -smaa_info.rt_metrics.xy) +
            in.tex_coord.xyxy;

        // We exploit bilinear filtering to mix current pixel with the chosen
        // neighbor:
        var color = blending_weight.x *
            textureSampleLevel(color_texture, blend_sampler, blending_coord.xy, 0.0);
        color += blending_weight.y *
            textureSampleLevel(color_texture, blend_sampler, blending_coord.zw, 0.0);

        // TODO: Reprojection

        return color;
    }
}
