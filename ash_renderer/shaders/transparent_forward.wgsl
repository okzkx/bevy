// 8.1.3 透明前向绘制（vertex + fragment 双入口）：Clustered Forward 透明层。
// 顶点半边与 gbuffer_write 同构（set2 常驻表按 instance_index 读，firstInstance
// 携带槽号）；fragment 做透明行的光照与 alpha 输出——光照 = 方向光 Lambert +
// 环境光（与不透明合成同公式，判定线②"同场景光照一致"）+ cluster 点光源列表
//（3D cluster 索引 = 屏幕 tile × 深度片，片号由 clip.w 视图深度指数映射）。
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
    _pad: vec3<u32>,
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

const INV_PI: f32 = 0.3183099;
const EXPOSURE: f32 = 0.0010019;
const MAX_PER_CLUSTER: u32 = 16u;
const CLUSTER_TILE: f32 = 32.0;

fn saturate(v: f32) -> f32 {
    return clamp(v, 0.0, 1.0);
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
    // —— 光照：方向光 + 环境（与不透明合成同公式同常量，判定线②）
    let l_raw = frame.dir_to_light.xyz;
    let l = select(vec3f(0.0, 1.0, 0.0), normalize(l_raw), dot(l_raw, l_raw) > 1e-8);
    var light_sum = frame.light_color.rgb * max(dot(n, l), 0.0) * INV_PI + frame.ambient_color.rgb;
    // —— cluster 点光源：屏幕 tile + 深度片 → cluster 索引 → 光源列表
    if (params.light_count > 0u) {
        let tile = vec2<u32>(in.clip.xy / vec2f(CLUSTER_TILE));
        let tile_i = min(tile.x, params.tiles_x - 1u) + min(tile.y, params.tiles_y - 1u) * params.tiles_x;
        // 视图深度（clip.w）→ 指数分片（与 cluster_build/CPU 参照同式）
        let ratio = params.far / params.near;
        let z = clamp(in.clip.w, params.near, params.far);
        let slice = min(u32(f32(params.slices) * log2(z / params.near) / log2(ratio)), params.slices - 1u);
        let ci = tile_i + slice * params.tiles_x * params.tiles_y;
        let record = clusters[ci];
        for (var k = 0u; k < record.count; k = k + 1u) {
            let li = record.indices[k];
            let to_light = lights[li].pos_radius.xyz - in.world_pos;
            let d2 = dot(to_light, to_light);
            let r = lights[li].pos_radius.w;
            // 平方衰减窗（Unreal 族）：作用域外硬截断 = 光源球语义
            let atten = select(0.0, pow(saturate(1.0 - d2 / (r * r)), 2.0), d2 < r * r);
            if (atten > 0.0) {
                let dir = normalize(to_light);
                light_sum = light_sum + lights[li].color_intensity.rgb
                    * max(dot(n, dir), 0.0) * INV_PI * atten;
            }
        }
    }
    return vec4f(albedo.rgb * light_sum * EXPOSURE, albedo.a);
}

