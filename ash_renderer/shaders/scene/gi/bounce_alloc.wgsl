// 9.6.3 bounce 分配（compute）：消费同帧 estimation 写下的分配请求，为"命中点所在
// cell 还没有 patch"的地方补一条 patch（rank = 1）。
//
// 与屏幕播种的关系（照 Unity `PatchAllocationRequest` 的形状）：
// - 两者共用 `CellAllocationMarks` 的帧号纪元标签仲裁，同帧同 cell 只有一个入口拿到
//   分配权——不能另开一条绕过 marks 的分配路径（否则同 cell 双分配、池被冲）。
// - 播种先录（本帧的屏幕覆盖优先），本 pass 在其后：screen 已经覆盖的 cell 这里一律跳过。
// - 只有 rank 0 的 patch 才能提出请求（`estimation` 里判），所以这条路径不会自激。
// - 新 patch 的种子留零：rank = 1 不享受预热，下一帧起由估计正常收敛；心跳记在分配帧，
//   之后没被屏幕刷新就按普通淘汰窗回收。
//
// 接口：set0 = GI 集（b0 cell→patch / b1 marks / b2 几何 / b3+b4 辐照度 / b5 统计 /
// b6 patch→cell / b7 ring / b8 计数 / b9 GI UBO / b13 请求表），无 push。
// 线程 = 请求上限（`GI_BOUNCE_REQUEST_CAPACITY`），实际条数取 `counters[11]`。
// #import gi_cell
// #import gi_ring
// #import gi_bounce

@group(0) @binding(0) var<storage, read_write> cell_patch: array<u32>;
@group(0) @binding(1) var<storage, read_write> cell_marks: array<atomic<u32>>;
@group(0) @binding(2) var<storage, read_write> patch_geometry: array<PatchGeometry>;
@group(0) @binding(3) var<storage, read_write> patch_irradiance0: array<PatchIrradiance>;
@group(0) @binding(4) var<storage, read_write> patch_irradiance1: array<PatchIrradiance>;
@group(0) @binding(5) var<storage, read_write> patch_statistics: array<PatchStatistics>;
@group(0) @binding(6) var<storage, read_write> patch_cell: array<u32>;
@group(0) @binding(7) var<storage, read_write> ring: array<atomic<u32>, 12>;
@group(0) @binding(8) var<storage, read_write> counters: array<atomic<u32>, 16>;
@group(0) @binding(9) var<uniform> gi: GiUniforms;
@group(0) @binding(13) var<storage, read> bounce_requests: array<BounceRequest>;
@group(0) @binding(15) var<storage, read_write> filtered_irradiance1: array<PatchIrradiance>;

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let index = gid.x;
    if (index >= GI_BOUNCE_REQUEST_CAPACITY) {
        return;
    }
    let count = min(atomicLoad(&counters[GI_COUNTER_BOUNCE_REQUESTS]), GI_BOUNCE_REQUEST_CAPACITY);
    if (index >= count) {
        return;
    }
    let request = bounce_requests[index];
    let cell = request.cell.x;
    // 已有 patch（播种或前一条请求先到）就跳过
    if (cell_patch[cell] != INVALID_PATCH) {
        return;
    }
    let normal_length = length(request.normal.xyz);
    if (!(normal_length > 0.0)) {
        return;
    }
    // 同帧同 cell 仲裁：与播种共用同一个纪元标签（每帧唯一且非 0，见 patch_alloc.wgsl；
    // 用 `帧号 | 1` 会让相邻偶数帧与奇数帧撞标签，奇数帧的分配全被跳过）
    let epoch = gi.frame.x * 2u + 1u;
    let previous = atomicExchange(&cell_marks[cell], epoch);
    if (previous == epoch) {
        return;
    }
    let capacity = u32(gi.volume.w);
    let slot = ring_append(capacity);
    if (slot == INVALID_PATCH) {
        // 池满：归还标签允许后续帧重试，失败计数与播种共用同一个槽位
        atomicStore(&cell_marks[cell], 0u);
        atomicAdd(&counters[1], 1u);
        return;
    }
    var empty_irradiance: PatchIrradiance;
    empty_irradiance.l0 = vec4f(0.0);
    empty_irradiance.l1x = vec4f(0.0);
    empty_irradiance.l1y = vec4f(0.0);
    empty_irradiance.l1z = vec4f(0.0);
    patch_geometry[slot].position = request.position;
    patch_geometry[slot].normal = vec4f(request.normal.xyz / normal_length, 0.0);
    patch_irradiance0[slot] = empty_irradiance;
    patch_irradiance1[slot] = empty_irradiance;
    // 新身份清空 b15 和有效帧戳，避免回收槽的旧历史污染屏幕链。
    filtered_irradiance1[slot] = empty_irradiance;
    patch_statistics[slot].mean = vec4f(0.0);
    patch_statistics[slot].variance = vec4f(0.0);
    // rank = 1（state 低 8 位）：bounce 出来的 patch 不参与再分配、也不享受预热
    patch_statistics[slot].state = 1u;
    patch_statistics[slot].request = 0u;
    patch_statistics[slot].update_count = 0u;
    patch_statistics[slot].heartbeat = gi.frame.x & 0xFFFFu;
    patch_cell[slot] = cell;
    cell_patch[cell] = slot;
    atomicAdd(&counters[0], 1u);
    let active_count = atomicAdd(&ring[1], 1u) + 1u;
    atomicMax(&ring[2], active_count);
}
