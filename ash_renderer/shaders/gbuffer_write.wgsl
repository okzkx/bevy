// 不透明 deferred G-buffer 写入：albedo（线性反照率）+ normal（世界空间 xyz
// 直存）+ matparam（r=metallic / g=AO / b=perceptualRoughness / a=reflectance）
// + 深度（reverse-Z）。光照在 gbuffer_composite.wgsl（全屏光照累积）。
//
// 材质换算（common/mesh.wgsl sample_surface，Filament gltfio 同款）：
// metallic_roughness 贴图乘标量、perceptualRoughness clamp 0.045、occlusion 贴图
// R 通道取 AO；normal map 经派生 TBN（屏幕空间余切框架，无顶点 tangent 依赖——
// glTF 资产不保证 TANGENT 属性）扰动世界法线。
//
// 接口：
// - set0 b0/b1：纹理数组 + sampler 数组（runtime array，build.rs 补丁器补 capability）。
// - set1 b0：每帧 UBO（FrameUniforms 208B；本 pass 只消费 view_proj）。
// - set2 b0/b1/b2：对象表/模型环/材质环（记录尺寸见 common/instance.wgsl）。
//
// alpha 语义：不透明 pass 不携带 alpha——albedo.a 恒写 1.0（背景判定走合成端的
// depth==0）；alpha<1 的行由透明层承接（build_indirect 对其 instanceCount=0）。
enable wgpu_binding_array;
// #import mesh

@group(0) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(0) @binding(1) var samplers: binding_array<sampler>;
@group(1) @binding(0) var<uniform> frame: FrameUniforms;
@group(2) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(2) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(2) @binding(2) var<storage, read> materials: array<MaterialRecord>;

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

@vertex
fn vs_main(
    @location(0) position: vec3f,
    @location(1) normal: vec3f,
    @location(2) uv: vec2f,
    @builtin(instance_index) instance_index: u32,
) -> VsOut {
    return mesh_vertex(position, normal, uv, instance_index);
}

struct GBufferOut {
    @location(0) albedo: vec4f,
    @location(1) normal: vec4f,
    @location(2) matparam: vec4f,
}

@fragment
fn fs_main(in: VsOut) -> GBufferOut {
    let mat = materials[in.material];
    let surf = sample_surface(mat, in.uv);

    // —— 法线：几何法线为底，normal map 经派生 TBN 扰动（切线空间 xyz，
    // glTF +Y up 约定：tex*2-1 后直接进 TBN）。
    // 坑点：TBN 计算必须在一致控制流里（dpdx/dpdy 规范要求——分支内调用遇
    // helper invocation 分歧 = 未定义值，实抓过全画面黑/白紊乱）
    let tbn = cotangent_frame(normalize(in.world_normal), in.world_pos, in.uv);
    var n = normalize(in.world_normal);
    if (mat.tex_normal != NO_TEXTURE) {
        let nt = textureSampleLevel(
            textures[mat.tex_normal], samplers[mat.sampler_index], in.uv, 0.0).xyz;
        n = normalize(tbn * (nt * 2.0 - 1.0));
    }

    return GBufferOut(
        vec4f(surf.albedo.rgb, 1.0),
        vec4f(n, 0.0),
        // a = reflectance 原值（介电体 F0 = 0.16·reflectance² 在光照端换算）
        vec4f(surf.metallic, surf.occlusion, surf.perceptual_roughness, mat.reflectance),
    );
}
