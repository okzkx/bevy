// SMAA 1x 第二 pass：混合权重计算（diag/正交搜索 + area/search LUT）。
// 移植口径与公共段见 common/smaa_common.wgsl。
// #import smaa_common

//-----------------------------------------------------------------------------
// Vertex Shaders


struct BlendingWeightCalculationVaryings {
    @builtin(position) position: vec4<f32>,
    @location(0) offset_0: vec4<f32>,
    @location(1) offset_1: vec4<f32>,
    @location(2) offset_2: vec4<f32>,
    @location(3) tex_coord: vec2<f32>,
}


@group(0) @binding(0) var edges_texture: texture_2d<f32>;
@group(0) @binding(1) var<uniform> smaa_info: SmaaInfo;
@group(0) @binding(2) var edges_sampler: sampler;
@group(0) @binding(3) var search_texture: texture_2d<f32>;
@group(0) @binding(4) var area_texture: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32)
        -> BlendingWeightCalculationVaryings {
    let varyings = calculate_vertex_varyings(vertex_index);

    var weight_varyings = BlendingWeightCalculationVaryings();
    weight_varyings.position = vec4(varyings.clip_coord, 0.0, 1.0);
    weight_varyings.tex_coord = varyings.tex_coord;

    // We will use these offsets for the searches later on (see @PSEUDO_GATHER4):
    weight_varyings.offset_0 = smaa_info.rt_metrics.xyxy * vec4(-0.25, -0.125, 1.25, -0.125) +
        varyings.tex_coord.xyxy;
    weight_varyings.offset_1 = smaa_info.rt_metrics.xyxy * vec4(-0.125, -0.25, -0.125, 1.25) +
        varyings.tex_coord.xyxy;

    // And these for the searches, they indicate the ends of the loops:
    weight_varyings.offset_2 =
        smaa_info.rt_metrics.xxyy * vec4(-2.0, 2.0, -2.0, 2.0) * f32(SMAA_MAX_SEARCH_STEPS) +
        vec4(weight_varyings.offset_0.xz, weight_varyings.offset_1.yw);

    return weight_varyings;
}


fn decode_diag_bilinear_access_2(in_e: vec2<f32>) -> vec2<f32> {
    // Bilinear access for fetching 'e' have a 0.25 offset, and we are
    // interested in the R and G edges:
    //
    // +---G---+-------+
    // |   x o R   x   |
    // +-------+-------+
    //
    // Then, if one of these edge is enabled:
    //   Red:   (0.75 * X + 0.25 * 1) => 0.25 or 1.0
    //   Green: (0.75 * 1 + 0.25 * X) => 0.75 or 1.0
    //
    // This function will unpack the values (mad + mul + round):
    // wolframalpha.com: round(x * abs(5 * x - 5 * 0.75)) plot 0 to 1
    var e = in_e;
    e.r = e.r * abs(5.0 * e.r - 5.0 * 0.75);
    return round(e);
}

fn decode_diag_bilinear_access_4(e: vec4<f32>) -> vec4<f32> {
    let e_rb = e.rb * abs(5.0 * e.rb - 5.0 * 0.75);
    return round(vec4(e_rb.x, e.g, e_rb.y, e.a));
}

/**
 * These functions allows to perform diagonal pattern searches.
 */
fn search_diag_1(tex_coord: vec2<f32>, dir: vec2<f32>, e: ptr<function, vec2<f32>>) -> vec2<f32> {
    var coord = vec4(tex_coord, -1.0, 1.0);
    let t = vec3(smaa_info.rt_metrics.xy, 1.0);
    while (coord.z < f32(SMAA_MAX_SEARCH_STEPS_DIAG - 1u) && coord.w > 0.9) {
        coord = vec4(t * vec3(dir, 1.0) + coord.xyz, coord.w);
        *e = textureSampleLevel(edges_texture, edges_sampler, coord.xy, 0.0).rg;
        coord.w = dot(*e, vec2(0.5));
    }
    return coord.zw;
}

