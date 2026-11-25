// 9.5 整理（compute）：把 ring 内容区间里的空洞项前移压缩。每帧若干波，每波 512 条
// （256 线程 × 2 条），存活条目用原子取号领取搬移目标；跨帧续扫游标存在 ring 配置的
// scan 里，扫到区间末尾就把"读/写两份配置"翻转（flip-flop）。
//
// 为什么不用 workgroup 前缀和：naga 30 给 <workgroup> 聚合类型发 Offset/ArrayStride
// 装饰，被 spirv-val 按 VUID-StandaloneSpirv-None-10684（Workgroup 不得有显式布局）
// 拒；项目构建门的补丁只处理描述符索引，不为它开路。原子取号得到"顺序随调度、集合
// 确定"的压缩：目标偏移恒 ≤ 源偏移（相等 = 无洞），ring 顺序无外部语义。
//
// 正确性要点：
// - 本波写范围 ⊆ [本波读起点, 本波读起点 + 512)，可能覆盖本波自己的源，所以先把两条
//   源记录读进私有存储（局部变量），workgroupBarrier + storageBarrier 之后才落笔。
// - 跨波安全：本波写偏移 < 下一波读起点（存活数 ≤ 处理条数），不会吞掉下一波的源。
// - 源槽的 patch→cell 指针不主动清：淘汰/存活判定都带 cell 反查，陈旧指针只会被当成
//   死项忽略，绝不误伤整理后的活 patch。
//
// 接口：set0 b0 cell→patch / b2..b5 四张 patch 表 / b6 patch→cell / b7 ring /
// b8 计数 / b9 GI UBO / b14+b15 时域滤波历史（跟着 patch 搬）。dispatch 组数 = UBO ring.x
//（每帧波数），每组一个 workgroup。
// #import gi_cell

const DEFRAG_WAVE: u32 = 512u;
const DEFRAG_THREADS: u32 = 256u;

@group(0) @binding(0) var<storage, read_write> cell_patch: array<u32>;
@group(0) @binding(2) var<storage, read_write> patch_geometry: array<PatchGeometry>;
@group(0) @binding(3) var<storage, read_write> patch_irradiance0: array<PatchIrradiance>;
@group(0) @binding(4) var<storage, read_write> patch_irradiance1: array<PatchIrradiance>;
@group(0) @binding(5) var<storage, read_write> patch_statistics: array<PatchStatistics>;
@group(0) @binding(6) var<storage, read_write> patch_cell: array<u32>;
@group(0) @binding(7) var<storage, read_write> ring: array<atomic<u32>, 12>;
@group(0) @binding(8) var<storage, read_write> counters: array<atomic<u32>, 16>;
@group(0) @binding(9) var<uniform> gi: GiUniforms;
// 9.7.3 时域滤波的两张表**按槽位**存历史：搬 patch 时必须一起搬，否则新槽位上留着
// 另一条 patch 的历史，指数滑动平均每帧都混进别人的值（实测会把屏幕侧能量压到 0.42 倍，
// 并且逐帧抖动——见 9.7.3 施工记录 §补漏）。
@group(0) @binding(14) var<storage, read_write> filtered_irradiance0: array<PatchIrradiance>;
@group(0) @binding(15) var<storage, read_write> filtered_irradiance1: array<PatchIrradiance>;

