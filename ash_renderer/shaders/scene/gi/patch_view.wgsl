// 9.5 patch 调试视图（两种形态，只读缓存表与 G-buffer，不改缓存）：
// - 形态 A `fs_cover`（主）：全屏 pass，每个像素按自己的深度与平面法线算
//   `级联 → 体素 → 桶 → cell → patch`，把缓存数据直接铺在可见网格表面上——覆盖空洞、
//   位置偏离、桶条纹、池水位都以颜色直出。与 9.7 的查表端是同一组 gi_cell 函数。
// - 形态 B `vs_marker`/`fs_marker`：一次实例化 draw，每个池槽一个小方片（位置取 patch、
//   朝向取法线、边长 = 该级体素 1/4），未用/陈旧槽输出退化三角形。深度用人工测试：
//   片段窗口深度 vs 场景深度（reverse-Z 下更大 = 更近），方片沿法线朝相机一侧偏移
//   半个体素避免与表面 z-fighting。
//
// 视图号（push.view）：0 关 / 1 覆盖 / 2 级联 / 3 桶 / 4 rank / 5 心跳 /
// 6 池水位与失败计数（屏幕顶部 5% 条）/ 7 位置偏离（判定线 ① 的阈值视图）/ 8 cell 归属。
// 只读不写：调试件不得改变被测状态（不刷心跳、不分配）。
//
// 接口：set0 帧 UBO / set1 G-buffer（b1 normal、b3 depth）/ set2 GI 表 + UBO；
// push（var<immediate>，16B）：视图号 + 法线来源 + 屏幕宽高。
// #import frame
// #import fullscreen
// #import gi_cell

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(1) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(2) @binding(0) var<storage, read> cell_patch: array<u32>;
@group(2) @binding(2) var<storage, read> patch_geometry: array<PatchGeometry>;
@group(2) @binding(5) var<storage, read> patch_statistics: array<PatchStatistics>;
@group(2) @binding(6) var<storage, read> patch_cell: array<u32>;
@group(2) @binding(7) var<storage, read> ring: array<u32, 12>;
@group(2) @binding(8) var<storage, read> counters: array<u32, 16>;
@group(2) @binding(9) var<uniform> gi: GiUniforms;

struct PushView {
    view: u32,
    normal_source: u32,
    width: u32,
    height: u32,
}
var<immediate> push: PushView;

// 背景/条底：暗蓝灰（display 域，输出前统一转线性）。
const VIEW_BACKGROUND: vec3f = vec3f(0.05, 0.06, 0.09);
const VIEW_OUTSIDE_VOLUME: vec3f = vec3f(0.15, 0.30, 0.95);

// 像素 → (级联, cell, 世界位置, 平面法线)。任一环节无效返回 valid = false。
struct SurfaceHit {
    valid: bool,
    outside_volume: bool,
    cascade: u32,
    cell: u32,
    world: vec3f,
    normal: vec3f,
    voxel: f32,
}

