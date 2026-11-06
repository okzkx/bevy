// Hi-Z 深度金字塔归约（compute 单入口）：把源图逐 2×2 取 MIN 缩半写进目标级。每次
// dispatch 只做一级，由 Rust 侧 HizPyramid::record_reduction 按 mip 链串行录制。
// 目标 level 1 的源 = 深度附件，L≥2 的源 = 金字塔 L-1 级（都是单级采样视图）；level 0
// 不派发。
//
// 方向：本仓深度 reverse-Z（近 1、远 0，clear 0），值越小 = 越远。tile 的保守遮挡上界 =
// tile 内最远的已缓冲表面 = 值最小者 → 归约取 MIN。坑：正向 Z 的同类写法是 MAX，取错 =
// 系统性错杀。CPU 参照 = hiz.rs reduce_tile_reverse_z；遮挡比较与 mip 选层在
// occlusion.wgsl。
//
// 接口：独立小布局（不走 set2，金字塔随 swapchain 重建会换 image/视图）：b0 = 源采样
// 视图（textureLoad 不需要采样器），b1 = 目标 storage 视图（r32float，write）。
//
// 每 invocation 写互不重叠的 dst 像素。

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
    // 坑：mip 尺寸向下取整时，奇数源图会留下尾行/尾列；并入最后一个目标 texel，
    // 保证所有源深度都参与 MIN 归约。
    let end_x = select(min(start.x + 2u, src_dims.x), src_dims.x, gid.x + 1u == dims.x);
    let end_y = select(min(start.y + 2u, src_dims.y), src_dims.y, gid.y + 1u == dims.y);
    var m = 1.0;
    for (var y = start.y; y < end_y; y += 1u) {
        for (var x = start.x; x < end_x; x += 1u) {
            m = min(m, textureLoad(src, vec2<u32>(x, y), 0).x);
        }
    }
    textureStore(dst, gid.xy, vec4<f32>(m, 0.0, 0.0, 0.0));
}
