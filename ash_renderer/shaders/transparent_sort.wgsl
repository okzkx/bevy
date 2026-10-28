// 透明 GPU bitonic 排序（compute 单入口）：把 collect 产的 (深度键, 行)
// 对按键升序全排——键 = 包围盒中心视图深度（clip.w），padding = -inf（宿主
// 每帧 fill）。gather 端从升序数组**尾部反向**取（最远先画 = back-to-front，
// 近似排序精度与 CPU 同档，物体级）。
//
// 网络形状（标准 bitonic，N = TRANSPARENT_CAPACITY = 16384，pow2）：
//   for k in 2..=N (×2)  { for j in k/2..1 (÷2) { dispatch(k, j) } }
// 105 次派发，每派发 N/2 线程各管一对 (i, i+j)；配对块由 j 决定。
// push（var<immediate>，8B）：(k, j) 段参数——宿主预生成整段序列逐派发。
//
// 并发语义：每线程独占读改写一对元素（i 与 i^k 互补分派，无写冲突）；
// 派发间屏障由录制层逐派发插（compute rw → compute rw）。

const TRANSPARENT_CAPACITY: u32 = 16384u;

@group(0) @binding(0) var<storage, read_write> t_index_list: array<u32>;
@group(0) @binding(1) var<storage, read_write> t_keys: array<f32>;

var<immediate> push: PushSort;
struct PushSort {
    k: u32,  // 当前段长（2 的幂，覆盖对大小）
    j: u32,  // 方向块大小（决定升/降）
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    // 每个线程映射到一个 j 配对块中的较小下标；半数线程覆盖全长网络的
    // 全部 N/2 对，而不是只处理数组前半段。
    let i = (gid.x / push.j) * (2u * push.j) + (gid.x % push.j);
    let partner = i + push.j;
    // 容量与派发恒定匹配；守卫保留给越界派发配置错误的防御。
    if partner >= TRANSPARENT_CAPACITY {
        return;
    }
    // 方向由段长 k 决定（经典 bitonic：k 块前半升后半降），j 是段内配对距离。
    // 最外层方向按容量选为全升序，保证 -inf padding 落在前缀。
    let ascending = (i & push.k) == 0u || push.k == TRANSPARENT_CAPACITY;
    let a = t_keys[i];
    let b = t_keys[partner];
    let swap = select(b > a, a > b, ascending);
    if (swap) {
        t_keys[i] = b;
        t_keys[partner] = a;
        let ia = t_index_list[i];
        let ib = t_index_list[partner];
        t_index_list[i] = ib;
        t_index_list[partner] = ia;
    }
}
