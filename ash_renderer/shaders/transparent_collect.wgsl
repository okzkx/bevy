// 透明行收集（compute 单入口）：从在场环筛材质 alpha<1 的行，atomicAdd
// 紧凑成"透明行索引 + 深度键"列表——深度键（模型包围盒中心的视图深度）是
// transparent_sort bitonic 排序的排序键。
//
// 接口：set0 = set2（对象表 b0 / 模型环 b1 / 材质环 b2 / 在场环 b4），
// set1 = 光源分簇集（只读 b0 ClusterParams 的 view_proj），set2 = 透明命令链集
//（b0 索引列表 / b1 键 / b3 计数——原子累加，宿主每帧清零）。push（var<immediate>，4B）：count = 在场行数（dispatch
// 上界，CPU 在账）。
//
// 并发语义：atomicAdd 抢透明行下标（列表顺序 = 完成序，非在场序——排序与
// 绘制均不依赖该顺序）；索引/键写入下标互斥，普通 store。
// 超容量（TRANSPARENT_CAPACITY）：计数照实累加（诊断信号），行丢弃。
// #import instance
// #import cluster

@group(0) @binding(0) var<storage, read> objects: array<ObjectRecord>;
@group(0) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(0) @binding(2) var<storage, read> materials: array<MaterialRecord>;
@group(0) @binding(4) var<storage, read> presence: array<u32>;
@group(1) @binding(0) var<storage, read> params: ClusterParams;
@group(2) @binding(0) var<storage, read_write> t_index_list: array<u32>;
@group(2) @binding(1) var<storage, read_write> t_keys: array<f32>;
@group(2) @binding(3) var<storage, read_write> t_count: array<atomic<u32>>;

const TRANSPARENT_CAPACITY: u32 = 16384u;  // transparent.rs 同名常量

var<immediate> push: PushCollect;
struct PushCollect {
    count: u32,  // 在场行数（dispatch 上界）
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if i >= push.count {
        return;
    }
    let slot = presence[i];
    let rec = objects[slot];
    let alpha = materials[rec.material_index].base_color.a;
    if (alpha < 1.0) {
        let idx = atomicAdd(&t_count[0], 1u);
        if (idx < TRANSPARENT_CAPACITY) {
            t_index_list[idx] = slot;
            // 深度键 = 包围盒中心（局部）经模型变换的视图深度（clip.w = 视图
            // 深度，透视投影恒真）；transparent_sort 按它升序排。
            let model = models[slot];
            let center = (rec.aabb_min.xyz + rec.aabb_max.xyz) * 0.5;
            let clip = params.view_proj * (model * vec4f(center, 1.0));
            t_keys[idx] = clip.w;
        }
    }
}
