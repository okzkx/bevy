// 9.6.2 光源侧采样清单（compute）：每线程从"灯位 → 球面均匀方向"打一条射线，命中就记下
// 命中点位置 / 法线 / 反照率 / 距离 / 1-pdf / 灯号，供 estimation 的重连接取样。
//
// 为什么是清单：成本与 patch 数解耦——清单条数与 patch 数无关，全场共享（Unity 的
// `PunctualLightSampling.compute` 同款）。清单条数由宿主给（`push.sample_count`），每条
// 线程按 `idx % light_count` 轮流摊到各盏灯上，所以 pdf 是 `1/(4π · light_count)`，
// 1-pdf 就写进记录的 `hit_normal.w`。
//
// 点光近似：`LightRecord` 只有 pos + 作用半径，没有锥角 → 按"全向点光"均匀球面采样；
// 聚光的锥角缺口记在 9.8 的局限清单里（见段施工记录 9.6.2 与 §5 待决）。
//
// 接口：
// - set0：GI 集（b9 GI UBO 取帧号；b10 光源紧凑表；b11 本清单）
// - set1：实例表（b0 对象记录 / b2 材质环）
// - set2：光线求交集（b0 TLAS / b1 顶点池 / b2 索引池）
// - set3：bindless 贴图（b0 贴图 / b1 采样器，取命中点反照率）
// - push（immediate，8B）：清单条数 + 活灯数
enable wgpu_ray_query;
enable wgpu_binding_array;
// #import gi_cell
// #import gi_sampling
// #import cluster
// #import gi_light_sample
// #import instance
// #import raytracing

@group(0) @binding(9) var<uniform> gi: GiUniforms;
@group(0) @binding(10) var<storage, read> gi_lights: array<LightRecord>;
@group(0) @binding(11) var<storage, read_write> light_samples: array<LightSample>;
@group(1) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(1) @binding(2) var<storage, read> materials: array<MaterialRecord>;
@group(2) @binding(0) var scene_tlas: acceleration_structure;
@group(2) @binding(1) var<storage, read> vertices: array<f32>;
@group(2) @binding(2) var<storage, read> indices: array<u32>;
@group(3) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(3) @binding(1) var samplers: binding_array<sampler>;

var<immediate> push: PushLightSampling;
struct PushLightSampling {
    // 本帧要采的条数（≤ 清单容量）。
    sample_count: u32,
    // 活灯条数（光源紧凑表里 0..light_count 有效）。
    light_count: u32,
}

const RAY_MASK: u32 = 0xFFu;
// 从灯位出发的起点抬升：灯自身不是几何体，给一个极小值只为避开数值贴面
const LIGHT_RAY_TMIN: f32 = 0.01;

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let idx = gid.x;
    if (idx >= push.sample_count) {
        return;
    }
    var record: LightSample;
    record.hit_pos = vec4f(0.0);
    record.hit_normal = vec4f(0.0);
    record.albedo = vec4f(0.0);
    record.info = vec4u(0u, 0u, 0u, 0u);

    let light_count = push.light_count;
    if (light_count == 0u) {
        light_samples[idx] = record;
        return;
    }
    let light_index = idx % light_count;
    let light = gi_lights[light_index];
    // 死灯（半径 ≤ 0）也占一条线程，但产出无效记录
    if (light.pos_radius.w <= 0.0) {
        light_samples[idx] = record;
        return;
    }

    // 序列：按"清单下标 + 帧号"打散，保证每帧换新方向而同一帧内各条互不重复
    var rng = gi_r2_init(low_bias_hash_32(idx), gi.frame.x);
    let u = gi_r2_sample2(rng, 0u);
    let z = u.x * 2.0 - 1.0;
    let r = sqrt(max(0.0, 1.0 - z * z));
    let phi = GI_TWO_PI * u.y;
    let direction = vec3f(r * cos(phi), r * sin(phi), z);

    let origin = light.pos_radius.xyz;
    let hit = trace_scene(origin, direction, LIGHT_RAY_TMIN, light.pos_radius.w, RAY_MASK);
    if (!hit.hit) {
        light_samples[idx] = record;
        return;
    }
    let geo = hit_geometry(hit.instance_slot, hit.primitive, hit.bary);
    let world_pos = (hit.object_to_world * vec4f(geo.position, 1.0)).xyz;
    var world_normal = (hit.object_to_world * vec4f(geo.normal, 0.0)).xyz;
    let len_n = length(world_normal);
    if (len_n <= 0.0) {
        light_samples[idx] = record;
        return;
    }
    world_normal = world_normal / len_n;
    // 法线翻到与射线同侧（与 Unity 的 hitNormal 口径一致：后续 cos 项都按"朝向灯侧"算）
    if (dot(direction, world_normal) >= 0.0) {
        world_normal = -world_normal;
    }
    let material = materials[geo.material_index];
    var albedo = material.base_color.rgb;
    if (material.tex_base_color != NO_TEXTURE) {
        albedo = albedo * textureSampleLevel(
            textures[material.tex_base_color],
            samplers[material.sampler_index],
            geo.uv,
            0.0,
        ).rgb;
    }

    record.hit_pos = vec4f(world_pos, hit.t);
    record.hit_normal = vec4f(world_normal, GI_SPHERE_PDF_4PI * f32(light_count));
    record.albedo = vec4f(albedo, 0.0);
    record.info = vec4u(light_index, 1u, 0u, 0u);
    light_samples[idx] = record;
}
