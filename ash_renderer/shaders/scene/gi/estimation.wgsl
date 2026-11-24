// 9.6 estimation（compute）：每个活跃 patch 每帧在法线周围发少量均匀半球射线，
// 命中点取直接光（9.6.1 首版只有方向光 + 低档 PCF 阴影；光源侧清单与自发光三角形
// 在 9.6.2 接上）+ 多跳自查询（9.6.3），投影成 RGBL1 辐照度并按方差引导做时域混合。
//
// 数学链（与 Unity `Estimation.compute` 同形，见 common/gi_sampling.wgsl 的头注）：
//   逐样本 radiance → 按 Y 基常数投影 → ×(1/pdf)/N（半球均匀 pdf = 1/2π）→ 三路合流
//   → 余弦卷积折基常数 → 时域混合 → 写 ping-pong 的另一份。
// 未命中的射线取天空辐亮度（与 9.2 的 IBL 捕获同源）；`ibl_enabled = 0` 时取 0，
// 与 9.5 种子（IBL 关 = 零种子）口径一致，所以关掉 IBL 也不会凭空多出环境项。
//
// ping-pong：写 `b3 if frame&1==0 else b4`，读另一份（= 上一帧的结果）。读者（⑧ 段视图与
// 9.7 的屏幕查表）必须按同一个奇偶取，奇偶由 GI UBO 的 `frame.x & 1` 给出。
//
// 接口：
// - set0 b0：帧 UBO（方向光方向/强度、IBL 开关与强度、天空参数）。
// - set1：GI 表（b2 几何 / b3+b4 辐照度双份 / b5 统计 / b6 patch→cell / b8 计数 / b9 GI UBO）。
// - set2：实例表（b0 对象记录 / b2 材质环）。
// - set3：光线求交集（b0 TLAS / b1 顶点池 / b2 索引池；9.4 那一份，不绑其调试图与统计）。
// - set4：阴影集（b0 阴影 UBO / b1..b4 四级联图，用低档 5 tap PCF）。
// - set5：bindless 贴图（b0 贴图数组 / b1 采样器数组，取命中点 albedo）。
//
// 线程 = 池容量（每线程一条 patch，非活跃直接返回）。
enable wgpu_ray_query;
enable wgpu_binding_array;
// #import frame
// #import gi_cell
// #import gi_sampling
// #import gi_light_sample
// #import gi_emissive
// #import gi_bounce
// #import cluster
// #import instance
// #import raytracing
// #import sky
// #import shadow_ubo
// #import shadow

@group(0) @binding(0) var<uniform> frame: FrameUniforms;
@group(1) @binding(0) var<storage, read> cell_patch: array<u32>;
@group(1) @binding(2) var<storage, read> patch_geometry: array<PatchGeometry>;
@group(1) @binding(3) var<storage, read_write> patch_irradiance0: array<PatchIrradiance>;
@group(1) @binding(4) var<storage, read_write> patch_irradiance1: array<PatchIrradiance>;
@group(1) @binding(5) var<storage, read_write> patch_statistics: array<PatchStatistics>;
@group(1) @binding(6) var<storage, read> patch_cell: array<u32>;
@group(1) @binding(8) var<storage, read_write> counters: array<atomic<u32>, 16>;
@group(1) @binding(9) var<uniform> gi: GiUniforms;
@group(1) @binding(10) var<storage, read> gi_lights: array<LightRecord>;
@group(1) @binding(11) var<storage, read> light_samples: array<LightSample>;
@group(1) @binding(12) var<storage, read> emissive_triangles: array<EmissiveTriangle>;
@group(1) @binding(13) var<storage, read_write> bounce_requests: array<BounceRequest>;
@group(2) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(2) @binding(2) var<storage, read> materials: array<MaterialRecord>;
@group(3) @binding(0) var scene_tlas: acceleration_structure;
@group(3) @binding(1) var<storage, read> vertices: array<f32>;
@group(3) @binding(2) var<storage, read> indices: array<u32>;
@group(4) @binding(0) var<uniform> shadow: ShadowUniforms;
@group(4) @binding(1) var shadow_map0: texture_2d<f32>;
@group(4) @binding(2) var shadow_map1: texture_2d<f32>;
@group(4) @binding(3) var shadow_map2: texture_2d<f32>;
@group(4) @binding(4) var shadow_map3: texture_2d<f32>;
@group(5) @binding(0) var textures: binding_array<texture_2d<f32>>;
@group(5) @binding(1) var samplers: binding_array<sampler>;

