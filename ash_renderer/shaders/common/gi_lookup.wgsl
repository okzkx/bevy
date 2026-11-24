// 9.7 只读查表基座：世界位置 + 法线 → cell 的定位链，以及"查到的 patch 如何求值"。
// 只放纯函数，不声明任何 @group/@binding（与 gi_cell.wgsl 头注同一纪律）——所以这里
// 到 cell 索引为止，cell→patch 与 patch→rank 那两步由各入口自己读表后回传。
//
// 为什么单独抽一层：9.6 的 cache_evaluate 已把整链写完但带 bounce 分配副作用，9.7 的
// 屏幕查表要**只读**版；复制一份必然漂移（桶归属或取整口径一改就两套真相），所以抽成
// 这里一份，estimation 调它再叠自己的副作用（行为逐位不变）。
//
// 采样规则与屏幕侧四级滤波的 Unity 依据见笔记《Unity SCGI 实现》与
// 《SCGI 间接光注入端》；9.7 施工计划 §0.0/§9.7.1 是本文件的口径来源。
// #import gi_cell

// 定位结果。`cascade == INVALID_PATCH` 表示位置在 Volume 外（屏幕侧据此写 a = 0 回 IBL）。
struct LookupKey {
    // 级联号；INVALID_PATCH = Volume 外。
    cascade: u32,
    // cell 表线性下标（级联 × 存储槽 × 方向桶三者折叠）。
    cell: u32,
    // 命中的方向桶号（0..BUCKETS_PER_CELL），调试视图与读数用。
    bucket: u32,
}

// 体积中心 / 最细体素边长 / res / 级联数 / 各级体素偏移都由调用方从 GI UBO 显式传入：
// 公共片段不碰 uniform，避免"这段算术隐式要求 GI UBO 在某个 bind 上"。
//
// 算术与 9.5 播种、9.6 cache_evaluate 完全同源：球判据选级联 → 该级体素边长 → 体积坐标
// → 存储槽 → 方向桶 → cell 索引。`center` 必须来自 GI UBO 的 `target_pos`（不是帧 UBO
// 的 `camera_pos`：今天同值，但那是巧合，不是契约）。
fn lookup_cell_key(
    p: vec3f,
    n: vec3f,
    center: vec3f,
    voxel_min: f32,
    res: u32,
    cascades: u32,
    offsets: array<vec4i, MAX_CASCADES>,
) -> LookupKey {
    var key: LookupKey;
    let cascade = resolve_cascade(center, p, voxel_min, res, cascades);
    key.cascade = cascade;
    if (cascade == INVALID_PATCH) {
        key.cell = INVALID_PATCH;
        key.bucket = INVALID_PATCH;
        return key;
    }
    let voxel = voxel_size(voxel_min, cascade);
    let offset = offsets[cascade].xyz;
    let pv = volume_position(p, voxel, offset, i32(res));
    let storage = volume_to_storage(pv, offset, i32(res));
    let bucket = direction_to_bucket_index(n, BUCKET_RESOLUTION);
    key.bucket = bucket;
    key.cell = cell_index(cascade, storage, bucket, res);
    return key;
}

// 查到的 patch 辐照度按法线求值：E(n) = l0 + l1·n。
// 基常数与余弦卷积都已折在存储值里，这里**不能**再乘 1/π 或再做一次余弦卷积——那是消费端
// `albedo · Fd_Lambert()` 的事（9.3 与 9.6 同一口径）。
fn lookup_eval(irr: PatchIrradiance, n: vec3f) -> vec3f {
    return irr.l0.xyz
        + irr.l1x.xyz * n.x
        + irr.l1y.xyz * n.y
        + irr.l1z.xyz * n.z;
}

// ---- 1/4 屏幕网格映射（gather 与播种必须同一份口径）----
// 播种每帧在 4×4 块里只走一个子像素：sub = (frame·7) mod 16，列 = sub%4、行 = sub/4。
// 16 帧走遍整块；7 与 16 互质保证是双射（不是来回两点）。CPU 镜像见
// `src/render/passes/gi/volume.rs` 的 `seed_subsample`（单测钉住两边同式）。
const QUARTER_BLOCK: u32 = 4u;
const QUARTER_SUBSAMPLES: u32 = QUARTER_BLOCK * QUARTER_BLOCK;

// 本帧被播种的子像素在块内的（列, 行）。
fn seed_subsample(frame_index: u32) -> vec2u {
    let sub = (frame_index * 7u) % QUARTER_SUBSAMPLES;
    return vec2u(sub % QUARTER_BLOCK, sub / QUARTER_BLOCK);
}

// 1/4 像素 + 块内偏移 → 全分辨率像素（等价于播种的 `full = low*4 + sub%4 / sub/4`）。
fn low_to_full(low: vec2u, sub: vec2u) -> vec2u {
    return low * QUARTER_BLOCK + sub;
}

// gather 的 3 个块内样本点：本帧播种的子像素、它的对角、块心。
//
// 为什么不按块心单点取：单点查到的 patch 与本帧播种落在的那格不是同一格，命中率会被
// 系统性低估（9.7 施工计划 §0.0 最后一行）。三点里只有"本帧播种点"与播种同格，另两点
// 是把该块内的样本摊开，降低单点噪声。
// 块心取 (1, 1)：4×4 的四个中心像素 (1,1)/(1,2)/(2,1)/(2,2) 等价，取左上那个与
// `low_to_full` 的整数算术一致。样本数后续可调（9.7.4 的 `lookup_samples`）。
const GATHER_SAMPLE_COUNT: u32 = 3u;

fn gather_sample_offsets(frame_index: u32) -> array<vec2u, GATHER_SAMPLE_COUNT> {
    let sub = seed_subsample(frame_index);
    let opposite = vec2u(
        QUARTER_BLOCK - 1u - sub.x,
        QUARTER_BLOCK - 1u - sub.y,
    );
    let center = vec2u(1u, 1u);
    return array<vec2u, GATHER_SAMPLE_COUNT>(sub, opposite, center);
}
