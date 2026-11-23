// 环境 cubemap 捕获（compute 单入口）：把 9.1 物理天空逐面求值写进 cubemap mip0。
//
// 只排除**太阳圆盘**（`include_sun = false`）；太阳附近的米氏前向峰（散射亮区）仍
// 留在捕获里——HDRP 的 cubemap 烘焙同口径。金属上出现"第二个圆盘"才说明太阳没排干净，
// 宽亮区是允许的（见段施工计划 §0.2）。
//
// 接口：set0 b0 = mip0 的 2D-array storage 视图（6 层），b1 = 帧 UBO（天空参数与
// 相机海拔）。dispatch = (面宽/8, 面宽/8, 6)：z = 面号，一层一面；面内像素中心 →
// 方向用 common/ibl.wgsl 的 cube_face_direction（与硬件 cube 采样同约定）。
// #import frame
// #import sky
// #import ibl

@group(0) @binding(0) var dst: texture_storage_2d_array<rgba16float, write>;
@group(0) @binding(1) var<uniform> frame: FrameUniforms;

@compute @workgroup_size(8, 8, 1)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = vec2<u32>(textureDimensions(dst));
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let uv = (vec2f(gid.xy) + vec2f(0.5)) / vec2f(dims);
    let dir = cube_face_direction(gid.z, uv);
    // alpha 存 1：cubemap 的 alpha 不参与光照，但保持与过滤器一致的有限值
    textureStore(dst, vec2<i32>(gid.xy), i32(gid.z), vec4f(sky_radiance(dir, false), 1.0));
}