fn search_diag_2(tex_coord: vec2<f32>, dir: vec2<f32>, e: ptr<function, vec2<f32>>) -> vec2<f32> {
    var coord = vec4(tex_coord, -1.0, 1.0);
    coord.x += 0.25 * smaa_info.rt_metrics.x; // See @SearchDiag2Optimization
    let t = vec3(smaa_info.rt_metrics.xy, 1.0);
    while (coord.z < f32(SMAA_MAX_SEARCH_STEPS_DIAG - 1u) && coord.w > 0.9) {
        coord = vec4(t * vec3(dir, 1.0) + coord.xyz, coord.w);

        // @SearchDiag2Optimization
        // Fetch both edges at once using bilinear filtering:
        *e = textureSampleLevel(edges_texture, edges_sampler, coord.xy, 0.0).rg;
        *e = decode_diag_bilinear_access_2(*e);

        // Non-optimized version:
        // e.g = SMAASampleLevelZero(edgesTex, coord.xy).g;
        // e.r = SMAASampleLevelZeroOffset(edgesTex, coord.xy, int2(1, 0)).r;

        coord.w = dot(*e, vec2(0.5));
    }
    return coord.zw;
}

/// Similar to SMAAArea, this calculates the area corresponding to a certain
/// diagonal distance and crossing edges 'e'.
fn area_diag(dist: vec2<f32>, e: vec2<f32>, offset: f32) -> vec2<f32> {
    var tex_coord = vec2(SMAA_AREATEX_MAX_DISTANCE_DIAG) * e + dist;

    // We do a scale and bias for mapping to texel space:
    tex_coord = SMAA_AREATEX_PIXEL_SIZE * tex_coord + 0.5 * SMAA_AREATEX_PIXEL_SIZE;

    // Diagonal areas are on the second half of the texture:
    tex_coord.x += 0.5;

    // Move to proper place, according to the subpixel offset:
    tex_coord.y += SMAA_AREATEX_SUBTEX_SIZE * offset;

    // Do it!
    return textureSampleLevel(area_texture, edges_sampler, tex_coord, 0.0).rg;
}

/// This searches for diagonal patterns and returns the corresponding weights.
fn calculate_diag_weights(tex_coord: vec2<f32>, e: vec2<f32>, subsample_indices: vec4<f32>)
        -> vec2<f32> {
    var weights = vec2(0.0, 0.0);

    // Search for the line ends:
    var d = vec4(0.0);
    var end = vec2(0.0);
    if (e.r > 0.0) {
        let d_xz = search_diag_1(tex_coord, vec2(-1.0, 1.0), &end);
        d = vec4(d_xz.x, d.y, d_xz.y, d.w);
        d.x += f32(end.y > 0.9);
    } else {
        d = vec4(0.0, d.y, 0.0, d.w);
    }
    let d_yw = search_diag_1(tex_coord, vec2(1.0, -1.0), &end);
    d = vec4(d.x, d_yw.x, d.y, d_yw.y);

    if (d.x + d.y > 2.0) {  // d.x + d.y + 1 > 3
        // Fetch the crossing edges:
        let coords = vec4(-d.x + 0.25, d.x, d.y, -d.y - 0.25) * smaa_info.rt_metrics.xyxy +
            tex_coord.xyxy;
        var c = vec4(
            textureSampleLevel(edges_texture, edges_sampler, coords.xy, 0.0, vec2(-1, 0)).rg,
            textureSampleLevel(edges_texture, edges_sampler, coords.zw, 0.0, vec2( 1, 0)).rg,
        );
        let c_yxwz = decode_diag_bilinear_access_4(c.xyzw);
        c = c_yxwz.yxwz;

        // Non-optimized version:
        // float4 coords = mad(float4(-d.x, d.x, d.y, -d.y), SMAA_RT_METRICS.xyxy, texcoord.xyxy);
        // float4 c;
        // c.x = SMAASampleLevelZeroOffset(edgesTex, coords.xy, int2(-1,  0)).g;
        // c.y = SMAASampleLevelZeroOffset(edgesTex, coords.xy, int2( 0,  0)).r;
        // c.z = SMAASampleLevelZeroOffset(edgesTex, coords.zw, int2( 1,  0)).g;
        // c.w = SMAASampleLevelZeroOffset(edgesTex, coords.zw, int2( 1, -1)).r;

        // Merge crossing edges at each side into a single value:
        var cc = vec2(2.0) * c.xz + c.yw;

        // Remove the crossing edge if we didn't found the end of the line:
        cc = select(cc, vec2(0.0, 0.0), vec2<bool>(step(vec2(0.9), d.zw)));

        // Fetch the areas for this line:
        weights += area_diag(d.xy, cc, subsample_indices.z);
    }

    // Search for the line ends:
    let d_xz = search_diag_2(tex_coord, vec2(-1.0, -1.0), &end);
    if (textureSampleLevel(edges_texture, edges_sampler, tex_coord, 0.0, vec2(1, 0)).r > 0.0) {
        let d_yw = search_diag_2(tex_coord, vec2(1.0, 1.0), &end);
        d = vec4(d_xz.x, d_yw.x, d_xz.y, d_yw.y);
        d.y += f32(end.y > 0.9);
    } else {
        d = vec4(d_xz.x, 0.0, d_xz.y, 0.0);
    }

    if (d.x + d.y > 2.0) {  // d.x + d.y + 1 > 3
        // Fetch the crossing edges:
        let coords = vec4(-d.x, -d.x, d.y, d.y) * smaa_info.rt_metrics.xyxy + tex_coord.xyxy;
        let c = vec4(
            textureSampleLevel(edges_texture, edges_sampler, coords.xy, 0.0, vec2(-1,  0)).g,
            textureSampleLevel(edges_texture, edges_sampler, coords.xy, 0.0, vec2( 0, -1)).r,
            textureSampleLevel(edges_texture, edges_sampler, coords.zw, 0.0, vec2( 1,  0)).gr,
        );
        var cc = vec2(2.0) * c.xz + c.yw;

        // Remove the crossing edge if we didn't found the end of the line:
        cc = select(cc, vec2(0.0, 0.0), vec2<bool>(step(vec2(0.9), d.zw)));

        // Fetch the areas for this line:
        weights += area_diag(d.xy, cc, subsample_indices.w).gr;
    }

    return weights;
}


