// 网格绘制公共半边（不透明 G-buffer 写入与透明前向共用）：顶点变换 + 材质
// 参数换算。导入方须声明全局 `frame: FrameUniforms`、`objects: array<ObjectRecord>`、
// `models: array<mat4x4f>`、`textures: binding_array<texture_2d<f32>>`、
// `samplers: binding_array<sampler>`。
// #import frame
// #import instance

// Filament desktop 档 MIN_PERCEPTUAL_ROUGHNESS（surface_material.fs）；clamp 在
// 材质换算端做，光照端直用现值。
const MIN_PERCEPTUAL_ROUGHNESS: f32 = 0.045;

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) world_normal: vec3f,
    @location(1) uv: vec2f,
    @location(2) @interpolate(flat) material: u32,
    @location(3) world_pos: vec3f,
}

// 模型的法线矩阵（cofactor 形态）：与逆转置只差行列式倒数这一标量，normalize
// 后消失；免 WGSL 无 inverse() 内建的限制，非均匀缩放正确。
fn normal_cofactor(m: mat4x4f) -> mat3x3f {
    let a = m[0].xyz;
    let b = m[1].xyz;
    let c = m[2].xyz;
    return mat3x3f(cross(b, c), cross(c, a), cross(a, b));
}

// 实例槽 = @builtin(instance_index)：indirect 命令的 firstInstance 携带槽号
//（instanceCount=1 时 instance_index == firstInstance），按槽读 set2 常驻表。
// 列主序全链 + 正高度 viewport 直出（Y 翻转由 naga ADJUST_COORDINATE_SPACE 注入）。
fn mesh_vertex(position: vec3f, normal: vec3f, uv: vec2f, instance_index: u32) -> VsOut {
    let obj = objects[instance_index];
    let model = models[instance_index];
    var out: VsOut;
    let world = model * vec4f(position, 1.0);
    out.clip = frame.view_proj * world;
    out.world_pos = world.xyz;
    out.world_normal = normal_cofactor(model) * normal;
    out.uv = uv;
    out.material = obj.material_index;
    return out;
}

struct SurfaceSample {
    albedo: vec4f,
    metallic: f32,
    perceptual_roughness: f32,
    occlusion: f32,
}

// 材质参数换算（Filament gltfio 同款）：贴图通道乘标量、clamp 收口。mip 钉
// level 0。采样器全记录共享 sampler_index（SamplerKey 去重后同键）。
fn sample_surface(mat: MaterialRecord, uv: vec2f) -> SurfaceSample {
    var s: SurfaceSample;
    s.albedo = textureSampleLevel(textures[mat.tex_base_color], samplers[mat.sampler_index], uv, 0.0)
        * mat.base_color;
    var metallic = mat.metallic;
    var perceptual_roughness = mat.perceptual_roughness;
    if (mat.tex_metallic_roughness != NO_TEXTURE) {
        let mr = textureSampleLevel(
            textures[mat.tex_metallic_roughness], samplers[mat.sampler_index], uv, 0.0);
        // glTF 语义：G 通道 = roughness、B 通道 = metallic
        perceptual_roughness = perceptual_roughness * mr.g;
        metallic = metallic * mr.b;
    }
    s.perceptual_roughness = clamp(perceptual_roughness, MIN_PERCEPTUAL_ROUGHNESS, 1.0);
    s.metallic = clamp(metallic, 0.0, 1.0);
    s.occlusion = 1.0;
    if (mat.tex_occlusion != NO_TEXTURE) {
        // glTF 语义：R 通道 = ambient occlusion
        s.occlusion = textureSampleLevel(
            textures[mat.tex_occlusion], samplers[mat.sampler_index], uv, 0.0).r;
    }
    return s;
}