// 射线起点沿法线的抬升比例（体素边长的千分之一，Unity `GetAdditionalRayOffset` 同值）
// 再加一个绝对下限：极粗级联下体素可能很大，抬太多会让贴地的 patch 漏掉接触阴影。
const RAY_ORIGIN_OFFSET_RELATIVE: f32 = 0.001;
const RAY_ORIGIN_OFFSET_ABSOLUTE: f32 = 1e-4;
// tmax：不裁剪距离（GI 要看整条射线），只防 inf
const RAY_TMAX: f32 = 3.4e38;
// 射线掩码：与 tlas_build 写死的 0xFF 按位与（全不透明层）
const RAY_MASK: u32 = 0xFFu;

// 未命中：天空辐亮度。以帧 UBO 的 `ibl_enabled` 为准（与种子同一道开关），
// 强度乘数与 IBL 一致；常量天空自检走 sky.wgsl 的 `sky_uniform` 分支。
fn environment_radiance(dir: vec3f) -> vec3f {
    if (frame.ibl_enabled == 0u) {
        return vec3f(0.0);
    }
    return sky_radiance(dir, false) * frame.ibl_intensity;
}

// 多跳自查询（9.6.3）：命中点 → 级联/桶/cell → patch，读**上一帧**那份辐照度
// （本帧写 A 就读 B，写 B 就读 A），求值成 E(n) 后由调用方乘命中点 albedo/π。
//
// 同时承担 bounce 分配：cell 里没有 patch、且本 patch 有分配权（`frame.z != 0` 且
// rank 0）时，往请求表追加一条（位置/法线取命中点），同帧末尾的 bounce_alloc 消费。
// 桶归属只能用命中重建的插值法线（`hit_world_normal`），而播种用的是深度差分平面法线
// ——光滑曲面上两者不同源，这里会有桶错位（9.5 §0.5 的已知例外），错位率用
// `counters[8]`（命中）/`counters[15]`（落空）两个读数估。
fn cache_evaluate(p: vec3f, n: vec3f, write_second: bool, can_allocate: bool) -> vec3f {
    let center = gi.target_pos.xyz;
    let res = u32(gi.volume.y);
    let cascades = u32(gi.volume.z);
    let voxel_min = gi.volume.x;
    let cascade = resolve_cascade(center, p, voxel_min, res, cascades);
    if (cascade == INVALID_PATCH) {
        return vec3f(0.0);
    }
    let voxel = voxel_size(voxel_min, cascade);
    let offset = gi.cascade_offsets[cascade].xyz;
    let pv = volume_position(p, voxel, offset, i32(res));
    let storage = volume_to_storage(pv, offset, i32(res));
    let bucket = direction_to_bucket_index(n, BUCKET_RESOLUTION);
    let cell = cell_index(cascade, storage, bucket, res);
    // 名字不用 patch：WGSL 把 patch 列为保留关键字
    let patch_index = cell_patch[cell];
    if (patch_index == INVALID_PATCH) {
        atomicAdd(&counters[GI_COUNTER_BOUNCE_MISSES], 1u);
        if (can_allocate) {
            let request_slot = atomicAdd(&counters[GI_COUNTER_BOUNCE_REQUESTS], 1u);
            // 超限截断：计数照加（读数能看出超了多少），不越界写
            if (request_slot < GI_BOUNCE_REQUEST_CAPACITY) {
                var request: BounceRequest;
                request.cell = vec4u(cell, 0u, 0u, 0u);
                request.position = vec4f(p, 1.0);
                request.normal = vec4f(n, 0.0);
                bounce_requests[request_slot] = request;
            }
        }
        return vec3f(0.0);
    }
    atomicAdd(&counters[GI_COUNTER_BOUNCE_HITS], 1u);
    // 读另一份 = 读上一帧（写 A 读 B，写 B 读 A）
    var previous: PatchIrradiance;
    if (write_second) {
        previous = patch_irradiance0[patch_index];
    } else {
        previous = patch_irradiance1[patch_index];
    }
    return previous.l0.xyz
        + previous.l1x.xyz * n.x
        + previous.l1y.xyz * n.y
        + previous.l1z.xyz * n.z;
}