//-----------------------------------------------------------------------------
// Horizontal/Vertical Search Functions

/// This allows to determine how much length should we add in the last step
/// of the searches. It takes the bilinearly interpolated edge (see
/// @PSEUDO_GATHER4), and adds 0, 1 or 2, depending on which edges and
/// crossing edges are active.
fn search_length(e: vec2<f32>, offset: f32) -> f32 {
    // The texture is flipped vertically, with left and right cases taking half
    // of the space horizontally:
    var scale = SMAA_SEARCHTEX_SIZE * vec2(0.5, -1.0);
    var bias = SMAA_SEARCHTEX_SIZE * vec2(offset, 1.0);

    // Scale and bias to access texel centers:
    scale += vec2(-1.0,  1.0);
    bias  += vec2( 0.5, -0.5);

    // Convert from pixel coordinates to texcoords:
    // (We use SMAA_SEARCHTEX_PACKED_SIZE because the texture is cropped)
    scale *= 1.0 / SMAA_SEARCHTEX_PACKED_SIZE;
    bias *= 1.0 / SMAA_SEARCHTEX_PACKED_SIZE;

    // Lookup the search texture:
    return textureSampleLevel(search_texture, edges_sampler, scale * e + bias, 0.0).r;
}

/// Horizontal/vertical search functions for the 2nd pass.
fn search_x_left(in_tex_coord: vec2<f32>, end: f32) -> f32 {
    var tex_coord = in_tex_coord;

    /**
     * @PSEUDO_GATHER4
     * This texcoord has been offset by (-0.25, -0.125) in the vertex shader to
     * sample between edge, thus fetching four edges in a row.
     * Sampling with different offsets in each direction allows to disambiguate
     * which edges are active from the four fetched ones.
     */
    var e = vec2(0.0, 1.0);
    while (tex_coord.x > end &&
           e.g > 0.8281 &&  // Is there some edge not activated?
           e.r == 0.0) {    // Or is there a crossing edge that breaks the line?
        e = textureSampleLevel(edges_texture, edges_sampler, tex_coord, 0.0).rg;
        tex_coord += -vec2(2.0, 0.0) * smaa_info.rt_metrics.xy;
    }
    let offset = -(255.0 / 127.0) * search_length(e, 0.0) + 3.25;
    return smaa_info.rt_metrics.x * offset + tex_coord.x;
}

fn search_x_right(in_tex_coord: vec2<f32>, end: f32) -> f32 {
    var tex_coord = in_tex_coord;

    var e = vec2(0.0, 1.0);
    while (tex_coord.x < end &&
           e.g > 0.8281 &&  // Is there some edge not activated?
           e.r == 0.0) {    // Or is there a crossing edge that breaks the line?
        e = textureSampleLevel(edges_texture, edges_sampler, tex_coord, 0.0).rg;
        tex_coord += vec2(2.0, 0.0) * smaa_info.rt_metrics.xy;
    }
    let offset = -(255.0 / 127.0) * search_length(e, 0.5) + 3.25;
    return -smaa_info.rt_metrics.x * offset + tex_coord.x;
}

