// 透明前向绘制（vertex + fragment 双入口）：Clustered Forward 透明层。
// 顶点半边与 gbuffer_write 共用 common/mesh.wgsl（set2 常驻表按 instance_index
// 读，firstInstance 携带槽号）；fragment 做透明行的光照与 alpha 输出——光照与
// 不透明光照累积同光源、同一份 Filament BRDF（common/brdf.wgsl）+ cluster 点光源
// 列表（3D cluster 索引 = 屏幕 tile × 深度片）。
//
// blend 形态（管线端 SRC_ALPHA/ONE_MINUS_SRC_ALPHA）：输出直色（非预乘），
// alpha = 材质 base_color.a。深度 = 不透明 G-buffer 深度附件重绑（loadOp
// LOAD、只测不写——被不透明覆盖的透明片元被 depth test 剔除）。
//
// 接口：set0/set1/set2 与主管线同一冻结表；set3 = 透明族小集
//（b0 ClusterParams / b1 lights / b2 cluster 列表）。
enable wgpu_binding_array;
// #import mesh
// #import cluster_lighting

@group(0) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(0) @binding(1) var samplers: binding_array<sampler>;
@group(1) @binding(0) var<uniform> frame: FrameUniforms;
@group(2) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(2) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(2) @binding(2) var<storage, read> materials: array<MaterialRecord>;
@group(3) @binding(0) var<storage, read> params: ClusterParams;
@group(3) @binding(1) var<storage, read> lights: array<LightRecord>;
@group(3) @binding(2) var<storage, read> clusters: array<ClusterRecord>;

@vertex
fn vs_main(
    @location(0) position: vec3f,
    @location(1) normal: vec3f,
    @location(2) uv: vec2f,
    @builtin(instance_index) instance_index: u32,
) -> VsOut {
    return mesh_vertex(position, normal, uv, instance_index);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    // forward 直读材质记录（不透明层经 G-buffer 中转，两侧换算同源同域）
    let mat = materials[in.material];
    let surf = sample_surface(mat, in.uv);
    let albedo = surf.albedo;
    let n = normalize(in.world_normal);
    let pixel = getStandardPixelParams(albedo.rgb, surf.metallic, surf.perceptual_roughness, mat.reflectance);

    let V = normalize(frame.camera_pos.xyz - in.world_pos);
    let NoV = clampNoV(saturate(dot(n, V)));
    var light_sum = vec3f(0.0);

    // —— 方向光先加、点光源后累加（累加顺序即浮点结果，保持不变）。
    // Filament 方向光形状：NoL × colorIntensity，强度已折进 rgb
    let l = safe_light_dir(frame.dir_to_light.xyz);
    let dir_NoL = saturate(dot(n, l));
    if (dir_NoL > 0.0) {
        light_sum = light_sum + surfaceShading(
            pixel, l, vec4f(frame.light_color.rgb, 1.0), 1.0, dir_NoL, V, n, NoV, surf.occlusion);
    }
    light_sum = accumulate_cluster_lights(
        light_sum, params, in.clip.xy, pixel, in.world_pos, V, n, NoV, surf.occlusion);

    // 环境光项在光源循环外（与不透明合成同款语义）
    let ambient = albedo.rgb * frame.ambient_color.rgb;
    return vec4f((light_sum + ambient) * EXPOSURE, albedo.a);
}