fn resolve_surface(pixel: vec2i, uv: vec2f) -> SurfaceHit {
    var hit: SurfaceHit;
    hit.valid = false;
    hit.outside_volume = false;
    hit.cascade = 0u;
    hit.cell = 0u;
    hit.world = vec3f(0.0);
    hit.normal = vec3f(0.0);
    hit.voxel = 1.0;

    let depth = textureLoad(depth_tex, pixel, 0).r;
    if (depth <= 0.0) {
        return hit;
    }
    let full = vec2f(f32(push.width), f32(push.height));
    let p = world_from_depth(uv, depth, frame.inv_view_proj);
    var n = vec3f(0.0);
    if (push.normal_source == 1u) {
        n = textureLoad(normal_tex, pixel, 0).xyz;
    } else {
        let texel = vec2f(1.0) / full;
        let d_left = textureLoad(depth_tex, pixel + vec2i(-1, 0), 0).r;
        let d_right = textureLoad(depth_tex, pixel + vec2i(1, 0), 0).r;
        let d_up = textureLoad(depth_tex, pixel + vec2i(0, -1), 0).r;
        let d_down = textureLoad(depth_tex, pixel + vec2i(0, 1), 0).r;
        n = flat_normal(
            p, uv, texel, depth, d_left, d_right, d_down, d_up,
            frame.inv_view_proj, frame.camera_pos.xyz,
        );
    }
    if (dot(n, n) < 1e-8) {
        return hit;
    }
    hit.world = p;
    hit.normal = normalize(n);

    let center = gi.target_pos.xyz;
    let res = u32(gi.volume.y);
    let cascades = u32(gi.volume.z);
    let cascade = resolve_cascade(center, p, gi.volume.x, res, cascades);
    if (cascade == INVALID_PATCH) {
        hit.outside_volume = true;
        return hit;
    }
    let voxel = voxel_size(gi.volume.x, cascade);
    let offset = gi.cascade_offsets[cascade].xyz;
    let pv = volume_position(p, voxel, offset, i32(res));
    let storage = volume_to_storage(pv, offset, i32(res));
    let bucket = direction_to_bucket_index(hit.normal, BUCKET_RESOLUTION);
    hit.cascade = cascade;
    hit.cell = cell_index(cascade, storage, bucket, res);
    hit.voxel = voxel;
    hit.valid = true;
    return hit;
}

// 顶部状态条（视图 6）：左 90% = 池占用（active / capacity），右 10% = 本帧分配失败
// 计数（以 256 为满标）。条外区域由调用方画别的。
fn status_bar(pixel_xy: vec2f) -> vec3f {
    let height = f32(push.height);
    if (pixel_xy.y >= height * 0.05) {
        return VIEW_BACKGROUND;
    }
    let x01 = clamp(pixel_xy.x / max(f32(push.width), 1.0), 0.0, 1.0);
    let capacity = max(f32(u32(gi.volume.w)), 1.0);
    if (x01 < 0.9) {
        let load = f32(ring[1]) / capacity;
        let fill = x01 / 0.9 < load;
        return select(vec3f(0.18, 0.20, 0.24), vec3f(0.20, 0.85, 0.30), fill);
    }
    let failed = clamp(f32(counters[1]) / 256.0, 0.0, 1.0);
    let fill = (x01 - 0.9) / 0.1 < failed;
    return select(vec3f(0.18, 0.20, 0.24), vec3f(0.95, 0.20, 0.15), fill);
}

@fragment
fn fs_cover(in: VsOut) -> @location(0) vec4f {
    let pixel_xy = in.clip.xy;
    let pixel = vec2i(i32(pixel_xy.x), i32(pixel_xy.y));

    // 视图 6：整屏只画顶部条 + 暗色 cell 归属底
    if (push.view == 6u) {
        let bar = status_bar(pixel_xy);
        if (pixel_xy.y < f32(push.height) * 0.05) {
            return vec4f(display_encode(bar), 1.0);
        }
        let hit = resolve_surface(pixel, in.uv);
        if (!hit.valid) {
            return vec4f(display_encode(VIEW_BACKGROUND), 1.0);
        }
        return vec4f(display_encode(hash_color(hit.cell / BUCKETS_PER_CELL) * 0.25), 1.0);
    }

    let hit = resolve_surface(pixel, in.uv);
    if (!hit.valid) {
        let color = select(VIEW_BACKGROUND, VIEW_OUTSIDE_VOLUME, hit.outside_volume);
        return vec4f(display_encode(color), 1.0);
    }
    let patch_index = cell_patch[hit.cell];
    var age = 0u;
    var deviation = 0.0;
    if (patch_index != INVALID_PATCH) {
        age = heartbeat_age(gi.frame.x, patch_statistics[patch_index].heartbeat);
        // 位置偏离 = patch 位置到当前表面切平面的垂直距离 / 体素（沿法线投影；同平面
        // 上的切向偏移不算——一个 cell 本来覆盖整个体素，当前像素与存的位置切向可差
        // 一整格，只有离面才是"贴不贴表面"）。
        let offset = patch_geometry[patch_index].position.xyz - hit.world;
        deviation = abs(dot(offset, hit.normal)) / hit.voxel;
    }
    let bucket = cell_bucket(hit.cell);
    let color = view_color(
        push.view, hit.cell, patch_index, hit.cascade, bucket, age, gi.frame.y, deviation,
    );
    return vec4f(display_encode(color), 1.0);
}

