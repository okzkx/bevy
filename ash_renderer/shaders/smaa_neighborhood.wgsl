// SMAA 1x 第三 pass：邻域混合（SmaaInfo.flags.x = 0 时权重清零直通）。
// 移植口径与公共段见 common/smaa_common.wgsl。
// #import smaa_common

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
    // SMAA 关（flags.x=0）= 权重清零直通（输出 = 输入）
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
