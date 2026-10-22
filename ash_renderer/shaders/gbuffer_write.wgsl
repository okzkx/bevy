// 8.1 不透明 deferred G-buffer 写入：把"直出颜色"改成"写 G-buffer"——
// albedo（线性反照率）+ normal（世界空间 xyz 直存）+ matparam（材质标量，
// 8.2.1 扩列）+ 深度（reverse-Z）。光照计算从本 shader 退役：Lambert 移到
// gbuffer_composite.wgsl（全屏合成），8.2 起换光照累积 pass。
//
// 8.2.1 PBR 参数面：材质记录扩列后，本 shader 端完成 Filament gltfio 同款
// 参数换算——metallic_roughness 贴图（G=roughness/B=metallic）乘标量、
// perceptualRoughness clamp 0.045（desktop MIN_PERCEPTUAL_ROUGHNESS）、
// normal map 经派生 TBN（屏幕空间余切框架，无顶点 tangent 依赖——glTF 资产
// 不保证 TANGENT 属性）扰动世界法线、occlusion 贴图 R 通道取 AO；三者进
// matparam 附件（r=metallic/g=AO/b=perceptualRoughness/a=reflectance），光照
// 累积 pass 直读。
//
// 接口（与 debug_draw 同一 set/binding 冻结表）：
// - set0 b0/b1：纹理数组 + sampler 数组（runtime array，build.rs 补丁器补 capability）。
// - set1 b0：每帧 UBO（FrameUniforms 128B——view_proj 供 vertex 变换；灯光/
//   mode 字段在本 pass 无消费者，留 128B 冻结布局不拆）。
// - set2 b0/b1/b2：对象表/模型环/材质环（与 Rust 镜像静态互证，64B 记录）。
//
// 贴图槽哨兵：tex_normal/tex_metallic_roughness/tex_occlusion 缺席时为
// 0xFFFFFFFF（Rust 侧 NO_TEXTURE_INDEX），分支跳过采样；base_color 恒在场
// （缺席走 fallback 白图）。采样器全记录共享 base_color 槽位（SamplerKey
// 去重后同键；差异化采样器需求出现时再扩列，8.2.1 施工记录钉明）。
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

// 64B 材质记录（8.2.1 扩列，与 instance_table.rs MaterialRecordLayout 静态互证：
// base_color @0 / PBR 标量 @16..32 / 贴图槽 @32..48 / 采样器 @48 / 垫至 64）
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

@group(0) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(0) @binding(1) var samplers: binding_array<sampler>;
@group(1) @binding(0) var<uniform> frame: FrameUniforms;
@group(2) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(2) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(2) @binding(2) var<storage, read> materials: array<MaterialRecord>;

const NO_TEXTURE: u32 = 0xFFFFFFFFu;
// Filament desktop 档 MIN_PERCEPTUAL_ROUGHNESS（surface_material.fs；clamp 在
// G-buffer 写入端做，光照累积端直用现值）
const MIN_PERCEPTUAL_ROUGHNESS: f32 = 0.045;

// 模型的逆转置法线矩阵（cofactor 矩阵形态，与 debug_draw 同源——见其注释）
fn normal_cofactor(m: mat4x4f) -> mat3x3f {
    let a = m[0].xyz;
    let b = m[1].xyz;
    let c = m[2].xyz;
    return mat3x3f(cross(b, c), cross(c, a), cross(a, b));
}

