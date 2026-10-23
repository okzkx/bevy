// 8.1.3 透明前向绘制（vertex + fragment 双入口）：Clustered Forward 透明层。
// 顶点半边与 gbuffer_write 同构（set2 常驻表按 instance_index 读，firstInstance
// 携带槽号）；fragment 做透明行的光照与 alpha 输出——光照 = Filament BRDF
//（8.2.3 起**与不透明光照累积同一份 lobe 函数**，照抄口径逐式同源；判定线
// "透明层与不透明累积同光源同 BRDF 一致"）+ cluster 点光源列表（3D cluster
// 索引 = 屏幕 tile × 深度片，片号由 clip.w 视图深度指数映射）。
//
// blend 形态（管线端 SRC_ALPHA/ONE_MINUS_SRC_ALPHA）：输出直色（非预乘），
// alpha = 材质 base_color.a。深度 = 不透明 G-buffer 深度附件重绑（loadOp
// LOAD、只测不写——被不透明覆盖的透明片元被 depth test 剔除）。
//
// 接口：set0/set1/set2 与主管线同一冻结表；set3 = 透明族小集
//（b0 params[b1] 内 light_count / b1 lights / b2 cluster 列表）。
// cluster 索引公式的像素/分片映射与 cluster_build.wgsl 同式（tile 32px、
// 指数分片、near/far 0.1/200——CLUSTER_* 常量族同源）。
enable wgpu_binding_array;

struct FrameUniforms {
    view_proj: mat4x4f,
    dir_to_light: vec4f,
    ambient_color: vec4f,
    light_color: vec4f,
    mode: u32,
    inv_view_proj: mat4x4f,
    camera_pos: vec4f,  // 8.2.3（UBO 208B）：BRDF V 向量取数
}

struct ObjectRecord {
    first_index: u32,
    index_count: u32,
    vertex_base: u32,
    material_index: u32,
    aabb_min: vec4f,
    aabb_max: vec4f,
}

struct MaterialRecord {
    base_color: vec4f,
    metallic: f32,
    perceptual_roughness: f32,
    reflectance: f32,
    _pad0: u32,
    tex_base_color: u32,
    tex_normal: u32,
    tex_metallic_roughness: u32,
    tex_occlusion: u32,
    sampler_index: u32,
    // 垫至 64 用标量（8.2.6 实抓：vec3<u32> 对齐 16 被排到 64..76，把步长
    // 撑到 80 ≠ 宿主 MATERIAL_RECORD_SIZE 64——材质槽 ≥1 全体错位，缺陷①②
    // 同根；build.rs 布局互证闸门防复发）
    _pad1: u32,
    _pad2: u32,
    _pad3: u32,
}

// 64B 记录（_pad 的成员对齐把步长从 48 撑到 64——Rust 侧 pack 同布局，
// light_table.rs 互证）
struct LightRecord {
    pos_radius: vec4f,       // xyz 世界位置 + w 半径（米；0 = 死灯，门卫跳过）
    color_intensity: vec4f,  // 颜色（rgb 线性）× 强度（w）
    flags: u32,              // bit0 = shadow（8.3 预留）；bits 8..15 = type
    _pad: vec3<u32>,
}

struct ClusterParams {
    view_proj: mat4x4f,
    inv_view_proj: mat4x4f,
    near: f32,
    far: f32,
    tiles_x: u32,
    tiles_y: u32,
    slices: u32,
    light_count: u32,
    extent_w: f32,
    extent_h: f32,
}

struct ClusterRecord {
    count: u32,
    indices: array<u32, 16>,
}

@group(0) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(0) @binding(1) var samplers: binding_array<sampler>;
@group(1) @binding(0) var<uniform> frame: FrameUniforms;
@group(2) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(2) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(2) @binding(2) var<storage, read> materials: array<MaterialRecord>;
@group(3) @binding(0) var<storage, read> params: ClusterParams;
@group(3) @binding(1) var<storage, read> lights: array<LightRecord>;
@group(3) @binding(2) var<storage, read> clusters: array<ClusterRecord>;

const PI: f32 = 3.14159265358979;
const EXPOSURE: f32 = 0.00032;  // 8.4.1 重校准：与 gbuffer_composite 同锚（ACES filmic，直射中灰面 ≈0.5 HDR）
const MAX_PER_CLUSTER: u32 = 16u;
const CLUSTER_TILE: f32 = 32.0;

