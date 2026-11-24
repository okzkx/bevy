// CSM 阴影采样公共片段（8.3 的 PCSS + 9.6 的低档 PCF）：按视深选级联、法线抬升投影、
// 出盒退级、级间淡出。从 `scene/gbuffer/gbuffer_composite.wgsl` 整段抽出（9.6.2 的
// "合成端继续用同一定义"），合成端与 9.6 estimation compute 共用这一份。
//
// 入口文件须声明本片段读的全局（名字即契约）：
//   shadow: ShadowUniforms        —— 级联矩阵/分割带/开关/texel 尺度（common/shadow_ubo.wgsl）
//   shadow_map0..3: texture_2d<f32> —— 四级联深度图（D32，不可过滤，只用 textureLoad）
// 光方向不走全局：调用方把 `safe_light_dir(frame.dir_to_light.xyz)` 传进来，本片段
// 不依赖帧 UBO（compute 入口的 set 序因此少一条）。
//
// 深度比较是 reverse-Z：更近太阳 = 值更大。UBO `map_size.y < 0.5` = 阴影关（可见性恒 1）。
// 采样核是逐像素旋转的螺旋盘（不是规则网格）：近级联 texel 世界尺寸约 1cm，
// 屏幕上一个 texel 不足一像素，固定网格会把阴影图 texel 网格叠成可见交叉网纹。

// 半影宽度 / 遮挡体到接收面距离（米/米），约等于把太阳当成有面积的光源
const SHADOW_PENUMBRA_SCALE: f32 = 0.02;
// 半影半径上限（texel）；同时是出盒判定的采样核余量
const SHADOW_MAX_RADIUS: f32 = 4.0;
// 采样盘 tap 数（黄金角螺旋，半径按 sqrt((i+0.5)/N) 分布 → 盘内面积均匀）
const SHADOW_TAPS: u32 = 16u;
// 黄金角（弧度）：相邻 tap 的角步长，使螺旋在盘内无可见径向条带
const SHADOW_GOLDEN_ANGLE: f32 = 2.3999632;
// blocker 搜索盘半径（texel）：与原 3×3 步长 2（覆盖 ±2 texel）同域
const SHADOW_BLOCKER_RADIUS: f32 = 2.0;
// 级联淡出带宽占本级覆盖范围的比例：末段与下一级线性混合，消除分割带硬切
const SHADOW_FADE_FRACTION: f32 = 0.10;
// 级联覆盖近端（米），镜像 cascades.rs 的 SHADOW_NEAR_DISTANCE
const SHADOW_NEAR_DISTANCE: f32 = 0.5;
// 低档 PCF 的 tap 数（含中心）：estimation 每条命中射线只配一次阴影采样
const SHADOW_LOW_TAPS: f32 = 5.0;

fn shadow_map_load(ci: u32, px: vec2<i32>) -> f32 {
    let last = i32(shadow.map_size.x) - 1;
    let p = clamp(px, vec2<i32>(0), vec2<i32>(last));
    switch ci {
        case 0u: { return textureLoad(shadow_map0, p, 0).r; }
        case 1u: { return textureLoad(shadow_map1, p, 0).r; }
        case 2u: { return textureLoad(shadow_map2, p, 0).r; }
        default: { return textureLoad(shadow_map3, p, 0).r; }
    }
}

// 逐像素旋转相位（弧度）：哈希钉在世界坐标上——相机移动时采样相位不随屏幕漂移
// （无 TAA，逐像素抖动若随屏幕变化会让半影像素"沸腾"）；相邻像素世界点相差 ~1cm
// 就完全去相关，足够把规则网格打散成高频噪点。
fn shadow_rotation(world_pos: vec3f) -> f32 {
    let p = world_pos.xz * 733.0 + vec2f(world_pos.y * 317.0, world_pos.y * 419.0);
    return fract(sin(dot(p, vec2f(12.9898, 78.233))) * 43758.5453) * 6.2831853;
}

// 反照率相关的 bias（米 → 光深度）：斜面上采样点离中心越远，表面自身在图里越"高"。
fn shadow_bias(ci: u32, no_l: f32, tap_distance: f32) -> f32 {
    let texel_world = shadow.texel_world[ci];
    let inv_span = shadow.inv_span[ci];
    let tan_t = sqrt(max(1.0 - no_l * no_l, 0.0)) / max(no_l, 0.25);
    let bias_m = texel_world * (1.0 + tan_t) + 0.01;
    let slope_m = texel_world * tan_t;
    return (bias_m + slope_m * tap_distance) * inv_span;
}

