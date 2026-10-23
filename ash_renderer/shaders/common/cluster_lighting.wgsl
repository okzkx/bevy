// cluster 点光源累加（不透明光照累积与透明前向共用）。导入方须声明全局
// `frame`（FrameUniforms）、`lights: array<LightRecord>` 与
// `clusters: array<ClusterRecord>`。
// #import brdf
// #import cluster

// 在 acc 上逐灯累加本片元所在 cluster 的点光源贡献（加法顺序 = 调用方既有的
// 累加顺序）。early exit 同 Filament：NoL ≤ 0 || attenuation ≤ 0 不积累。
// 死灯（半径 ≤ 0）cluster_build 已不入列表，此处门卫只是防御。
// pixel_xy = 片元像素坐标（@builtin(position).xy）；视深由 world_pos 现算。
fn accumulate_cluster_lights(acc: vec3f, p: ClusterParams, pixel_xy: vec2f, pixel: PixelParams, world_pos: vec3f, V: vec3f, n: vec3f, NoV: f32, occlusion: f32) -> vec3f {
    var light_sum = acc;
    if (p.light_count > 0u) {
        let view_depth = (frame.view_proj * vec4f(world_pos, 1.0)).w;
        let record = clusters[cluster_index(p, pixel_xy, view_depth)];
        for (var k = 0u; k < record.count; k = k + 1u) {
            let light = lights[record.indices[k]];
            if (light.pos_radius.w <= 0.0) {
                continue;
            }
            let posToLight = light.pos_radius.xyz - world_pos;
            let l = normalize(posToLight);
            let NoL = saturate(dot(n, l));
            let falloff = 1.0 / max(light.pos_radius.w * light.pos_radius.w, 1e-8);
            let attenuation = getDistanceAttenuation(posToLight, falloff);
            if (NoL > 0.0 && attenuation > 0.0) {
                light_sum = light_sum + surfaceShading(
                    pixel, l, light.color_intensity, attenuation, NoL, V, n, NoV, occlusion);
            }
        }
    }
    return light_sum;
}