fn saturate(v: f32) -> f32 {
    return clamp(v, 0.0, 1.0);
}

// —— Filament 标准模型照抄区（与 gbuffer_composite.wgsl 同一份函数逐式同源；
// 对照基准与常量出处见该文件头部注释，此处不再复述）———————————————

fn pow5(v: f32) -> f32 {
    let v2 = v * v;
    return v2 * v2 * v;
}

fn clampNoV(NoV: f32) -> f32 {
    return max(NoV, 1e-4);
}

fn D_GGX(roughness: f32, NoH: f32, h: vec3f) -> f32 {
    let oneMinusNoHSquared = 1.0 - NoH * NoH;
    let a = NoH * roughness;
    let k = min(roughness / (oneMinusNoHSquared + a * a), 453.5);
    return k * (k * (1.0 / PI));
}

fn V_SmithGGXCorrelated(roughness: f32, NoV: f32, NoL: f32) -> f32 {
    let a2 = roughness * roughness;
    let lambdaV = NoL * sqrt((NoV - a2 * NoV) * NoV + a2);
    let lambdaL = NoV * sqrt((NoL - a2 * NoL) * NoL + a2);
    return 0.5 / max(lambdaV + lambdaL, 0.0000077);
}

fn F_Schlick(f0: vec3f, f90: f32, VoH: f32) -> vec3f {
    return f0 + (f90 - f0) * pow5(1.0 - VoH);
}

fn Fd_Lambert() -> f32 {
    return 1.0 / PI;
}

fn getSquareFalloffAttenuation(distanceSquare: f32, falloff: f32) -> f32 {
    let factor = distanceSquare * falloff;
    let smoothFactor = saturate(1.0 - factor * factor);
    return smoothFactor * smoothFactor;
}

fn getDistanceAttenuation(posToLight: vec3f, falloff: f32) -> f32 {
    let distanceSquare = dot(posToLight, posToLight);
    let attenuation = getSquareFalloffAttenuation(distanceSquare, falloff);
    return attenuation / max(distanceSquare, 1e-4);
}

fn computeDiffuseColor(baseColor: vec4f, metallic: f32) -> vec3f {
    return baseColor.rgb * (1.0 - metallic);
}

fn computeF0(baseColor: vec4f, metallic: f32, reflectance: f32) -> vec3f {
    return baseColor.rgb * metallic + (reflectance * (1.0 - metallic));
}

struct PixelParams {
    diffuseColor: vec3f,
    f0: vec3f,
    roughness: f32,
}

// forward 半边：参数从材质记录现读（不透明层经 G-buffer 中转，本层直读记录）
fn getStandardPixelParams(albedo: vec3f, metallic: f32, perceptualRoughness: f32, reflectance: f32) -> PixelParams {
    var pixel: PixelParams;
    pixel.roughness = perceptualRoughness * perceptualRoughness;
    pixel.diffuseColor = computeDiffuseColor(vec4f(albedo, 1.0), metallic);
    pixel.f0 = computeF0(vec4f(albedo, 1.0), metallic, reflectance);
    return pixel;
}

fn surfaceShading(pixel: PixelParams, light_l: vec3f, light_colorIntensity: vec4f, light_attenuation: f32, light_NoL: f32, V: vec3f, n: vec3f, NoV: f32, occlusion: f32) -> vec3f {
    let h = normalize(V + light_l);
    let NoL = light_NoL;
    let NoH = saturate(dot(n, h));
    let LoH = saturate(dot(light_l, h));

    let roughness = pixel.roughness;
    let D = D_GGX(roughness, NoH, h);
    let Vterm = V_SmithGGXCorrelated(roughness, NoV, NoL);
    let f90 = saturate(dot(pixel.f0, vec3f(50.0 * 0.33)));
    let F = F_Schlick(pixel.f0, f90, LoH);
    let Fr = (D * Vterm) * F;

    let Fd = pixel.diffuseColor * Fd_Lambert();

    let color = Fd + Fr;
    return (color * light_colorIntensity.rgb)
        * (light_colorIntensity.w * light_attenuation * NoL * occlusion);
}

fn normal_cofactor(m: mat4x4f) -> mat3x3f {
    let a = m[0].xyz;
    let b = m[1].xyz;
    let c = m[2].xyz;
    return mat3x3f(cross(b, c), cross(c, a), cross(a, b));
}

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) world_normal: vec3f,
    @location(1) uv: vec2f,
    @location(2) @interpolate(flat) material: u32,
    @location(3) world_pos: vec3f,
}

