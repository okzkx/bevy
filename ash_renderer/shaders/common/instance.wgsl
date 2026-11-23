// 实例表记录族：与 src/render/resident/instance_table.rs 的打包逐字节同序，
// 记录尺寸由 build.rs 的 HOST_LAYOUT_GATE 编译期互证。

// 对象表记录（48B）：基准 mesh 引脚 + 材质索引 + 局部包围盒。
struct ObjectRecord {
    first_index: u32,
    index_count: u32,
    vertex_base: u32,
    material_index: u32,
    aabb_min: vec4f,
    aabb_max: vec4f,
}

// 材质记录（80B）：base_color @0 / PBR 标量 @16..28 / 贴图槽 @28..48（自发光在前，
// 其余四张随后）/ 采样器 @48 / alpha 模式 @52 / 裁切阈值 @56 / 自发光因子 @64。
// 贴图槽缺席为 NO_TEXTURE；base_color 恒在场（缺席走 fallback 白图）。
struct MaterialRecord {
    base_color: vec4f,
    metallic: f32,
    perceptual_roughness: f32,
    reflectance: f32,
    tex_emissive: u32,
    tex_base_color: u32,
    tex_normal: u32,
    tex_metallic_roughness: u32,
    tex_occlusion: u32,
    sampler_index: u32,
    alpha_mode: u32,
    alpha_cutoff: f32,
    // 垫到 64B：让后面的 vec4 落在 16B 对齐位置
    _pad0: u32,
    // 自发光因子（线性域，rgb 有效；w 未用）。用 vec4 占满尾部 16B，与宿主
    // [f32; 4] 逐字节对齐（vec3<f32> 同样按 16 对齐，插字段时容易被推到下一个
    // 16B 槽位，导致跨端偏移错位）
    emissive: vec4f,
}

// 材质 alpha 模式（与宿主 ALPHA_MODE_* 同值域）。
const ALPHA_MODE_OPAQUE: u32 = 0u;
const ALPHA_MODE_MASK: u32 = 1u;
const ALPHA_MODE_BLEND: u32 = 2u;

// 是否进透明层（与宿主 material_is_transparent 同口径）：blend 族恒透明；opaque
// 保留 factor alpha<1 的兜底；mask 行恒走不透明 pass 的片元裁切。
fn material_is_transparent(mat: MaterialRecord) -> bool {
    if (mat.alpha_mode == ALPHA_MODE_BLEND) {
        return true;
    }
    return mat.alpha_mode == ALPHA_MODE_OPAQUE && mat.base_color.a < 1.0;
}

// LOD 变体条目（12B）：绘制引脚三元组；index_count = 0 为未驻留哨兵。
struct LodEntry {
    first_index: u32,
    index_count: u32,
    vertex_base: u32,
}

// VkDrawIndexedIndirectCommand 的字节镜像（20B，字段序 = 规范定义序；
// vertexOffset 在规范里是 int32）。
struct IndirectCommand {
    index_count: u32,
    instance_count: u32,
    first_index: u32,
    vertex_offset: i32,
    first_instance: u32,
}

// 贴图槽缺席哨兵（Rust 侧 NO_TEXTURE_INDEX）。
const NO_TEXTURE: u32 = 0xFFFFFFFFu;
// 每实例 LOD 条目数（instance_table.rs 的 MAX_LOD_LEVELS；LOD 表按它直址）。
const MAX_LOD_LEVELS: u32 = 4u;

// 局部包围盒第 k 个角点（k ∈ 0..8，bit2/bit1/bit0 = x/y/z 取 max）。世界保守盒
// 与投屏都按 8 角点逐个变换，与 Rust conservative_world_aabb 逐位可比。
fn aabb_corner(rec: ObjectRecord, k: u32) -> vec3f {
    return vec3f(
        select(rec.aabb_min.x, rec.aabb_max.x, (k & 4u) != 0u),
        select(rec.aabb_min.y, rec.aabb_max.y, (k & 2u) != 0u),
        select(rec.aabb_min.z, rec.aabb_max.z, (k & 1u) != 0u),
    );
}
