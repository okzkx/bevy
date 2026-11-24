// 9.6.3 bounce 分配请求的记录定义（跨端契约）：生产者是 `scene/gi/estimation.wgsl`
// 的多跳自查询（cell 里没有 patch 时追加一条），消费者是同帧 ④.5 段末尾的
// `scene/gi/bounce_alloc.wgsl`。Rust 镜像见 `resident/tables/surface_cache.rs` 的
// GI_BOUNCE_REQUEST_SIZE（48B）。只放结构定义与常量，不声明任何 @group/@binding。

// 一条 bounce 分配请求（48B）：cell + 命中点位置 + 命中点法线。
struct BounceRequest {
    // x = cell 线性下标；yzw 保留。
    cell: vec4u,
    // xyz = 命中点世界位置（作为新 patch 的位置）。
    position: vec4f,
    // xyz = 命中点法线（作为新 patch 的法线）。
    normal: vec4f,
}

// 每帧请求上限（与 Rust 的 GI_BOUNCE_REQUEST_CAPACITY 同值）。
const GI_BOUNCE_REQUEST_CAPACITY: u32 = 4096u;
// 计数缓冲里的槽位（与 Rust 的 COUNTER_* 常量同值）。
const GI_COUNTER_BOUNCE_REQUESTS: u32 = 11u;
const GI_COUNTER_BOUNCE_HITS: u32 = 8u;
const GI_COUNTER_BOUNCE_MISSES: u32 = 15u;