// 单级联 PCSS：px = 接收点 texel 坐标，own = 接收点 reverse-Z 光深度。
// 斜面上采样点离中心越远，表面自身在图里越"高"，bias 按 tap 距离 × 坡度加。
fn shadow_pcss(ci: u32, px: vec2f, own: f32, no_l: f32, rot: f32) -> f32 {
    let inv_span = shadow.inv_span[ci];
    let texel_world = shadow.texel_world[ci];
    let center = vec2<i32>(px);
    // 旋转盘：黄金角步进（复数旋转）代替逐 tap 三角函数；blocker 与 PCF 同一相位。
    let step_c = cos(SHADOW_GOLDEN_ANGLE);
    let step_s = sin(SHADOW_GOLDEN_ANGLE);
    let rot_c = cos(rot);
    let rot_s = sin(rot);
    // —— blocker search（16 tap 旋转盘，外圈 2 texel）
    var blocker_sum = 0.0;
    var blocker_count = 0.0;
    var rc = rot_c;
    var rs = rot_s;
    for (var i = 0u; i < SHADOW_TAPS; i = i + 1u) {
        let r = sqrt((f32(i) + 0.5) * (1.0 / f32(SHADOW_TAPS))) * SHADOW_BLOCKER_RADIUS;
        let off = vec2f(rc, rs) * r;
        let d = shadow_map_load(ci, center + vec2<i32>(round(off)));
        if (d > own + shadow_bias(ci, no_l, length(off))) {
            blocker_sum = blocker_sum + d;
            blocker_count = blocker_count + 1.0;
        }
        let next_c = rc * step_c - rs * step_s;
        rs = rc * step_s + rs * step_c;
        rc = next_c;
    }
    if (blocker_count == 0.0) {
        return 1.0;
    }
    // —— 半影半径：遮挡体到接收面的米距离 × 光源尺度 → texel
    let blocker_m = (blocker_sum / blocker_count - own) / inv_span;
    let radius = clamp(blocker_m * SHADOW_PENUMBRA_SCALE / texel_world, 1.0, SHADOW_MAX_RADIUS);
    // —— PCF（16 tap 旋转盘，外圈 = 半影半径）
    var lit = 0.0;
    rc = rot_c;
    rs = rot_s;
    for (var i = 0u; i < SHADOW_TAPS; i = i + 1u) {
        let r = sqrt((f32(i) + 0.5) * (1.0 / f32(SHADOW_TAPS))) * radius;
        let off = vec2f(rc, rs) * r;
        let d = shadow_map_load(ci, center + vec2<i32>(round(off)));
        lit = lit + select(1.0, 0.0, d > own + shadow_bias(ci, no_l, length(off)));
        let next_c = rc * step_c - rs * step_s;
        rs = rc * step_s + rs * step_c;
        rc = next_c;
    }
    return lit / f32(SHADOW_TAPS);
}

// 低档 PCF（9.6 estimation 用）：中心 + 4 邻域十字共 5 tap，无旋转核、无 blocker 搜索、
// 半影半径恒 1 texel。GI 的每条命中射线都要一次阴影采样，全价 PCSS 在这里太贵；
// 方向光在 GI 里的角色是"挡住即变暗"，不需要半影精度。
fn shadow_pcf_low(ci: u32, px: vec2f, own: f32, no_l: f32) -> f32 {
    let center = vec2<i32>(px);
    let b = shadow_bias(ci, no_l, 1.0);
    var lit = 0.0;
    lit = lit + select(0.0, 1.0, shadow_map_load(ci, center) <= own + b);
    lit = lit + select(0.0, 1.0, shadow_map_load(ci, center + vec2<i32>(1, 0)) <= own + b);
    lit = lit + select(0.0, 1.0, shadow_map_load(ci, center + vec2<i32>(-1, 0)) <= own + b);
    lit = lit + select(0.0, 1.0, shadow_map_load(ci, center + vec2<i32>(0, 1)) <= own + b);
    lit = lit + select(0.0, 1.0, shadow_map_load(ci, center + vec2<i32>(0, -1)) <= own + b);
    return lit / SHADOW_LOW_TAPS;
}

// 单级采样：投影点（法线抬升后）落出本级盒返回 -1——调用方按原语义退到下一级。
fn shadow_cascade(ci: u32, world_pos: vec3f, n: vec3f, no_l: f32, rot: f32) -> f32 {
    let size = shadow.map_size.x;
    let margin = SHADOW_MAX_RADIUS + 3.0;
    // 法线抬升（防自遮蔽）：1~3 texel，掠射面抬得多
    let lifted = world_pos + n * (shadow.texel_world[ci] * (1.0 + 2.0 * (1.0 - no_l)));
    let cs = shadow.cascade_view_proj[ci] * vec4f(lifted, 1.0);
    let ndc = cs.xyz / cs.w;
    let px = vec2f(ndc.x * 0.5 + 0.5, 0.5 - 0.5 * ndc.y) * size;
    let inside = all(px >= vec2f(margin)) && all(px <= vec2f(size - margin))
        && ndc.z >= 0.0 && ndc.z <= 1.0;
    if (!inside) {
        return -1.0;
    }
    return shadow_pcss(ci, px, ndc.z, no_l, rot);
}