// 屏幕空间余切框架（Schüler 2015 "Normal Mapping Without a Precomputed
// Tangent"）：由世界位置/uv 的屏幕空间导数现算 TBN——无顶点 tangent 依赖，
// 任意 glTF 资产可用；导数要求 uniform 控制流（本 shader 无提前 return）。
// 退化守卫：uv 导数零区（平铺 UV 的整数缝/等值像素块）t=b=0 时
// inverseSqrt(0)=inf 与 0×inf 产生 NaN（实抓：全画面黑/白紊乱）——
// 退化侧 T/B 列落 0，最终归一化自然回落几何法线。
fn cotangent_frame(n: vec3f, p: vec3f, uv: vec2f) -> mat3x3f {
    let dp1 = dpdx(p);
    let dp2 = dpdy(p);
    let duv1 = dpdx(uv);
    let duv2 = dpdy(uv);
    let dp2perp = cross(dp2, n);
    let dp1perp = cross(n, dp1);
    let t = dp2perp * duv1.x + dp1perp * duv2.x;
    let b = dp2perp * duv1.y + dp1perp * duv2.y;
    let m = max(dot(t, t), dot(b, b));
    let invmax = select(0.0, inverseSqrt(m), m > 1e-20);
    let t_d = select(vec3f(0.0), normalize(t * invmax), m > 1e-20);
    let b_d = select(vec3f(0.0), normalize(b * invmax), m > 1e-20);
    return mat3x3f(t_d, b_d, n);
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
    // 列主序全链 + 正高度 viewport 直出（朝向定案，与 debug_draw 同源）
    let world = model * vec4f(position, 1.0);
    out.clip = frame.view_proj * world;
    out.world_pos = world.xyz;
    out.world_normal = normal_cofactor(model) * normal;
    out.uv = uv;
    out.material = obj.material_index;
    return out;
}

struct GBufferOut {
    @location(0) albedo: vec4f,
    @location(1) normal: vec4f,
    @location(2) matparam: vec4f,
}

@fragment
fn fs_main(in: VsOut) -> GBufferOut {
    let mat = materials[in.material];
    let albedo =
        textureSampleLevel(textures[mat.tex_base_color], samplers[mat.sampler_index], in.uv, 0.0)
            * mat.base_color;

    // —— PBR 标量换算（Filament gltfio 同款：贴图通道乘标量，clamp 收口）
    var metallic = mat.metallic;
    var perceptual_roughness = mat.perceptual_roughness;
    if (mat.tex_metallic_roughness != NO_TEXTURE) {
        let mr = textureSampleLevel(
            textures[mat.tex_metallic_roughness], samplers[mat.sampler_index], in.uv, 0.0);
        // glTF 语义：G 通道 = roughness、B 通道 = metallic（MetallicRoughness）
        perceptual_roughness = perceptual_roughness * mr.g;
        metallic = metallic * mr.b;
    }
    perceptual_roughness = clamp(perceptual_roughness, MIN_PERCEPTUAL_ROUGHNESS, 1.0);
    metallic = clamp(metallic, 0.0, 1.0);

    var occlusion = 1.0;
    if (mat.tex_occlusion != NO_TEXTURE) {
        // glTF 语义：R 通道 = ambient occlusion
        occlusion = textureSampleLevel(
            textures[mat.tex_occlusion], samplers[mat.sampler_index], in.uv, 0.0).r;
    }

    // —— 法线：几何法线为底，normal map 经派生 TBN 扰动（切线空间 xyz，
    // glTF +Y up 约定：tex*2-1 后直接进 TBN）。
    // TBN 计算必须在**一致控制流**里（dpdx/dpdy 规范要求——分支内调用遇
    // helper invocation 分歧 = 未定义值，实抓过全画面黑/白紊乱）
    let tbn = cotangent_frame(normalize(in.world_normal), in.world_pos, in.uv);
    var n = normalize(in.world_normal);
    if (mat.tex_normal != NO_TEXTURE) {
        let nt = textureSampleLevel(
            textures[mat.tex_normal], samplers[mat.sampler_index], in.uv, 0.0).xyz;
        n = normalize(tbn * (nt * 2.0 - 1.0));
    }

    return GBufferOut(
        vec4f(albedo.rgb, 1.0),
        vec4f(n, 0.0),
        // a = reflectance（介电体 F0 = 0.16·reflectance² 的原值，Filament 默认
        // 0.5；1.0 预留通道就此转正——8.2.3 光照累积直接消费）
        vec4f(metallic, occlusion, perceptual_roughness, mat.reflectance),
    );
}
