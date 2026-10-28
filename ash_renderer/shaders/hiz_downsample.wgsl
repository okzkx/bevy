// Hi-Z 深度金字塔归约（compute 单入口）：把源图逐 2×2 取 MIN 缩半写进
// 金字塔下一级。每次 dispatch 只做一级——源 = 深度附件（level 0）或金字塔上
// 一级（level L≥1，各带单级采样视图），目标 = 本级 storage view；逐级 dispatch
// 由 Rust 侧按 mip 链长度串行录制（bevy experimental/downsample_depth.wesl 的
// 单 pass SPD 形状不搬——批量重排换挡的复杂度对本段规模不划算，逐级串行
// 是"学机制与保守化形状，不搬整套架构"的取舍钉明）。
//
// reverse-Z 保守方向（src/render/gpu/hiz.rs 单测钉死）：本仓深度 reverse-Z
//（近平面 1、远平面 0，clear 0），深度值越小 = 越远。tile 的保守遮挡上界 =
// tile 内"最远"的已缓冲表面 = 值最小者 → 归约取 MIN。正向 Z 的同类写法是
// MAX，方向相反——取错 = 系统性错杀（最危险单点）。比较语义（遮挡判剔除）
// 与 mip 层选择在 occlusion.wgsl；CPU 参照 = hiz.rs reduce_tile_reverse_z。
//
// 接口：独立小布局（不走 set2——金字塔随 swapchain 重建会换 image/视图，
// set2"描述符 init 一次写入永不更新"的冻结纪律不为它开口，churn 隔离在本模块
// 对应的 Rust 侧 HizPyramid 内）：b0 = 源采样视图（texture_2d<f32>，textureLoad
// 不需要采样器），b1 = 目标 storage 视图（r32float，write）。
//
// 并发语义：每 invocation 写互不重叠的 dst 像素；奇数尺寸尾行/尾列并入
// 最后一格归约范围，所有源像素都有归属。

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<r32float, write>;

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = vec2<u32>(textureDimensions(dst));
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let src_dims = vec2<u32>(textureDimensions(src));
    let start = gid.xy * 2u;
    // mip 尺寸向下取整时，奇数源图会留下尾行/尾列；并入最后一个目标
    // texel，保证所有源深度都参与 MIN 归约。
    let end_x = select(min(start.x + 2u, src_dims.x), src_dims.x, gid.x + 1u == dims.x);
    let end_y = select(min(start.y + 2u, src_dims.y), src_dims.y, gid.y + 1u == dims.y);
    var m = 1.0;
    for (var y = start.y; y < end_y; y += 1u) {
        for (var x = start.x; x < end_x; x += 1u) {
            m = min(m, textureLoad(src, vec2<u32>(x, y), 0).x);
        }
    }
    // MIN = reverse-Z 的保守遮挡上界（见模块头注；hiz.rs 单测同源钉方向）
    textureStore(dst, gid.xy, vec4<f32>(m, 0.0, 0.0, 0.0));
}
