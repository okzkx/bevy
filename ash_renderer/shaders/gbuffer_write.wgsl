// 8.1 不透明 deferred G-buffer 写入：把"直出颜色"改成"写 G-buffer"——
// albedo（线性反照率）+ normal（世界空间 xyz 直存）+ 深度（reverse-Z）。
// 光照计算从本 shader 退役：Lambert 移到 gbuffer_composite.wgsl（全屏合成），
// 8.2 起换光照累积 pass。vertex 半边与 debug_draw.wgsl 同构（5.3 indirect
// 闭环契约：instance_index 读 set2，push 退役）。
//
// 接口（与 debug_draw 同一 set/binding 冻结表）：
// - set0 b0/b1：纹理数组 + sampler 数组（runtime array，build.rs 补丁器补 capability）。
// - set1 b0：每帧 UBO（FrameUniforms 128B——view_proj 供 vertex 变换；灯光/
//   mode 字段在本 pass 无消费者，留 128B 冻结布局不拆）。
// - set2 b0/b1/b2：对象表/模型环/材质环（与 Rust 镜像静态互证）。
//
// alpha 语义钉明：不透明 pass 不携带 alpha——albedo.a 恒写 1.0（背景判定走
// 合成端的 depth==0，不占用 albedo.a）；alpha<1 的行按现行"不透明调试覆盖"
// 策略照常画出（8.1.3 拆透明层后改由透明层承接）。
enable wgpu_binding_array;

struct FrameUniforms {
    view_proj: mat4x4f,
    dir_to_light: vec4f,
    ambient_color: vec4f,
    light_color: vec4f,
    mode: u32,
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
    tex_index: u32,
    sampler_index: u32,
    _pad: vec2<u32>,
}

@group(0) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(0) @binding(1) var samplers: binding_array<sampler>;
@group(1) @binding(0) var<uniform> frame: FrameUniforms;
@group(2) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(2) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(2) @binding(2) var<storage, read> materials: array<MaterialRecord>;

// 模型的逆转置法线矩阵（cofactor 矩阵形态，与 debug_draw 同源——见其注释）
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
    // 列主序全链 + 正高度 viewport 直出（朝向定案，与 debug_draw 同源）
    out.clip = frame.view_proj * (model * vec4f(position, 1.0));
    out.world_normal = normal_cofactor(model) * normal;
    out.uv = uv;
    out.material = obj.material_index;
    return out;
}

struct GBufferOut {
    @location(0) albedo: vec4f,
    @location(1) normal: vec4f,
}

@fragment
fn fs_main(in: VsOut) -> GBufferOut {
    let mat = materials[in.material];
    let albedo =
        textureSampleLevel(textures[mat.tex_index], samplers[mat.sampler_index], in.uv, 0.0)
            * mat.base_color;
    let n = normalize(in.world_normal);
    return GBufferOut(vec4f(albedo.rgb, 1.0), vec4f(n, 0.0));
}
