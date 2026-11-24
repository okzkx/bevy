// 9.5 心跳淘汰（compute）：容量线程（不是活跃数——线程里先判槽是否在用）。心跳距离按
// 模 65536 的最短距算（直接相减会在约 18 分钟处把全场同时淘汰）。
//
// 首版只做"无 bounce 直接释放"分支：bounce 的"先降 rank 1 再淘汰"留给 9.6（GI UBO 的
// frame.z 位已留）。陈旧槽指针（整理搬走后残留的 patch→cell）只清自己，不碰 cell 侧
// （cell 已指向新槽）。
//
// 接口：set0 b0 cell→patch / b5 统计（心跳） / b6 patch→cell / b7 ring / b8 计数 / b9 UBO。
// dispatch 线程 = 池容量。
// #import gi_cell

@group(0) @binding(0) var<storage, read_write> cell_patch: array<u32>;
@group(0) @binding(5) var<storage, read_write> patch_statistics: array<PatchStatistics>;
@group(0) @binding(6) var<storage, read_write> patch_cell: array<u32>;
@group(0) @binding(7) var<storage, read_write> ring: array<atomic<u32>, 12>;
@group(0) @binding(8) var<storage, read_write> counters: array<atomic<u32>, 16>;
@group(0) @binding(9) var<uniform> gi: GiUniforms;

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let slot = gid.x;
    if (slot >= u32(gi.volume.w)) {
        return;
    }
    let cell = patch_cell[slot];
    if (cell == INVALID_PATCH) {
        return;
    }
    // 陈旧指针（整理把它搬走了）：只清自己，别误清 cell 现在指向的活 patch。
    if (cell_patch[cell] != slot) {
        patch_cell[slot] = INVALID_PATCH;
        return;
    }
    // bounce 分支（9.6）：frame.z != 0 且 rank == 0 时先降 rank 1，下一轮再淘汰。
    if (gi.frame.z != 0u) {
        let rank = patch_statistics[slot].state & 0xFFu;
        if (rank == 0u) {
            patch_statistics[slot].state = (patch_statistics[slot].state & ~0xFFu) | 1u;
            patch_statistics[slot].heartbeat = gi.frame.x & 0xFFFFu;
            return;
        }
    }
    let age = heartbeat_age(gi.frame.x, patch_statistics[slot].heartbeat);
    if (age <= gi.frame.y) {
        return;
    }
    patch_cell[slot] = INVALID_PATCH;
    cell_patch[cell] = INVALID_PATCH;
    atomicSub(&ring[1], 1u);
    atomicAdd(&counters[2], 1u);
}
