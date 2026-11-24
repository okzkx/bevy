// 求交调试视图（compute 单入口）：每像素一条主射线 → inline ray query → 命中重建，
// 写 RGBA16F 调试图 + 原子计数。独立 compute，不接任何绘制 pass——含 RayQueryKHR 的
// shader 在非 RT 设备上建不了管线，塞进后链会破坏"RT 关 = 9.3 逐位一致"。
//
// 接口：
// - set0 b0 对象表（命中重建读记录；48B ObjectRecord）
// - set1 b0 每帧 UBO（FrameUniforms：inv_view_proj / camera_pos）
// - set2 b0 TLAS（acceleration_structure）/ b1 顶点池 / b2 索引池 /
//   b3 调试图（rgba16float storage）/ b4 统计（atomic u32：0=射线数 1=命中数）
// - push：width / height / view（0 距离色阶 / 1 世界法线 / 2 实例槽 hash）
//
// 主射线：像素 → uv → NDC（y 翻转与合成 pass 的 world_from_depth 同式）→ reverse-Z
// 近平面点（ndc.z=1；ndc.z=0 是无限远点，w=0 反投影出 NaN）→ 方向 = 近点 - 相机。
enable wgpu_ray_query;
// #import frame
// #import instance
// #import raytracing

@group(0) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(1) @binding(0) var<uniform> frame: FrameUniforms;
@group(2) @binding(0) var scene_tlas: acceleration_structure;
@group(2) @binding(1) var<storage, read> vertices: array<f32>;
@group(2) @binding(2) var<storage, read> indices: array<u32>;
@group(2) @binding(3) var debug_out: texture_storage_2d<rgba16float, write>;
@group(2) @binding(4) var<storage, read_write> stats: array<atomic<u32>>;

// 未命中背景色（与 GI 无关，只为读图区分）。
const MISS_COLOR: vec3f = vec3f(0.05, 0.08, 0.12);
// 距离色阶的远界（世界单位，米）：t ≥ 该值画全黑。
const DIST_FAR_M: f32 = 40.0;

var<immediate> push: PushDebug;
struct PushDebug {
    width: u32,
    height: u32,
    // 子视图：0 = 命中距离 t 色阶，1 = 世界法线，2 = 实例槽 hash。
    view: u32,
    _pad: u32,
}

// 实例槽 → 三通道伪色（相邻槽色差明显，用于核对"哪个实例被命中"）。
fn slot_color(slot: u32) -> vec3f {
    let h = slot * 2654435761u;
    return vec3f(
        f32((h >> 16u) & 0xFFu) / 255.0,
        f32((h >> 8u) & 0xFFu) / 255.0,
        f32(h & 0xFFu) / 255.0,
    );
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let px = gid.x;
    if (px >= push.width * push.height) {
        return;
    }
    let x = px % push.width;
    let y = px / push.width;
    let uv = (vec2f(f32(x), f32(y)) + vec2f(0.5)) / vec2f(f32(push.width), f32(push.height));
    // 与 gbuffer_composite 的 world_from_depth 同式：uv → shader-NDC
    let ndc = vec3f(uv.x * 2.0 - 1.0, 1.0 - 2.0 * uv.y, 1.0);
    let edge = frame.inv_view_proj * vec4f(ndc, 1.0);
    let origin = frame.camera_pos.xyz;
    let dir = normalize(edge.xyz / edge.w - origin);
    let hit = trace_scene(origin, dir, 0.01, 1.0e30, 0xFFu);
    atomicAdd(&stats[0], 1u);
    var color = MISS_COLOR;
    if (hit.hit) {
        atomicAdd(&stats[1], 1u);
        if (push.view == 0u) {
            let c = clamp(hit.t / DIST_FAR_M, 0.0, 1.0);
            color = vec3f(1.0 - c);
        } else if (push.view == 1u) {
            let n = hit_world_normal(hit);
            color = normalize(n) * 0.5 + vec3f(0.5);
        } else {
            color = slot_color(hit.instance_slot);
        }
    }
    textureStore(debug_out, vec2i(i32(x), i32(y)), vec4f(color, 1.0));
}