fn search_y_up(in_tex_coord: vec2<f32>, end: f32) -> f32 {
    var tex_coord = in_tex_coord;

    var e = vec2(1.0, 0.0);
    while (tex_coord.y > end &&
           e.r > 0.8281 &&  // Is there some edge not activated?
           e.g == 0.0) {    // Or is there a crossing edge that breaks the line?
        e = textureSampleLevel(edges_texture, edges_sampler, tex_coord, 0.0).rg;
        tex_coord += -vec2(0.0, 2.0) * smaa_info.rt_metrics.xy;
    }
    let offset = -(255.0 / 127.0) * search_length(e.gr, 0.0) + 3.25;
    return smaa_info.rt_metrics.y * offset + tex_coord.y;
}

fn search_y_down(in_tex_coord: vec2<f32>, end: f32) -> f32 {
    var tex_coord = in_tex_coord;

    var e = vec2(1.0, 0.0);
    while (tex_coord.y < end &&
           e.r > 0.8281 &&  // Is there some edge not activated?
           e.g == 0.0) {    // Or is there a crossing edge that breaks the line?
        e = textureSampleLevel(edges_texture, edges_sampler, tex_coord, 0.0).rg;
        tex_coord += vec2(0.0, 2.0) * smaa_info.rt_metrics.xy;
    }
    let offset = -(255.0 / 127.0) * search_length(e.gr, 0.5) + 3.25;
    return -smaa_info.rt_metrics.y * offset + tex_coord.y;
}

/// Ok, we have the distance and both crossing edges. So, what are the areas
/// at each side of current edge?
fn area(dist: vec2<f32>, e1: f32, e2: f32, offset: f32) -> vec2<f32> {
    // Rounding prevents precision errors of bilinear filtering:
    var tex_coord = SMAA_AREATEX_MAX_DISTANCE * round(4.0 * vec2(e1, e2)) + dist;

    // We do a scale and bias for mapping to texel space:
    tex_coord = SMAA_AREATEX_PIXEL_SIZE * tex_coord + 0.5 * SMAA_AREATEX_PIXEL_SIZE;

    // Move to proper place, according to the subpixel offset:
    tex_coord.y += SMAA_AREATEX_SUBTEX_SIZE * offset;

    // Do it!
    return textureSampleLevel(area_texture, edges_sampler, tex_coord, 0.0).rg;
}

//-----------------------------------------------------------------------------
// Corner Detection Functions

fn detect_horizontal_corner_pattern(weights: vec2<f32>, tex_coord: vec4<f32>, d: vec2<f32>)
        -> vec2<f32> {
    let left_right = step(d.xy, d.yx);
    var rounding = (1.0 - SMAA_CORNER_ROUNDING_NORM) * left_right;

    rounding /= left_right.x + left_right.y; // Reduce blending for pixels in the center of a line.

    var factor = vec2(1.0, 1.0);
    factor.x -= rounding.x *
        textureSampleLevel(edges_texture, edges_sampler, tex_coord.xy, 0.0, vec2(0,  1)).r;
    factor.x -= rounding.y *
        textureSampleLevel(edges_texture, edges_sampler, tex_coord.zw, 0.0, vec2(1,  1)).r;
    factor.y -= rounding.x *
        textureSampleLevel(edges_texture, edges_sampler, tex_coord.xy, 0.0, vec2(0, -2)).r;
    factor.y -= rounding.y *
        textureSampleLevel(edges_texture, edges_sampler, tex_coord.zw, 0.0, vec2(1, -2)).r;

    return weights * saturate(factor);
}

fn detect_vertical_corner_pattern(weights: vec2<f32>, tex_coord: vec4<f32>, d: vec2<f32>)
        -> vec2<f32> {
    let left_right = step(d.xy, d.yx);
    var rounding = (1.0 - SMAA_CORNER_ROUNDING_NORM) * left_right;

    rounding /= left_right.x + left_right.y;

    var factor = vec2(1.0, 1.0);
    factor.x -= rounding.x *
        textureSampleLevel(edges_texture, edges_sampler, tex_coord.xy, 0.0, vec2( 1, 0)).g;
    factor.x -= rounding.y *
        textureSampleLevel(edges_texture, edges_sampler, tex_coord.zw, 0.0, vec2( 1, 1)).g;
    factor.y -= rounding.x *
        textureSampleLevel(edges_texture, edges_sampler, tex_coord.xy, 0.0, vec2(-2, 0)).g;
    factor.y -= rounding.y *
        textureSampleLevel(edges_texture, edges_sampler, tex_coord.zw, 0.0, vec2(-2, 1)).g;

    return weights * saturate(factor);
}