// 命中点朝入射方向反射出去的辐亮度：albedo/π ·（方向光 · NoL · 阴影 + 多跳缓存 E）。
fn hit_radiance(hit: SceneHit, light_dir: vec3f, write_second: bool, can_allocate: bool) -> vec3f {
    let geo = hit_geometry(hit.instance_slot, hit.primitive, hit.bary);
    let world_pos = (hit.object_to_world * vec4f(geo.position, 1.0)).xyz;
    var world_normal = (hit.object_to_world * vec4f(geo.normal, 0.0)).xyz;
    let len_n = length(world_normal);
    if (len_n > 0.0) {
        world_normal = world_normal / len_n;
    }
    let material = materials[geo.material_index];
    var albedo = material.base_color.rgb;
    if (material.tex_base_color != NO_TEXTURE) {
        let sampled = textureSampleLevel(
            textures[material.tex_base_color],
            samplers[material.sampler_index],
            geo.uv,
            0.0,
        );
        albedo = albedo * sampled.rgb;
    }
    let l = normalize(light_dir);
    let no_l = max(dot(world_normal, l), 0.0);
    var radiance = vec3f(0.0);
    if (no_l > 0.0) {
        let visibility = shadow_visibility_low(world_pos, world_normal, l);
        // light_color.rgb 已含强度（见 frame.wgsl 的字段注释），这里只差 BRDF 的 1/π
        radiance = albedo * (1.0 / PI) * frame.light_color.rgb * no_l * visibility;
    }
    // 多跳：命中点自己也要有"被环境照亮"的那一份，否则墙角的间接光到不了下一跳
    if (gi.estimation.z != 0u) {
        let cached = cache_evaluate(world_pos, world_normal, write_second, can_allocate);
        radiance = radiance + albedo * (1.0 / PI) * cached;
    }
    return radiance;
}

// 光源侧清单的重连接取样（照 Unity `SamplePunctualLightBounceRadiance`）：
// 清单里的一条采样给了"灯照亮了某个隐面点"，这里从 patch 往那个点补一条重连接射线验证
// 遮挡，再用面积↔立体角的 Jacobian 把"隐面点被照亮的辐亮度"换算成 patch 方向上的辐射亮度。
// 归一化：`hit_normal.w` 里存着 1/pdf（= 4π × 灯数），这里不再乘任何采样数——
// 调用方最后统一除样本条数。
fn punctual_bounce_radiance(sample_index: u32, position: vec3f, normal: vec3f, origin: vec3f) -> vec3f {
    let record = light_samples[sample_index];
    if (record.info.y == 0u) {
        return vec3f(0.0);
    }
    let to_hit = record.hit_pos.xyz - position;
    // 隐面点必须落在 patch 的前半球（Unity 的 planeDistance > epsilon）
    let plane_distance = dot(normal, to_hit);
    if (plane_distance <= 0.01) {
        return vec3f(0.0);
    }
    let distance_squared = dot(to_hit, to_hit);
    let reconnection_distance = sqrt(max(distance_squared, 1e-8));
    let direction = to_hit / reconnection_distance;
    // 重连接射线：打近一点（留 2cm 余量）只判遮挡，不要求命中同一三角形
    atomicAdd(&counters[9], 1u);
    let occluder = trace_scene(origin, direction, 0.0, reconnection_distance - 0.02, RAY_MASK);
    if (occluder.hit) {
        return vec3f(0.0);
    }
    let light = gi_lights[record.info.x];
    if (light.pos_radius.w <= 0.0) {
        return vec3f(0.0);
    }
    let light_to_hit = record.hit_pos.xyz - light.pos_radius.xyz;
    let light_distance_squared = max(dot(light_to_hit, light_to_hit), 1e-6);
    let light_direction = light_to_hit / sqrt(light_distance_squared);
    // 隐面点被这盏灯照亮的辐亮度：cos_bounce · 距离衰减（含 1/d² 与作用半径的平滑窗）
    let cos_bounce = max(dot(-light_direction, record.hit_normal.xyz), 0.0);
    if (cos_bounce <= 0.0) {
        return vec3f(0.0);
    }
    let falloff = 1.0 / max(light.pos_radius.w * light.pos_radius.w, 1e-8);
    let window = getSquareFalloffAttenuation(light_distance_squared, falloff);
    if (window <= 0.0) {
        return vec3f(0.0);
    }
    let emitted = light.color_intensity.rgb * light.color_intensity.w * (window / light_distance_squared) * cos_bounce;
    let reflected = record.albedo.rgb * (1.0 / PI) * emitted;
    // 面积测度 → patch 方向立体角：cos_rec / d²，再乘 1/pdf
    let cos_reconnection = max(dot(-direction, record.hit_normal.xyz), 0.0);
    let contribution = reflected * (cos_reconnection / max(distance_squared, 1e-6)) * record.hit_normal.w;
    // 有限性兜底：极近的重连接点会炸出 Inf/NaN，一个坏样本会把整条 patch 染黑
    if (!(contribution.x == contribution.x) || !(contribution.y == contribution.y) || !(contribution.z == contribution.z)) {
        return vec3f(0.0);
    }
    return contribution;
}

