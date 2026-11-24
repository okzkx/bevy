// 9.5 滚动释放（compute）：级联偏移变化的帧才录——逐 Volume 体素把存储槽换回世界位置，
// 落在该级球判据之外就把这个体素下 16 个桶的 cell 三连释放（cell→patch / patch→cell /
// 心跳统计都归零由淘汰侧处理；这里只摘指针）。
//
// 为什么不需要搬运数据：volume_to_storage 里级联偏移被代数消掉，世界体素 → 存储槽是
// 世界对齐的环形瓦片（见 gi_cell.wgsl）。相机移动时数据原地不动，只有"已经出界"的槽
// 需要回收。
//
// 接口：set0 b0 cell→patch / b6 patch→cell / b7 ring / b8 计数 / b9 GI UBO。
// dispatch 线程 = 级联数 × res³（每线程循环 16 桶）。
// #import gi_cell

@group(0) @binding(0) var<storage, read_write> cell_patch: array<u32>;
@group(0) @binding(6) var<storage, read_write> patch_cell: array<u32>;
@group(0) @binding(7) var<storage, read_write> ring: array<atomic<u32>, 12>;
@group(0) @binding(8) var<storage, read_write> counters: array<atomic<u32>, 16>;
@group(0) @binding(9) var<uniform> gi: GiUniforms;

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
    let volume_pos = storage_to_volume(storage, offset, i32(res));
    let world = volume_world_center(volume_pos, offset, voxel, i32(res));
    if (is_inside_cascade(gi.target_pos.xyz, world, voxel, res)) {
        return;
    }
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
        cell_patch[cell] = INVALID_PATCH;
        patch_cell[patch_index] = INVALID_PATCH;
        atomicSub(&ring[1], 1u);
        atomicAdd(&counters[2], 1u);
    }
}