@vertex
fn vs_main(
    @location(0) position: vec3f,
    @location(1) normal: vec3f,
    @location(2) uv: vec2f,
    @builtin(instance_index) instance_index: u32,
) -> VsOut {
    let obj = objects[instance_index];
    let model = models[instance_index];
    var out: VsOut;
    out.clip = frame.view_proj * (model * vec4f(position, 1.0));
    out.world_normal = normal_cofactor(model) * normal;
    out.world_pos = (model * vec4f(position, 1.0)).xyz;
    out.uv = uv;
    out.material = obj.material_index;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let mat = materials[in.material];
    let albedo =
        textureSampleLevel(textures[mat.tex_base_color], samplers[mat.sampler_index], in.uv, 0.0)
            * mat.base_color;
    let n = normalize(in.world_normal);
    // —— 参数换算（forward 直读材质记录，与不透明层 G-buffer 现值同源同域）
    var perceptual_roughness = mat.perceptual_roughness;
    var metallic = mat.metallic;
    if (mat.tex_metallic_roughness != 0xFFFFFFFFu) {
        let mr = textureSampleLevel(
            textures[mat.tex_metallic_roughness], samplers[mat.sampler_index], in.uv, 0.0);
        perceptual_roughness = perceptual_roughness * mr.g;
        metallic = metallic * mr.b;
    }
    perceptual_roughness = clamp(perceptual_roughness, 0.045, 1.0);
    metallic = clamp(metallic, 0.0, 1.0);
    var occlusion = 1.0;
    if (mat.tex_occlusion != 0xFFFFFFFFu) {
        occlusion = textureSampleLevel(
            textures[mat.tex_occlusion], samplers[mat.sampler_index], in.uv, 0.0).r;
    }
    let pixel = getStandardPixelParams(albedo.rgb, metallic, perceptual_roughness, mat.reflectance);

    let V = normalize(frame.camera_pos.xyz - in.world_pos);
    let NoV = clampNoV(saturate(dot(n, V)));
    var light_sum = vec3f(0.0);

    // —— 方向光（Filament 形状：NoL × colorIntensity，强度已折进 rgb）
    let l_raw = frame.dir_to_light.xyz;
    let l = select(vec3f(0.0, 1.0, 0.0), normalize(l_raw), dot(l_raw, l_raw) > 1e-8);
    let dir_NoL = saturate(dot(n, l));
    if (dir_NoL > 0.0) {
        light_sum = light_sum + surfaceShading(
            pixel, l, vec4f(frame.light_color.rgb, 1.0), 1.0, dir_NoL, V, n, NoV, occlusion);
    }

    // —— cluster 点光源（Filament punctual 衰减照抄；early exit 同式）
    if (params.light_count > 0u) {
        let tile = vec2<u32>(in.clip.xy / vec2f(CLUSTER_TILE));
        let tile_i = min(tile.x, params.tiles_x - 1u) + min(tile.y, params.tiles_y - 1u) * params.tiles_x;
        let ratio = params.far / params.near;
        let z = clamp(in.clip.w, params.near, params.far);
        let slice = min(u32(f32(params.slices) * log2(z / params.near) / log2(ratio)), params.slices - 1u);
        let ci = tile_i + slice * params.tiles_x * params.tiles_y;
        let record = clusters[ci];
        for (var k = 0u; k < record.count; k = k + 1u) {
            let li = record.indices[k];
            let light = lights[li];
            if (light.pos_radius.w <= 0.0) {
                continue;  // 死灯门卫（8.2.2）
            }
            let posToLight = light.pos_radius.xyz - in.world_pos;
            let pl = normalize(posToLight);
            let NoL = saturate(dot(n, pl));
            let falloff = 1.0 / max(light.pos_radius.w * light.pos_radius.w, 1e-8);
            let attenuation = getDistanceAttenuation(posToLight, falloff);
            if (NoL > 0.0 && attenuation > 0.0) {
                light_sum = light_sum + surfaceShading(
                    pixel, pl, light.color_intensity, attenuation, NoL, V, n, NoV, occlusion);
            }
        }
    }

    // 环境光项在光源循环外（与不透明合成同款语义）
    let ambient = albedo.rgb * frame.ambient_color.rgb;
    return vec4f((light_sum + ambient) * EXPOSURE, albedo.a);
}

