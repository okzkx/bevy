// 透明前向绘制（vertex + fragment 双入口）：Clustered Forward 透明层。
// 顶点段与 gbuffer_write 共用 common/mesh.wgsl（set2 常驻表按 instance_index
// 读，firstInstance 携带槽号）；fragment 做透明行的光照与 alpha 输出——与不透明
// 光照累积用同样的光源、同一份 BRDF（common/brdf.wgsl）和 cluster 点光源列表
//（3D cluster 索引 = 屏幕 tile × 深度片）。
//
// blend 形态（管线端 SRC_ALPHA/ONE_MINUS_SRC_ALPHA）：输出直色（非预乘），
// alpha = 贴图 a × 材质 base_color.a。深度 = 不透明 G-buffer 深度附件重绑（loadOp
// LOAD、只测不写——被不透明覆盖的透明片元被 depth test 剔除）。
//
// 接口：set0/set1/set2 与 G-buffer 写入管线相同；set3 = 光源分簇集
//（b0 ClusterParams / b1 lights / b2 cluster 列表）；set4 = IBL 集（与环境基线同源，
// 透明层也接 IBL——它不是 GI，9.7 的 SCGI 仍只作用于不透明）。
enable wgpu_binding_array;
// #import mesh
// #import cluster_lighting
// #import ibl

@group(0) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(0) @binding(1) var samplers: binding_array<sampler>;
@group(1) @binding(0) var<uniform> frame: FrameUniforms;
@group(2) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(2) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(2) @binding(2) var<storage, read> materials: array<MaterialRecord>;
@group(3) @binding(0) var<storage, read> params: ClusterParams;
@group(3) @binding(1) var<storage, read> lights: array<LightRecord>;
@group(3) @binding(2) var<storage, read> clusters: array<ClusterRecord>;
// set4 环境光照：b0 预滤波 cube、b1 采样器、b2 DFG LUT、b3 SH-L2 系数、
// b4 GlobalProbe 16 桶（9.3；与不透明同源，透明层只换漫反射编码）
@group(4) @binding(0) var env_cube: texture_cube<f32>;
@group(4) @binding(1) var ibl_sampler: sampler;
@group(4) @binding(2) var dfg_lut: texture_2d<f32>;
@group(4) @binding(3) var<storage, read> sh_coeffs: array<vec4f, 9>;
@group(4) @binding(4) var<storage, read> probe_buckets: array<vec4f, 64>;

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
    // 零法线兜底（同不透明 G-buffer 写入口径）：退化面片不让 NaN 进光照
    let n = safe_normalize(in.world_normal, vec3f(0.0, 1.0, 0.0));
    let pixel = getStandardPixelParams(albedo.rgb, surf.metallic, surf.perceptual_roughness, mat.reflectance);

    let V = normalize(frame.camera_pos.xyz - in.world_pos);
    let NoV = clampNoV(saturate(dot(n, V)));
    var light_sum = vec3f(0.0);

    // —— 方向光先加、点光源后累加（累加顺序即浮点结果，保持不变）。
    // 方向光：NoL × 颜色强度，强度已折进 rgb
    let l = safe_light_dir(frame.dir_to_light.xyz);
    let dir_NoL = saturate(dot(n, l));
    if (dir_NoL > 0.0) {
        light_sum = light_sum + surfaceShading(
            pixel, l, vec4f(frame.light_color.rgb, 1.0), 1.0, dir_NoL, V, n, NoV, surf.occlusion);
    }
    light_sum = accumulate_cluster_lights(
        light_sum, params, in.clip.xy, pixel, in.world_pos, V, n, NoV, surf.occlusion);

    // 环境项在光源循环外（与不透明合成同款语义）：IBL 开 = SH-L2 漫反射 + split-sum
    // 高光（替换旧常量项）；关 = 步骤 8 原文 albedo × ambient。粗糙度用 surf 的
    // perceptualRoughness（不是 α）。自发光同不透明层：按 Bevy StandardMaterial 默认
    // （emissive_exposure_weight = 0）不随曝光缩放，加在曝光乘算之外，仍写进 HDR；
    // alpha 继续由混合状态乘。
    var environment = albedo.rgb * frame.ambient_color.rgb;
    if (frame.ibl_enabled != 0u) {
        // 9.2.8：视向侧法线 + specular AA（与不透明同口径，见 common/ibl.wgsl）
        let n_ibl = ibl_normal(n, V);
        let no_v = clampNoV(saturate(dot(n_ibl, V)));
        let p_ibl = ibl_roughness(n_ibl, surf.perceptual_roughness);
        // 漫反射编码二选一（9.3）：0 = SH-L2、1 = GlobalProbe 16 桶；法线口径同不透明
        var e = sh_irradiance(n_ibl, sh_coeffs);
        if (frame.ibl_mode == 1u) {
            let base = direction_to_bucket_index(n_ibl, 4u) * 4u;
            e = probe16_eval(
                n_ibl,
                probe_buckets[base].xyz,
                probe_buckets[base + 1u].xyz,
                probe_buckets[base + 2u].xyz,
                probe_buckets[base + 3u].xyz);
        }
        let diffuse = pixel.diffuseColor * Fd_Lambert() * e * frame.ibl_intensity;
        let specular = ibl_specular(
            env_cube, ibl_sampler, dfg_lut, n_ibl, V, no_v, pixel.f0, p_ibl,
            IBL_MAX_MIP) * frame.ibl_intensity;
        environment = diffuse + specular;
    }
    return vec4f((light_sum + environment) * EXPOSURE + surf.emissive, albedo.a);
}