@compute @workgroup_size(256)
fn cs_main(@builtin(local_invocation_index) lid: u32) {
    let capacity = u32(gi.volume.w);
    let flip = atomicLoad(&ring[0]);
    let read_config = 4u + flip * 4u;
    let write_config = 4u + (1u - flip) * 4u;
    let start = atomicLoad(&ring[read_config]);
    let count = atomicLoad(&ring[read_config + 1u]);
    let scan = atomicLoad(&ring[read_config + 2u]);

    // —— 读相位：两条源记录读进私有存储（此时没有任何线程写）——
    let offset0 = scan + lid * 2u;
    let offset1 = offset0 + 1u;
    var cell0 = INVALID_PATCH;
    var cell1 = INVALID_PATCH;
    var live0 = 0u;
    var live1 = 0u;
    var geometry0: PatchGeometry;
    var geometry1: PatchGeometry;
    var irradiance0_a: PatchIrradiance;
    var irradiance1_a: PatchIrradiance;
    var irradiance0_b: PatchIrradiance;
    var irradiance1_b: PatchIrradiance;
    var statistics0: PatchStatistics;
    var statistics1: PatchStatistics;
    // 时域滤波的两条历史（b14/b15）跟着 patch 走
    var filtered0_a: PatchIrradiance;
    var filtered1_a: PatchIrradiance;
    var filtered0_b: PatchIrradiance;
    var filtered1_b: PatchIrradiance;
    // 源槽号：写相位用它判断"原地不动"（dst == src 时不必回写）
    var slot0 = INVALID_PATCH;
    var slot1 = INVALID_PATCH;

    if (offset0 < count) {
        let slot = (start + offset0) % capacity;
        let cell = patch_cell[slot];
        // 存活 = 双表互指成立（陈旧槽的 cell 已指向别处，按死项丢弃）
        if (cell != INVALID_PATCH && cell_patch[cell] == slot) {
            live0 = 1u;
            cell0 = cell;
            slot0 = slot;
            geometry0 = patch_geometry[slot];
            irradiance0_a = patch_irradiance0[slot];
            irradiance1_a = patch_irradiance1[slot];
            statistics0 = patch_statistics[slot];
            filtered0_a = filtered_irradiance0[slot];
            filtered1_a = filtered_irradiance1[slot];
        }
    }
    if (offset1 < count) {
        let slot = (start + offset1) % capacity;
        let cell = patch_cell[slot];
        if (cell != INVALID_PATCH && cell_patch[cell] == slot) {
            live1 = 1u;
            cell1 = cell;
            slot1 = slot;
            geometry1 = patch_geometry[slot];
            irradiance0_b = patch_irradiance0[slot];
            irradiance1_b = patch_irradiance1[slot];
            statistics1 = patch_statistics[slot];
            filtered0_b = filtered_irradiance0[slot];
            filtered1_b = filtered_irradiance1[slot];
        }
    }

    // 执行序 + 存储序：所有源读在任一落笔之前完成
    workgroupBarrier();
    storageBarrier();

    // —— 写相位：原子取号领目标偏移（无序压缩，不动点）——
    if (live0 == 1u) {
        let dst_offset = atomicAdd(&ring[write_config + 1u], 1u);
        let dst = (start + dst_offset) % capacity;
        patch_geometry[dst] = geometry0;
        patch_irradiance0[dst] = irradiance0_a;
        patch_irradiance1[dst] = irradiance1_a;
        patch_statistics[dst] = statistics0;
        patch_cell[dst] = cell0;
        // 时域历史跟着走（原地不动时 dst == slot0，写回同值也无副作用，但省掉这两笔拷贝）
        if (dst != slot0) {
            filtered_irradiance0[dst] = filtered0_a;
            filtered_irradiance1[dst] = filtered1_a;
        }
        cell_patch[cell0] = dst;
        atomicAdd(&counters[3], 1u);
    }
    if (live1 == 1u) {
        let dst_offset = atomicAdd(&ring[write_config + 1u], 1u);
        let dst = (start + dst_offset) % capacity;
        patch_geometry[dst] = geometry1;
        patch_irradiance0[dst] = irradiance0_b;
        patch_irradiance1[dst] = irradiance1_b;
        patch_statistics[dst] = statistics1;
        patch_cell[dst] = cell1;
        if (dst != slot1) {
            filtered_irradiance0[dst] = filtered0_b;
            filtered_irradiance1[dst] = filtered1_b;
        }
        cell_patch[cell1] = dst;
        atomicAdd(&counters[3], 1u);
    }

    workgroupBarrier();
    storageBarrier();

    // —— 游标推进；扫到区间末尾翻转读/写配置 ——
    if (lid == 0u) {
        let new_scan = min(scan + DEFRAG_WAVE, count);
        atomicStore(&ring[read_config + 2u], new_scan);
        if (new_scan >= count) {
            // 写配置转正（count 已由本周期各波累加）；旧读配置清空成下一周期的写配置
            atomicStore(&ring[write_config], start);
            atomicStore(&ring[write_config + 2u], 0u);
            atomicStore(&ring[read_config + 1u], 0u);
            atomicStore(&ring[read_config + 2u], 0u);
            atomicStore(&ring[0], 1u - flip);
        }
    }
}
