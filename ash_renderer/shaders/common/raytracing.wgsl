// 射线求交公共片段（9.4 求交器）：所有 ray query 调用收在这一份片段里，换后端只换它 +
// render/rt/ 的宿主模块。入口文件须自带 `enable wgpu_ray_query;`（#import 是文本展开，
// enable 不能进片段），并声明本片段引用的全局名（名字即契约）：
//   scene_tlas: acceleration_structure —— 本帧槽 TLAS
//   objects: array<ObjectRecord>       —— 对象表（记录定义在 common/instance.wgsl）
//   vertices: array<f32>               —— 顶点池（32B 交错 = 8×f32/顶点）
//   indices: array<u32>                —— 索引池
//
// 命中只回"三角形编号"：顶点属性一律回池取（BLAS 不是几何副本）。对象记录的
// first_index 是索引池下标、vertex_base 是 32B 单位，与宿主 MeshSlot 同语义。

// 顶点步长的 f32 数（宿主 VERTEX_STRIDE = 32B，位置 3 + 法线 3 + UV 2）。
const RAY_VERTEX_STRIDE_F32: u32 = 8u;

// 一条射线的命中结果。未命中时 hit = false，其余字段无意义。
struct SceneHit {
    hit: bool,
    // 命中距离（世界单位，沿 dir 的 t）。dir 归一化时即世界距离。
    t: f32,
    // 实例槽号（宿主写进 instanceCustomIndex 的值，不是 TLAS 数组下标）。
    instance_slot: u32,
    // 几何内的三角形序号（一网格一几何，直接是网格三角形号）。
    primitive: u32,
    // 重心坐标 xy，第三分量 = 1 - x - y。
    bary: vec2f,
    front_face: bool,
    // 实例的 模型 → 世界 变换（ray query 自带，TLAS 实例记录里的 3×4）。
    object_to_world: mat4x3f,
}

// 命中三角形的顶点属性（模型局部空间）。
struct HitGeometry {
    position: vec3f,
    normal: vec3f,
    uv: vec2f,
    // 对象记录的材质环下标。
    material_index: u32,
}

// 打一条射线；mask 与实例 mask 按位与（实例恒 0xFF）。
fn trace_scene(origin: vec3f, dir: vec3f, tmin: f32, tmax: f32, mask: u32) -> SceneHit {
    var q: ray_query;
    // flags = 0（无强制不透明/双面覆盖；实例 flags 已关背面剔除）
    rayQueryInitialize(&q, scene_tlas, RayDesc(0u, mask, tmin, tmax, origin, dir));
    // 一直推进到没有候选命中（any-hit 语义在本段不用：全不透明）
    while (rayQueryProceed(&q)) {}
    let inter = rayQueryGetCommittedIntersection(&q);
    rayQueryTerminate(&q);
    var out: SceneHit;
    out.hit = inter.kind != 0u;
    out.t = inter.t;
    out.instance_slot = inter.instance_custom_data;
    out.primitive = inter.primitive_index;
    out.bary = inter.barycentrics;
    out.front_face = inter.front_face;
    out.object_to_world = inter.object_to_world;
    return out;
}

// 命中重建：对象记录 → 三索引 → 三顶点 → 重心插值。读对象表与两个池（storage）。
fn hit_geometry(instance_slot: u32, primitive: u32, bary: vec2f) -> HitGeometry {
    let rec = objects[instance_slot];
    let base = rec.first_index + primitive * 3u;
    let i0 = indices[base] + rec.vertex_base;
    let i1 = indices[base + 1u] + rec.vertex_base;
    let i2 = indices[base + 2u] + rec.vertex_base;
    let o0 = i0 * RAY_VERTEX_STRIDE_F32;
    let o1 = i1 * RAY_VERTEX_STRIDE_F32;
    let o2 = i2 * RAY_VERTEX_STRIDE_F32;
    let w = 1.0 - bary.x - bary.y;
    let p0 = vec3f(vertices[o0], vertices[o0 + 1u], vertices[o0 + 2u]);
    let p1 = vec3f(vertices[o1], vertices[o1 + 1u], vertices[o1 + 2u]);
    let p2 = vec3f(vertices[o2], vertices[o2 + 1u], vertices[o2 + 2u]);
    let n0 = vec3f(vertices[o0 + 3u], vertices[o0 + 4u], vertices[o0 + 5u]);
    let n1 = vec3f(vertices[o1 + 3u], vertices[o1 + 4u], vertices[o1 + 5u]);
    let n2 = vec3f(vertices[o2 + 3u], vertices[o2 + 4u], vertices[o2 + 5u]);
    let uv0 = vec2f(vertices[o0 + 6u], vertices[o0 + 7u]);
    let uv1 = vec2f(vertices[o1 + 6u], vertices[o1 + 7u]);
    let uv2 = vec2f(vertices[o2 + 6u], vertices[o2 + 7u]);
    var g: HitGeometry;
    g.position = p0 * bary.x + p1 * bary.y + p2 * w;
    // 插值后不归一化由调用方决定（法线插值本身就是近似的）
    g.normal = n0 * bary.x + n1 * bary.y + n2 * w;
    g.uv = uv0 * bary.x + uv1 * bary.y + uv2 * w;
    g.material_index = rec.material_index;
    return g;
}

// 命中点世界位置：局部插值位置经实例变换。
fn hit_world_pos(hit: SceneHit) -> vec3f {
    let g = hit_geometry(hit.instance_slot, hit.primitive, hit.bary);
    return (hit.object_to_world * vec4f(g.position, 1.0)).xyz;
}

// 命中点世界法线：局部插值法线经实例变换（非均匀缩放下不精确，GI 只要方向）。
fn hit_world_normal(hit: SceneHit) -> vec3f {
    let g = hit_geometry(hit.instance_slot, hit.primitive, hit.bary);
    return (hit.object_to_world * vec4f(g.normal, 0.0)).xyz;
}

// 命中点 UV（与 G-buffer 同一套网格 UV）。
fn hit_uv(hit: SceneHit) -> vec2f {
    return hit_geometry(hit.instance_slot, hit.primitive, hit.bary).uv;
}