// 自发光面积采样（第三路）：均匀抽三角形 + 解析面积权重。
// pdf（面积测度）= 1/(N·area_i)，所以 1/pdf = N·area_i；再乘面积→立体角的
// Jacobian `cos/d²` 就是这条样本对方向积分的权重。定向光的可见性用一条阴影射线判。
fn emissive_area_sample(triangle_index: u32, u_primary: vec2f, u_secondary: vec2f,
    origin: vec3f) -> vec3f {
    let record = emissive_triangles[triangle_index];
    // 均匀重心采样：r1 = sqrt(1-ξ1) 的常规参数化（两分量之和 ≤ 1）
    let r1 = sqrt(max(0.0, 1.0 - u_primary.y));
    let b1 = r1 * (1.0 - u_secondary.x);
    let b2 = r1 * u_secondary.x;
    let point = record.p0.xyz + record.e1.xyz * b1 + record.e2.xyz * b2;
    let to_point = point - origin;
    let distance_squared = dot(to_point, to_point);
    if (distance_squared < 1e-8) {
        return vec3f(0.0);
    }
    let distance = sqrt(distance_squared);
    let direction = to_point / distance;
    // 阴影射线：打近 2cm 留余量，命中即被挡
    atomicAdd(&counters[9], 1u);
    let occluder = trace_scene(origin, direction, 0.0, distance - 0.02, RAY_MASK);
    if (occluder.hit) {
        return vec3f(0.0);
    }
    let triangle_normal = cross(record.e1.xyz, record.e2.xyz);
    let normal_length = length(triangle_normal);
    if (!(normal_length > 0.0)) {
        return vec3f(0.0);
    }
    // 双面自发光：取 |cos|，两面的贡献都算
    let cos_triangle = abs(dot(triangle_normal / normal_length, direction));
    if (!(cos_triangle > 0.0)) {
        return vec3f(0.0);
    }
    return record.emissive.rgb * (cos_triangle / distance_squared);
}

