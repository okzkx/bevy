// 透明 GPU bitonic 排序（compute 单入口）：把 collect 产的 (深度键, 行)
// 对按键升序全排——键 = 包围盒中心视图深度（clip.w），padding = -inf（宿主
// 每帧 fill）。gather 端从升序数组**尾部反向**取（最远先画 = back-to-front，
// 近似排序精度与 CPU 同档，物体级）。
//
// 网络形状（标准 bitonic，N = TRANSPARENT_CAPACITY = 16384，pow2）：
//   for k in 2..=N (×2)  { for j in k/2..1 (÷2) { dispatch(k, j) } }
// 105 次派发，每派发 N/2 线程各管一对 (i, i^k)；方向块由 j 决定。
// push（var<immediate>，8B）：(k, j) 段参数——宿主预生成整段序列逐派发。
//
// 并发语义：每线程独占读改写一对元素（i 与 i^k 互补分派，无写冲突）；
// 派发间屏障由录制层逐派发插（compute rw → compute rw）。

@group(0) @binding(4) var<storage, read_write> t_index_list: array<u32>;
@group(0) @binding(5) var<storage, read_write> t_keys: array<f32>;

var<immediate> push: PushSort;
struct PushSort {
    k: u32,  // 当前段长（2 的幂，覆盖对大小）
    j: u32,  // 方向块大小（决定升/降）
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let partner = i ^ push.j;
    if partner <= i {
        return;  // 每对只由较小下标一侧处理（i ^ j < i 的线程跳过）
    }
    // 方向由段长 k 决定（经典 bitonic：k 块前半升后半降），j 是段内配对距离
    let ascending = (i & push.k) == 0u;
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
