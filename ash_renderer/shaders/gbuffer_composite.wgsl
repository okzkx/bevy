// 8.1 延迟合成（全屏）：读 G-buffer（albedo/normal/depth），按 frame.mode
// 三分支输出——与现行 forward 直出同语义（判定线①：G-buffer 语义与 forward
// 对齐的单光源 A/B 对照的 deferred 侧）。
// - 0 Lambert：albedo × (direct + ambient) × 曝光——公式与 debug_draw 的
//   forward Lambert 逐字同源（UBO 灯光数据、1/π、EXPOSURE 同值）。
// - 1 Unlit：albedo 附件直出（反照率验收）。
// - 2 Normal：normal 附件可视化（法线方向验收仪器，n×0.5+0.5）。
// - 3 Position（8.1.2）：G-buffer 深度反投影重建世界位置并可视化——世界位置
//   取法拍板 = 深度重建（省全屏位置附件），本模式是其验收仪器：clip 反推
//   world = inv_view_proj × clip(uv, depth)，世界坐标 ×0.0625+0.5 编码直出
//   （平面上应呈连续渐变、无碎片跳变；步骤 9 SCGI 播种/gather 的取数面）。
// 背景判定：reverse-Z 深度清 0 = 远平面（无几何写深度）→ 输出清屏色——
// forward 直出时清屏色由 loadOp 给出，deferred 改由本分支承接，语义等价。
//
// 接口：set0 = 帧 UBO（FrameUniforms，与主管线 set1 同一 layout 对象——集与
// 布局同源）；set1 = G-buffer 采样集（b0 albedo / b1 normal / b3 depth / b4 采样器，
// gbuffer.rs 布局契约）。

struct FrameUniforms {
    view_proj: mat4x4f,
    dir_to_light: vec4f,
    ambient_color: vec4f,
    light_color: vec4f,
    mode: u32,
    // 8.1.2（UBO 128→192B 第二次解冻）：clip→world 反投影矩阵
    inv_view_proj: mat4x4f,
}

const MODE_LAMBERT: u32 = 0u;
const MODE_UNLIT: u32 = 1u;
const MODE_NORMAL: u32 = 2u;
const MODE_POSITION: u32 = 3u;

const INV_PI: f32 = 0.3183099;
const EXPOSURE: f32 = 0.0010019;

// 清屏色（与 host 侧 CLEAR_COLOR 同值——线性域，SRGB 编码在最终 ROP 一次）
const CLEAR_COLOR: vec3f = vec3f(0.02, 0.06, 0.14);

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var albedo_tex: texture_2d<f32>;
@group(1) @binding(1) var normal_tex: texture_2d<f32>;
@group(1) @binding(3) var depth_tex: texture_2d<f32>;
@group(1) @binding(4) var gbuf_sampler: sampler;

struct VsOut {
    @builtin(position) clip: vec4f,
    @location(0) uv: vec2f,
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // 全屏三角形（无需顶点 buffer）：三个越界角点覆盖视口。
    // uv.y 取 0.5 - p.y/2：naga 对 @builtin(position) 注入 Y 翻转（正高度
    // viewport 朝向定案的同一机制），翻转发生在 clip 写出、uv 不随之翻——
    // 图像坐标 y 向下，顶行 ↔ shader-NDC y=+1 ↔ uv.y=0。
    var p = array<vec2f, 3>(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
    var out: VsOut;
    out.clip = vec4f(p[vi], 0.0, 1.0);
    out.uv = vec2f(p[vi].x * 0.5 + 0.5, 0.5 - p[vi].y * 0.5);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    let albedo = textureSampleLevel(albedo_tex, gbuf_sampler, in.uv, 0.0);
    let depth = textureSampleLevel(depth_tex, gbuf_sampler, in.uv, 0.0);
    // 背景：reverse-Z 清 0 = 远平面，无几何写深度——输出清屏色
    if (depth.r <= 1e-6) {
        return vec4f(CLEAR_COLOR, 1.0);
    }
    if frame.mode == MODE_UNLIT {
        return vec4f(albedo.rgb, 1.0);
    }
    let n = normalize(textureSampleLevel(normal_tex, gbuf_sampler, in.uv, 0.0).xyz);
    if frame.mode == MODE_NORMAL {
        return vec4f(n * 0.5 + vec3f(0.5), 1.0);
    }
    if frame.mode == MODE_POSITION {
        // 深度反投影：uv → shader-NDC（uv.y = 0.5 - 0.5·ndc.y 的逆变换）→
        // clip → world（列主序同链）。w 商消除透视除法。
        let ndc = vec3f(in.uv.x * 2.0 - 1.0, 1.0 - 2.0 * in.uv.y, depth.r);
        let clip = vec4f(ndc, 1.0);
        let world = frame.inv_view_proj * clip;
        let world_pos = world.xyz / world.w;
        return vec4f(world_pos * 0.0625 + vec3f(0.5), 1.0);
    }
    // Lambert（mode 0 及未知值兜底）——与 forward Lambert 同公式同常量
    let l_raw = frame.dir_to_light.xyz;
    let l = select(vec3f(0.0, 1.0, 0.0), normalize(l_raw), dot(l_raw, l_raw) > 1e-8);
    let lambert = max(dot(n, l), 0.0);
    let direct = frame.light_color.rgb * lambert * INV_PI;
    let ambient = frame.ambient_color.rgb;
    return vec4f(albedo.rgb * (direct + ambient) * EXPOSURE, 1.0);
}
