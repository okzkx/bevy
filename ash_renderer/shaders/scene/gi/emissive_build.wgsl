// 9.6.2 自发光三角形表重建（compute）：每线程一个对象槽，把该对象里"自发光材质"的三角形
// 逐条展开成世界空间的记录（p0 / e1 / e2 / 自发光辐亮度 + 面积），条数进 `counters[14]`。
//
// 为什么不做 CDF/前缀和：面积加权采样需要前缀和，而 naga 30 对 workgroup 聚合会发
// Offset/ArrayStride（9.5 defrag 的教训），单线程串扫 32768 条又是毫秒级。首版改为
// **均匀抽三角形 + 解析面积权重**（pdf = 1/(N·area)，估计量 = Le·N·area·cos/d²），
// 无 CDF、无前缀和、无额外屏障；代价是三角形面积差异大时方差高，A/B 与是否补 CDF
// 记在 9.6.5 的读数里（施工记录 9.6.2 有裁决说明）。
//
// 接口：
// - set0：GI 集（b8 计数 / b12 自发光表）
// - set1：实例表（b0 对象 / b1 模型矩阵 / b2 材质环）
// - set2：光线求交集（b1 顶点池 / b2 索引池；不读 TLAS）
// - set3：bindless 贴图（b0 贴图 / b1 采样器，自发光贴图）
// - push（immediate，4B）：对象表容量
enable wgpu_ray_query;
enable wgpu_binding_array;
// #import gi_emissive
// #import instance
// #import raytracing

@group(0) @binding(8) var<storage, read_write> counters: array<atomic<u32>, 16>;
@group(0) @binding(12) var<storage, read_write> emissive_triangles: array<EmissiveTriangle>;
@group(1) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(1) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(1) @binding(2) var<storage, read> materials: array<MaterialRecord>;
@group(2) @binding(0) var scene_tlas: acceleration_structure;
@group(2) @binding(1) var<storage, read> vertices: array<f32>;
@group(2) @binding(2) var<storage, read> indices: array<u32>;
@group(3) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(3) @binding(1) var samplers: binding_array<sampler>;

var<immediate> push: PushEmissiveBuild;
struct PushEmissiveBuild {
    object_capacity: u32,
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let object_slot = gid.x;
    if (object_slot >= push.object_capacity) {
        return;
    }
    let object = objects[object_slot];
    if (object.index_count < 3u) {
        return;
    }
    let material = materials[object.material_index];
    // 纯因子与贴图都为零 = 不发光，整条跳过（Sponza 的绝大多数材质走这里）
    let has_factor = any(material.emissive.rgb > vec3f(0.0));
    if (!has_factor && material.tex_emissive == NO_TEXTURE) {
        return;
    }
    let model = models[object_slot];
    let triangle_count = object.index_count / 3u;

    // 第一遍：只数有效三角形（面积为零的退化三角形跳过）。
    //
    // 9.10 对照修正：原实现对**每个**三角形各做一次全局 `atomicAdd(&counters[14], 1u)`，
    // 于是保留集合取决于线程调度顺序——超过 GI_EMISSIVE_CAPACITY 时，每帧留下的子集不同，
    // 自发光照明整片抖（Unity 在 `EmissiveTriangleAddition.compute:32-33` 里按线程组聚合
    // 一次计数，这里按对象聚合——本实现一个线程一个对象）。
    var valid_count = 0u;
    for (var t = 0u; t < triangle_count; t = t + 1u) {
        let base = object.first_index + t * 3u;
        let i0 = indices[base] + object.vertex_base;
        let i1 = indices[base + 1u] + object.vertex_base;
        let i2 = indices[base + 2u] + object.vertex_base;
        let o0 = i0 * RAY_VERTEX_STRIDE_F32;
        let o1 = i1 * RAY_VERTEX_STRIDE_F32;
        let o2 = i2 * RAY_VERTEX_STRIDE_F32;
        let p0 = (model * vec4f(vec3f(vertices[o0], vertices[o0 + 1u], vertices[o0 + 2u]), 1.0)).xyz;
        let p1 = (model * vec4f(vec3f(vertices[o1], vertices[o1 + 1u], vertices[o1 + 2u]), 1.0)).xyz;
        let p2 = (model * vec4f(vec3f(vertices[o2], vertices[o2 + 1u], vertices[o2 + 2u]), 1.0)).xyz;
        let area = 0.5 * length(cross(p1 - p0, p2 - p0));
        if (area > 0.0) {
            valid_count = valid_count + 1u;
        }
    }
    if (valid_count == 0u) {
        return;
    }
    // 一次预留连续区间：同一对象的三角形在表里连续，池满时留下的是稳定前缀。
    let base_slot = atomicAdd(&counters[14], valid_count);

    var cursor = 0u;
    for (var t = 0u; t < triangle_count; t = t + 1u) {
        let base = object.first_index + t * 3u;
        let i0 = indices[base] + object.vertex_base;
        let i1 = indices[base + 1u] + object.vertex_base;
        let i2 = indices[base + 2u] + object.vertex_base;
        let o0 = i0 * RAY_VERTEX_STRIDE_F32;
        let o1 = i1 * RAY_VERTEX_STRIDE_F32;
        let o2 = i2 * RAY_VERTEX_STRIDE_F32;
        let p0 = (model * vec4f(vec3f(vertices[o0], vertices[o0 + 1u], vertices[o0 + 2u]), 1.0)).xyz;
        let p1 = (model * vec4f(vec3f(vertices[o1], vertices[o1 + 1u], vertices[o1 + 2u]), 1.0)).xyz;
        let p2 = (model * vec4f(vec3f(vertices[o2], vertices[o2 + 1u], vertices[o2 + 2u]), 1.0)).xyz;
        let e1 = p1 - p0;
        let e2 = p2 - p0;
        let area = 0.5 * length(cross(e1, e2));
        if (!(area > 0.0)) {
            continue;
        }
        // 自发光辐亮度：材质因子 × 贴图（UV 取三顶点平均，三角形小时足够）
        let uv = (vec2f(vertices[o0 + 6u], vertices[o0 + 7u])
            + vec2f(vertices[o1 + 6u], vertices[o1 + 7u])
            + vec2f(vertices[o2 + 6u], vertices[o2 + 7u])) / 3.0;
        var radiance = material.emissive.rgb;
        if (material.tex_emissive != NO_TEXTURE) {
            radiance = radiance * textureSampleLevel(
                textures[material.tex_emissive],
                samplers[material.sampler_index],
                uv,
                0.0,
            ).rgb;
        }
        // 超限截断（计数已按整批加过，读数里能看出超了多少；不静默丢进池外地址）
        let slot = base_slot + cursor;
        cursor = cursor + 1u;
        if (slot >= GI_EMISSIVE_CAPACITY) {
            return;
        }
        var record: EmissiveTriangle;
        record.p0 = vec4f(p0, area);
        record.e1 = vec4f(e1, 0.0);
        record.e2 = vec4f(e2, 0.0);
        record.emissive = vec4f(radiance, 0.0);
        emissive_triangles[slot] = record;
    }
}