// 方差引导的滞后（Unity `FilterTemporallyVarianceGuided`）：
// - 稳定后权重 = lerp(0.995, short_hysteresis, driftWeight)，drift 越小越靠近长滞后（更稳），
//   越大越靠近短滞后（更快跟手）；drift = |短期均值 − 旧 l0| / 标准差，权重再 smoothstep。
// - `old_sample_count != updateMax` 时是**精确累计平均**（old/(old+new)），不是指数混合：
//   样本少时指数平均有偏，到 updateMax 之后才转指数。
fn filter_temporally_variance_guided(
    short_hysteresis: f32,
    new_sample_count: u32,
    old_sample_count: u32,
    short_variance: vec3f,
    short_mean: vec3f,
    new_estimate: GiRgbl1,
    old_estimate: GiRgbl1,
    update_max: u32,
) -> GiRgbl1 {
    let std_dev = sqrt(max(short_variance, vec3f(0.0)));
    let drift = abs(short_mean - old_estimate.l0) / max(std_dev, vec3f(1e-9));
    // 经验系数 1/4：稳定度与反应速度的折中（Unity 同值）
    let drift_weight = smoothstep(vec3f(0.0), vec3f(1.0), drift * 0.25);
    var update_weight = mix(vec3f(0.995), vec3f(short_hysteresis), drift_weight);
    if (old_sample_count != update_max) {
        update_weight = vec3f(f32(old_sample_count) / f32(old_sample_count + new_sample_count));
    }
    return gi_rgbl1_lerp(new_estimate, old_estimate, update_weight);
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3u) {
    let patch_idx = gid.x;
    let capacity = u32(gi.volume.w);
    if (patch_idx >= capacity) {
        return;
    }
    // 活跃判据：patch→cell 反向指针为 INVALID = 空槽（与 ring 水位无关，整理留下洞也准）
    if (patch_cell[patch_idx] == INVALID_PATCH) {
        return;
    }
    // 帧稀释（9.6.5）：样本档降到 1 还超预算时按"patch 哈希取模"跳帧，让超出的部分
    // 摊到后续帧——用哈希而不是池下标的取模，避免按池顺序成块跳档（那会看出条带）。
    let stride = max(gi.estimation_l.z, 1u);
    if (stride > 1u) {
        if (low_bias_hash_32(patch_idx) % stride != gi.frame.x % stride) {
            return;
        }
    }
    let geometry = patch_geometry[patch_idx];
    let position = geometry.position.xyz;
    let normal_raw = geometry.normal.xyz;
    let len_normal = length(normal_raw);
    if (len_normal <= 0.0) {
        return;
    }
    let normal = normal_raw / len_normal;
    let stats = patch_statistics[patch_idx];
    let rank = stats.state & 0xFFu;
    let old_count = stats.update_count;

    // 预热：新 patch（从没估计过、且不是 bounce 出来的）一次吃 samples × warmup 条样本，
    // 让首帧就有可用的低方差估计；否则逐帧 samples 条。样本偏移两态不同（Unity 同款），
    // 写反会让预热样本与稳态样本区间重叠。
    let samples = max(gi.estimation.x, 1u);
    let warmup = max(gi.estimation.y, 1u);
    let warm = old_count == 0u && rank == 0u;
    var sample_count = samples;
    var sample_offset = samples * (gi.frame.x + warmup);
    if (warm) {
        sample_count = samples * warmup;
        sample_offset = (gi.frame.x + 1u) * samples;
    }

    var rng = gi_r2_init(low_bias_hash_32(patch_idx), sample_offset);
    let origin = position + normal * (gi.volume.x * RAY_ORIGIN_OFFSET_RELATIVE
        + RAY_ORIGIN_OFFSET_ABSOLUTE);
    let light_dir = frame.dir_to_light.xyz;
    // 本帧写哪一份、以及能否申请 bounce 分配（只有 rank 0 的 patch 可以，Unity 同款）
    let write_second = (gi.frame.x & 1u) == 1u;
    let can_allocate = gi.frame.z != 0u && rank == 0u;

    var accumulator = gi_rgbl1_zero();
    var valid = 0u;
    for (var i = 0u; i < sample_count; i = i + 1u) {
        let u = gi_r2_sample2(rng, 0u);
        let dir = uniform_hemisphere_sample(u, normal);
        gi_r2_next(&rng);
        let hit = trace_scene(origin, dir, 0.0, RAY_TMAX, RAY_MASK);
        if (hit.hit) {
            let radiance = hit_radiance(hit, light_dir, write_second, can_allocate);
            gi_project_accumulate(&accumulator, radiance, dir);
            atomicAdd(&counters[10], 1u);
        } else {
            gi_project_accumulate(&accumulator, environment_radiance(dir), dir);
        }
        valid = valid + 1u;
    }
    atomicAdd(&counters[9], sample_count);
    atomicAdd(&counters[12], 1u);
    atomicAdd(&counters[13], sample_count);
    if (valid == 0u) {
        return;
    }

    // 第一路（迹线）：1/pdf · 1/N（半球均匀）
    var total = gi_rgbl1_scale(accumulator, GI_TWO_PI / f32(valid));

    // 第二路（光源侧清单）：每条样本自带 1/pdf（记录里），这里只除样本条数。
    // 两路的归一化不同，必须在各自归一化之后再合流，否则点光那路会被 2π/valid 放大。
    let light_sample_count = gi.estimation_l.y;
    if (light_sample_count > 0u && gi.estimation_l.x > 0u) {
        // 单独一条序列（Unity 也是重新 Init 同一个 seed/offset），避免与半球样本相关
        var light_rng = gi_r2_init(low_bias_hash_32(patch_idx) ^ 0x517CC1B7u, sample_offset);
        var punctual = gi_rgbl1_zero();
        for (var i = 0u; i < sample_count; i = i + 1u) {
            let u = gi_r2_sample2(light_rng, 0u);
            gi_r2_next(&light_rng);
            let pick = min(u32(u.x * f32(light_sample_count)), light_sample_count - 1u);
            let radiance = punctual_bounce_radiance(pick, position, normal, origin);
            if (all(radiance == vec3f(0.0))) {
                continue;
            }
            gi_project_accumulate(&punctual, radiance, normalize(light_samples[pick].hit_pos.xyz - position));
        }
        gi_rgbl1_add_scaled(&total, punctual, 1.0 / f32(sample_count));
    }

    // 第三路（自发光面积采样）：条数取 counters[14]（超容量时夹到容量）。
    let emissive_count = min(atomicLoad(&counters[14]), GI_EMISSIVE_CAPACITY);
    let emissive_intensity = gi.estimation_f.y;
    if (emissive_count > 0u && emissive_intensity > 0.0) {
        var emissive_rng = gi_r2_init(low_bias_hash_32(patch_idx) ^ 0x2545F491u, sample_offset);
        var emissive_acc = gi_rgbl1_zero();
        for (var i = 0u; i < sample_count; i = i + 1u) {
            let primary = gi_r2_sample2(emissive_rng, 0u);
            let secondary = gi_r2_sample2(emissive_rng, 1u);
            gi_r2_next(&emissive_rng);
            let pick = min(u32(primary.x * f32(emissive_count)), emissive_count - 1u);
            let record = emissive_triangles[pick];
            let radiance = emissive_area_sample(pick, primary, secondary, origin);
            if (all(radiance == vec3f(0.0))) {
                continue;
            }
            // 1/pdf = N · area（面积测度），再乘自发光强度与全局倍数
            let weight = f32(emissive_count) * record.p0.w * emissive_intensity;
            gi_project_accumulate(&emissive_acc, radiance * weight, normalize(record.p0.xyz - origin));
        }
        gi_rgbl1_add_scaled(&total, emissive_acc, 1.0 / f32(sample_count));
    }

    // 余弦卷积折基常数 → 求值式辐照度
    let estimate = gi_cosine_convolve_fold(total);

    // 上一帧的结果在哪一份（ping-pong，读另一份 = 读上一帧）
    var previous: PatchIrradiance;
    if (write_second) {
        previous = patch_irradiance0[patch_idx];
    } else {
        previous = patch_irradiance1[patch_idx];
    }
    var old_estimate = gi_rgbl1_zero();
    old_estimate.l0 = previous.l0.xyz;
    old_estimate.l1x = previous.l1x.xyz;
    old_estimate.l1y = previous.l1y.xyz;
    old_estimate.l1z = previous.l1z.xyz;

    // —— 时域（Unity `ProcessAndStoreRadianceSample` 的等价实现）——
    let short_hysteresis = gi.estimation_f.x;
    let update_max = gi.frame.w;
    // 预热那一趟按"相当于几帧的更新"记账：warmup 倍样本一次算 warmup 次更新
    let new_estimate_update_count = max(sample_count / samples, 1u);
    let new_count = min(old_count + new_estimate_update_count, update_max);
    let old_mean = stats.mean.xyz;
    let old_variance = stats.variance.xyz;
    // 短期滞后权重：样本少时按 old/(old+new) 收敛，不越过 short_hysteresis
    var short_weight = 0.0;
    if (old_count != 0u) {
        short_weight = min(
            f32(old_count) / f32(old_count + new_estimate_update_count),
            short_hysteresis,
        );
    }
    let new_l0_short = mix(estimate.l0, old_mean, vec3f(short_weight));
    let variance_sample = (estimate.l0 - new_l0_short) * (estimate.l0 - old_mean);
    let new_variance = mix(variance_sample, old_variance, vec3f(short_hysteresis));

    let blended = filter_temporally_variance_guided(
        short_hysteresis,
        new_estimate_update_count,
        old_count,
        new_variance,
        new_l0_short,
        estimate,
        old_estimate,
        update_max,
    );

    var out: PatchIrradiance;
    out.l0 = vec4f(blended.l0, 0.0);
    out.l1x = vec4f(blended.l1x, 0.0);
    out.l1y = vec4f(blended.l1y, 0.0);
    out.l1z = vec4f(blended.l1z, 0.0);
    if (write_second) {
        patch_irradiance1[patch_idx] = out;
    } else {
        patch_irradiance0[patch_idx] = out;
    }

    var updated = stats;
    updated.mean = vec4f(new_l0_short, 0.0);
    updated.variance = vec4f(new_variance, 0.0);
    updated.update_count = new_count;
    patch_statistics[patch_idx] = updated;
}