// 低档版的单级采样：同一条投影与出盒判据，核换成 5 tap PCF。
fn shadow_cascade_low(ci: u32, world_pos: vec3f, n: vec3f, no_l: f32) -> f32 {
    let size = shadow.map_size.x;
    let margin = SHADOW_MAX_RADIUS + 3.0;
    let lifted = world_pos + n * (shadow.texel_world[ci] * (1.0 + 2.0 * (1.0 - no_l)));
    let cs = shadow.cascade_view_proj[ci] * vec4f(lifted, 1.0);
    let ndc = cs.xyz / cs.w;
    let px = vec2f(ndc.x * 0.5 + 0.5, 0.5 - 0.5 * ndc.y) * size;
    let inside = all(px >= vec2f(margin)) && all(px <= vec2f(size - margin))
        && ndc.z >= 0.0 && ndc.z <= 1.0;
    if (!inside) {
        return -1.0;
    }
    return shadow_pcf_low(ci, px, ndc.z, no_l);
}

// 起始级联的选择结果（WGSL 没有元组，回结构体）。
struct ShadowCascadePick {
    start: u32,
    view_depth: f32,
}

// 按视深选起始级联（0..4；4 = 超出最后一级）。
fn shadow_pick_cascade(world_pos: vec3f) -> ShadowCascadePick {
    let view_depth = dot(world_pos - shadow.camera_pos.xyz, shadow.camera_forward.xyz);
    var start = 4u;
    for (var c = 0u; c < 4u; c = c + 1u) {
        if (view_depth <= shadow.splits[c]) {
            start = c;
            break;
        }
    }
    var pick: ShadowCascadePick;
    pick.start = start;
    pick.view_depth = view_depth;
    return pick;
}

fn shadow_visibility(world_pos: vec3f, n: vec3f, light_dir: vec3f) -> f32 {
    if (shadow.map_size.y < 0.5) {
        return 1.0;
    }
    let pick = shadow_pick_cascade(world_pos);
    let view_depth = pick.view_depth;
    let start = pick.start;
    let l = normalize(light_dir);
    let no_l = clamp(dot(n, l), 0.0, 1.0);
    let rot = shadow_rotation(world_pos);
    // 选级 + 出盒退级：本级不可采样就试更粗一级，四级都出盒才判无影
    var used = 4u;
    var vis = 1.0;
    for (var c = start; c < 4u; c = c + 1u) {
        let v = shadow_cascade(c, world_pos, n, no_l, rot);
        if (v >= 0.0) {
            vis = v;
            used = c;
            break;
        }
    }
    if (used >= 4u) {
        return 1.0;
    }
    // 级联淡出：只在本级覆盖范围末段（10% 带宽）与下一级混合。视深在淡出带内时
    // 两级各采一次，smoothstep 权重从 0 到 1；投影点在本级盒内才走到这里，出盒
    // fallback（used > start）的视深不落在本级末段，天然不参与淡出。
    if (used + 1u < 4u) {
        let prev = max(used, 1u) - 1u;
        let near = select(SHADOW_NEAR_DISTANCE, shadow.splits[prev], used > 0u);
        let far = shadow.splits[used];
        let band = max((far - near) * SHADOW_FADE_FRACTION, 1e-3);
        let fade = smoothstep(far - band, far, view_depth);
        if (fade > 0.0) {
            let next = shadow_cascade(used + 1u, world_pos, n, no_l, rot);
            if (next >= 0.0) {
                vis = mix(vis, next, fade);
            }
        }
    }
    return vis;
}

// 低档版（9.6）：同样的选级/出盒退级/级间淡出，核换成 5 tap PCF。背光面直接返回 1
//（调用方乘 NoL，背光本来没有贡献，避免在背光面白跑阴影采样）。
fn shadow_visibility_low(world_pos: vec3f, n: vec3f, light_dir: vec3f) -> f32 {
    if (shadow.map_size.y < 0.5) {
        return 1.0;
    }
    let l = normalize(light_dir);
    let no_l = clamp(dot(n, l), 0.0, 1.0);
    if (no_l <= 0.0) {
        return 1.0;
    }
    let pick = shadow_pick_cascade(world_pos);
    let view_depth = pick.view_depth;
    let start = pick.start;
    var used = 4u;
    var vis = 1.0;
    for (var c = start; c < 4u; c = c + 1u) {
        let v = shadow_cascade_low(c, world_pos, n, no_l);
        if (v >= 0.0) {
            vis = v;
            used = c;
            break;
        }
    }
    if (used >= 4u) {
        return 1.0;
    }
    if (used + 1u < 4u) {
        let prev = max(used, 1u) - 1u;
        let near = select(SHADOW_NEAR_DISTANCE, shadow.splits[prev], used > 0u);
        let far = shadow.splits[used];
        let band = max((far - near) * SHADOW_FADE_FRACTION, 1e-3);
        let fade = smoothstep(far - band, far, view_depth);
        if (fade > 0.0) {
            let next = shadow_cascade_low(used + 1u, world_pos, n, no_l);
            if (next >= 0.0) {
                vis = mix(vis, next, fade);
            }
        }
    }
    return vis;
}