//-----------------------------------------------------------------------------
// Blending Weight Calculation Pixel Shader (Second Pass)

@fragment
fn fs_main(in: BlendingWeightCalculationVaryings)
        -> @location(0) vec4<f32> {
    let subsample_indices = vec4(0.0);  // Just pass zero for SMAA 1x, see @SUBSAMPLE_INDICES.

    var weights = vec4(0.0);

    var e = textureSample(edges_texture, edges_sampler, in.tex_coord).rg;

    if (e.g > 0.0) {    // Edge at north
        // Diagonals have both north and west edges, so searching for them in
        // one of the boundaries is enough.
        weights = vec4(calculate_diag_weights(in.tex_coord, e, subsample_indices), weights.ba);

        // We give priority to diagonals, so if we find a diagonal we skip
        // horizontal/vertical processing.
        if (weights.r + weights.g != 0.0) {
            return weights;
        }

        var d: vec2<f32>;

        // Find the distance to the left:
        var coords: vec3<f32>;
        coords.x = search_x_left(in.offset_0.xy, in.offset_2.x);
        // in.offset_1.y = in.tex_coord.y - 0.25 * smaa_info.rt_metrics.y (@CROSSING_OFFSET)
        coords.y = in.offset_1.y;
        d.x = coords.x;

        // Now fetch the left crossing edges, two at a time using bilinear
        // filtering. Sampling at -0.25 (see @CROSSING_OFFSET) enables to
        // discern what value each edge has:
        let e1 = textureSampleLevel(edges_texture, edges_sampler, coords.xy, 0.0).r;

        // Find the distance to the right:
        coords.z = search_x_right(in.offset_0.zw, in.offset_2.y);
        d.y = coords.z;

        // We want the distances to be in pixel units (doing this here allow to
        // better interleave arithmetic and memory accesses):
        d = abs(round(smaa_info.rt_metrics.zz * d - in.position.xx));

        // SMAAArea below needs a sqrt, as the areas texture is compressed
        // quadratically:
        let sqrt_d = sqrt(d);

        // Fetch the right crossing edges:
        let e2 = textureSampleLevel(
            edges_texture, edges_sampler, coords.zy, 0.0, vec2<i32>(1, 0)).r;

        // Ok, we know how this pattern looks like, now it is time for getting
        // the actual area:
        weights = vec4(area(sqrt_d, e1, e2, subsample_indices.y), weights.ba);

        // Fix corners:
        coords.y = in.tex_coord.y;
        weights = vec4(
            detect_horizontal_corner_pattern(weights.rg, coords.xyzy, d),
            weights.ba
        );
    }

    if (e.r > 0.0) {    // Edge at west
        var d: vec2<f32>;

        // Find the distance to the top:
        var coords: vec3<f32>;
        coords.y = search_y_up(in.offset_1.xy, in.offset_2.z);
        // in.offset_1.x = in.tex_coord.x - 0.25 * smaa_info.rt_metrics.x
        coords.x = in.offset_0.x;
        d.x = coords.y;

        // Fetch the top crossing edges:
        let e1 = textureSampleLevel(edges_texture, edges_sampler, coords.xy, 0.0).g;

        // Find the distance to the bottom:
        coords.z = search_y_down(in.offset_1.zw, in.offset_2.w);
        d.y = coords.z;

        // We want the distances to be in pixel units:
        d = abs(round(smaa_info.rt_metrics.ww * d - in.position.yy));

        // SMAAArea below needs a sqrt, as the areas texture is compressed
        // quadratically:
        let sqrt_d = sqrt(d);

        // Fetch the bottom crossing edges:
        let e2 = textureSampleLevel(
            edges_texture, edges_sampler, coords.xz, 0.0, vec2<i32>(0, 1)).g;

        // Get the area for this direction:
        weights = vec4(weights.rg, area(sqrt_d, e1, e2, subsample_indices.x));

        // Fix corners:
        coords.x = in.tex_coord.x;
        weights = vec4(weights.rg, detect_vertical_corner_pattern(weights.ba, coords.xyxz, d));
    }

    return weights;
}


//-----------------------------------------------------------------------------
// Neighborhood Blending Pixel Shader (Third Pass)
