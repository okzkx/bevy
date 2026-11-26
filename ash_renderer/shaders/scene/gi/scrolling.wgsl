// 9.5 滚动释放（compute）：逐 Volume 体素检查 slot 身份，失效就把这个体素下 16 个桶的
// cell 三连释放（cell→patch / patch→cell / 心跳统计都归零由淘汰侧处理；这里只摘指针）。
//
// 为什么不需要搬运数据：volume_to_storage 里级联偏移被代数消掉，世界体素 → 存储槽是
// 世界对齐的环形瓦片（见 gi_cell.wgsl）。相机移动时数据原地不动，只有"已经出界"的槽
// 需要回收。
//
// 判据（9.10 对照修正）：原实现把存储槽先回绕成世界位置（storage_to_volume）再套球判据，
// 而 storage_to_volume 是 mod res 的回绕——已经滑出 Volume 的旧槽被模回对侧边缘，反推出的
// 世界位置落在球内，于是旧 patch 既不被释放、又会被对侧 cell 的查表读到（辐照度鬼影/穿墙
// 漏光），同时白占池槽直到心跳老化。Unity 的等价判据是 Scrolling.compute:32-40：用
// unwrapped 位置（oldPos = mod(storage − offset, res)、offsettedOld = oldPos − (new − old)）
// 判断是否越出 [0, res)，越出才释放。
//
// 这里改成更强的自校验：直接用 patch 自己的几何记录反算 (级联球, 存储槽, 方向桶)，
// 与它注册的 cell 不一致就释放。对环形回绕、级联降档、defrag 搬移都成立，
// 且不需要在 UBO 里多存一份上一帧 offset。
//
// 接口：set0 b0 cell→patch / b2 patch 几何 / b6 patch→cell / b7 ring / b8 计数 / b9 GI UBO。
// dispatch 线程 = 级联数 × res³（每线程循环 16 桶）。
// #import gi_cell

@group(0) @binding(0) var<storage, read_write> cell_patch: array<u32>;
@group(0) @binding(2) var<storage, read> patch_geometry: array<PatchGeometry>;
@group(0) @binding(6) var<storage, read_write> patch_cell: array<u32>;
@group(0) @binding(7) var<storage, read_write> ring: array<atomic<u32>, 12>;
@group(0) @binding(8) var<storage, read_write> counters: array<atomic<u32>, 16>;
@group(0) @binding(9) var<uniform> gi: GiUniforms;

// 槽身份自校验：patch 记录的位置/法线必须仍然对得上它注册的 (级联, 存储槽, 方向桶)。
// - 位置滑出本级球判据（相机移开、或该点已进入更细的级）→ 释放；
// - 位置重建出来的存储槽与本槽不同（环形瓦片在相机移动后换了归属）→ 释放；
// - 法线方向桶不同（法线来源变化或槽被复用）→ 释放。
fn patch_identity_holds(
    geometry: PatchGeometry,
    cascade: u32,
    storage: vec3i,
    bucket: u32,
    offset: vec3i,
    voxel: f32,
    res: u32,
) -> bool {
    let position = geometry.position.xyz;
    if (!is_inside_cascade(gi.target_pos.xyz, position, voxel, res)) {
        return false;
    }
    let volume_pos = volume_position(position, voxel, offset, i32(res));
    if (any(volume_to_storage(volume_pos, offset, i32(res)) != storage)) {
        return false;
    }
    return direction_to_bucket_index(geometry.normal.xyz, BUCKET_RESOLUTION) == bucket;
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let res = u32(gi.volume.y);
    let cascades = u32(gi.volume.z);
    let per_cascade = res * res * res;
    let index = gid.x;
    if (index >= per_cascade * cascades) {
        return;
    }
    let cascade = index / per_cascade;
    let plane = index % per_cascade;
    let storage = vec3i(
        i32(plane / (res * res)),
        i32((plane / res) % res),
        i32(plane % res),
    );
    let offset = gi.cascade_offsets[cascade].xyz;
    let voxel = voxel_size(gi.volume.x, cascade);
    for (var bucket = 0u; bucket < BUCKETS_PER_CELL; bucket = bucket + 1u) {
        let cell = cell_index(cascade, storage, bucket, res);
        let patch_index = cell_patch[cell];
        if (patch_index == INVALID_PATCH) {
            continue;
        }
        // 槽已被整理搬走（cell 仍指旧槽）：只清 cell 侧，别误清新位置的活 patch_index。
        if (patch_cell[patch_index] != cell) {
            cell_patch[cell] = INVALID_PATCH;
            continue;
        }
        if (patch_identity_holds(patch_geometry[patch_index], cascade, storage, bucket, offset, voxel, res)) {
            continue;
        }
        cell_patch[cell] = INVALID_PATCH;
        patch_cell[patch_index] = INVALID_PATCH;
        atomicSub(&ring[1], 1u);
        atomicAdd(&counters[2], 1u);
    }
}