// —— 形态 B：池槽方片 ——

struct MarkerOut {
    @builtin(position) clip: vec4f,
    @location(0) color: vec3f,
}

@vertex
fn vs_marker(
    @builtin(vertex_index) vertex_index: u32,
    @builtin(instance_index) instance_index: u32,
) -> MarkerOut {
    var out: MarkerOut;
    out.clip = vec4f(2.0, 2.0, 2.0, 1.0); // 退化：视口外
    out.color = vec3f(0.0);

    let capacity = u32(gi.volume.w);
    if (instance_index >= capacity) {
        return out;
    }
    let cell = patch_cell[instance_index];
    if (cell == INVALID_PATCH || cell_patch[cell] != instance_index) {
        return out;
    }
    let res = u32(gi.volume.y);
    let cascade = cell_cascade(cell, res);
    let voxel = voxel_size(gi.volume.x, cascade);
    var position = patch_geometry[instance_index].position.xyz;
    var normal = patch_geometry[instance_index].normal.xyz;
    // 朝相机一侧偏移半个体素：方片与表面同深会整片 z-fighting
    if (dot(normal, frame.camera_pos.xyz - position) < 0.0) {
        normal = -normal;
    }
    position = position + normal * (voxel * 0.5);

    // 方片基（以法线为面朝向；辅助轴避开与法线平行）
    let helper = select(vec3f(1.0, 0.0, 0.0), vec3f(0.0, 1.0, 0.0), abs(normal.x) > 0.99);
    let right = normalize(cross(normal, helper));
    let up = cross(normal, right);
    let size = voxel * 0.12;
    // 两个三角形：0,1,2 + 2,1,3 的展开形式（六顶点实例化）
    let corners = array<vec2f, 6>(
        vec2f(-1.0, -1.0),
        vec2f(1.0, -1.0),
        vec2f(-1.0, 1.0),
        vec2f(-1.0, 1.0),
        vec2f(1.0, -1.0),
        vec2f(1.0, 1.0),
    );
    let corner2 = corners[vertex_index];
    let corner = position + right * (corner2.x * size) + up * (corner2.y * size);
    out.clip = frame.view_proj * vec4f(corner, 1.0);

    let bucket = cell_bucket(cell);
    let age = heartbeat_age(gi.frame.x, patch_statistics[instance_index].heartbeat);
    // 方片压在覆盖图上，若与 cell 同色就看不见了：混 65% 暖白，位置/朝向是这一形态的
    // 观察对象，子视图颜色只做淡背景。
    let tint = view_color(push.view, cell, instance_index, cascade, bucket, age, gi.frame.y, 0.0);
    out.color = mix(tint, vec3f(1.0, 0.94, 0.82), 0.8);
    return out;
}

@fragment
fn fs_marker(in: MarkerOut) -> @location(0) vec4f {
    let pixel = vec2i(i32(in.clip.x), i32(in.clip.y));
    let scene_depth = textureLoad(depth_tex, pixel, 0).r;
    if (scene_depth <= 0.0) {
        discard;
    }
    // reverse-Z：窗口深度更大 = 更近。方片在表面朝相机一侧，理应略大；比场景小 = 被遮挡。
    if (in.clip.z < scene_depth) {
        discard;
    }
    return vec4f(display_encode(in.color), 1.0);
}
