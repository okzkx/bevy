// LOD 引脚选择。导入方须声明全局 `lod_table: array<LodEntry>`（set2 b7）。
// #import instance

// 从档位 level 向细档方向找最近常驻条目（index_count != 0）；全哨兵（条目未
// 写过/档位全缺）回退对象记录基准引脚——可绘行的基准 mesh 恒驻留，保证不访问
// 未驻留 LOD。
fn pick_lod_entry(rec: ObjectRecord, slot: u32, level: u32) -> LodEntry {
    var chosen = LodEntry(0u, 0u, 0u);
    var l = level;
    loop {
        let e = lod_table[slot * MAX_LOD_LEVELS + l];
        if (e.index_count != 0u) {
            chosen = e;
            break;
        }
        if (l == 0u) {
            break;
        }
        l = l - 1u;
    }
    if (chosen.index_count == 0u) {
        chosen = LodEntry(rec.first_index, rec.index_count, rec.vertex_base);
    }
    return chosen;
}
