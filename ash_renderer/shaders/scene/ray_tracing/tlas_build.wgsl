// TLAS 实例数组组装（compute 单入口，只用 set2 实例表）：每线程处理一条在场行，
// 把 变换 / 槽号 / mask / BLAS 地址 写进本帧槽的实例数组（64B/条，字节镜像
// VkAccelerationStructureInstanceKHR）。宿主随后一次 vkCmdBuildAccelerationStructuresKHR
// 用它构建 TLAS；未就绪槽的地址为 0（VUID-12281 允许，射线自然不命中）。
//
// 接口（set2，组号 0 = 实例表）：
// - b1 模型环（mat4x4f/槽，列主序）
// - b4 在场环（u32 槽号列表，本帧可绘行）
// - b10 BLAS 地址表（vec2<u32>/槽 = u64 的 lo/hi；WGSL 不用 u64，避免 shaderInt64 feature）
// - b11 TLAS 实例数组（read_write，64B/条，本帧全量重写）
// - push（var<immediate>，4B）：count = 在场行数。

@group(0) @binding(1) var<storage, read> models: array<mat4x4f>;
@group(0) @binding(4) var<storage, read> presence: array<u32>;
@group(0) @binding(10) var<storage, read> blas_address: array<vec2<u32>>;
@group(0) @binding(11) var<storage, read_write> tlas_instances: array<TlasInstance>;

// 实例记录 64B：48B 变换（VkTransformMatrixKHR，3×4 行主序）+ custom_index|mask +
// sbt_offset|flags + u64 地址拆两个 u32。宿主 build.rs 的 HOST_LAYOUT_GATE 断言 span=64。
struct TlasInstance {
    transform: array<vec4f, 3>,
    custom_index_mask: u32,
    sbt_offset_flags: u32,
    address_lo: u32,
    address_hi: u32,
}

// VkGeometryInstanceFlagBitsKHR：TRIANGLE_FACING_CULL_DISABLE（GI 双面参与）。
const INSTANCE_TRIANGLE_FACING_CULL_DISABLE: u32 = 1u;

var<immediate> push: PushTlasBuild;
struct PushTlasBuild {
    // dispatch 上界 = 在场行数（CPU 已知的 indirect_count）。
    count: u32,
}

@compute @workgroup_size(64)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= push.count) {
        return;
    }
    let slot = presence[i];
    let m = models[slot];
    var out: TlasInstance;
    // 坑：模型环是列主序 mat4x4f（m[c][r]），VkTransformMatrixKHR 是行主序 3×4——
    // 第 i 行 = (m[0][i], m[1][i], m[2][i], m[3][i])，写成取列要转置，写错几何整体错位。
    out.transform[0] = vec4f(m[0].x, m[1].x, m[2].x, m[3].x);
    out.transform[1] = vec4f(m[0].y, m[1].y, m[2].y, m[3].y);
    out.transform[2] = vec4f(m[0].z, m[1].z, m[2].z, m[3].z);
    // 坑：Packed24_8 的低 24 位 = custom index、高 8 位 = mask；写反 mask 变 0 = 全剔除
    out.custom_index_mask = (slot & 0x00FFFFFFu) | (0xFFu << 24u);
    // 低 24 位 = SBT 偏移（inline ray query 不用，0）；高 8 位 = flags
    out.sbt_offset_flags = INSTANCE_TRIANGLE_FACING_CULL_DISABLE << 24u;
    let addr = blas_address[slot];
    out.address_lo = addr.x;
    out.address_hi = addr.y;
    tlas_instances[i] = out;
}
