// ring 追加（分配入口专用，9.6.3 从 gi_cell 拆出）：返回池内槽号（INVALID_PATCH = 池满）。
// count 只增不减，洞由整理回收。
//
// 引用全局 `ring: array<atomic<u32>, 12>`（由导入方声明，名字即契约）；`INVALID_PATCH`
// 来自 common/gi_cell.wgsl，所以本片段要在它之后 import。屏幕播种（patch_alloc）与
// bounce 分配（bounce_alloc）共用这一份，避免两条分配路径写出不一致的 ring 水位。

fn ring_append(capacity: u32) -> u32 {
    let flip = atomicLoad(&ring[0]);
    let count_index = 4u + flip * 4u + 1u;
    let start_index = 4u + flip * 4u;
    var slot = INVALID_PATCH;
    loop {
        let count = atomicLoad(&ring[count_index]);
        if (count >= capacity) {
            break;
        }
        let result = atomicCompareExchangeWeak(&ring[count_index], count, count + 1u);
        if (result.exchanged) {
            let start = atomicLoad(&ring[start_index]);
            slot = (start + count) % capacity;
            break;
        }
    }
    return slot;
}

