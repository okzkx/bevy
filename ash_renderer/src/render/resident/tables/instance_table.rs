//! GPU 常驻绘制数据：对象表 + 九种帧槽环 + set2 描述符 + 实例槽账。
//!
//! 各表按"谁写、怎么写"分开，原则是不覆盖在飞命令还在读的内容。HOST_VISIBLE 的环
//! 按帧槽各一份，宿主写都在 `wait_for_slot` 的 fence 之后：
//!
//! | 表 | 内存 | 写法 | 消费者 |
//! |---|---|---|---|
//! | 对象表 | DEVICE_LOCAL（[`BufferRole::DeviceStorage`]） | **每槽只写一次**：经 staging 批次写入；换绑/换内容走"新槽 + 旧槽延迟回收"，从不原地覆盖 | VS 读范围/材质索引；compute 读局部包围盒 |
//! | 模型环 | HOST_VISIBLE（Storage）×`MAX_FRAMES_IN_FLIGHT` | **draw_frame 每帧增量写**（只写改动行，新行要写满两个帧槽） | VS 读 model |
//! | 材质环 | 同上 | **draw_frame 每帧全量写**（量级 = 可绘的唯一材质数），内容每帧从资产现读，材质/贴图改动次帧自动生效 | FS 读 base_color/贴图槽 |
//! | 标记环 | 同上 | **剔除 compute 每帧全量写**（dispatch 上界 = 实例槽 high-water）；剔除关闭时 host 填全可见 | build_indirect 读，宿主诊断读 |
//! | 遮挡标记环 | 同上 | **遮挡测试 compute 每帧写**；遮挡关闭时 host 填全可见 | 宿主诊断读（区分视锥/遮挡各剔了多少） |
//! | 在场环 | 同上 | **draw_frame 每帧全量写**可绘行的实例槽号列表 | build_indirect 读 |
//! | 命令环 | DEVICE_LOCAL（[`BufferRole::IndirectCommands`]）×`MAX_FRAMES_IN_FLIGHT` | **compute 每帧全量写**（在场行一行一条 `VkDrawIndexedIndirectCommand`，不可见行 `instanceCount=0`） | `cmd_draw_indexed_indirect`（纯设备侧，宿主不碰） |
//! | LOD 变体表环 | HOST_VISIBLE（Storage）×`MAX_FRAMES_IN_FLIGHT` | **draw_frame 增量写脏行**（槽 → 各档绘制参数 48B/槽，同模型环；未写条目为 0，消费端回退对象记录的基准参数） | build_indirect 读（b7，按选中档取参数） |
//! | LOD 档位环 | 同上 | **选档 compute 每帧写**本帧档位（u32/槽，也是迟滞状态）；LOD 关闭时 host 填 0 = 最细档 | 选档 compute 读写（b8），build_indirect 读 |
//! | 缺口标记环 | 同上 | **cull 清零 + build_indirect 置位**（选中档未驻留的在场槽写 `1<<level` 位图） | 宿主诊断/BRP/驻留管理器合并缺口（fence 后读本帧槽 = 两帧前的输出） |
//!
//! 槽位：实例槽（对象表下标）用 [`SlotAllocator`]，回收走延迟归还（在飞 draw 可能还在读
//! 旧记录，等 GPU 用完才复用）；材质槽只增不回收（量级 = 唯一材质数，有上界）。
//!
//! 描述符：set2 按帧槽各一套，b0..b2（对象表/本帧模型环/本帧材质环）图形与 compute 共读，
//! b3..b9（剔除标记/在场环/命令环/遮挡标记/LOD 变体表/LOD 档位/缺口标记）compute 专属。
//! **描述符建表时写一次永不更新**：buffer 固定容量不重建，用尽报 `Capacity`。pool 自建，
//! 不与 BindlessTextureTable 的 UPDATE_AFTER_BIND pool 混用。Hi-Z 金字塔视图随 swapchain
//! 重建而换，不进 set2，单独的布局在 hiz 模块。
//!
//! 场景绘制没有 push 常量：实例槽号经 indirect 命令的 `firstInstance` 带给 shader。

use std::collections::HashMap;

use ash::{vk, Device};
use bevy::asset::AssetId;
use bevy::log::info;
use bevy::math::Mat4;
use bevy::pbr::StandardMaterial;

use crate::common::bytes::{put_f32s, put_mat4, put_u32, put_u32s, put_vec4};
use crate::common::error::{Result, VulkanError};

use crate::render::resident::slot_allocator::SlotAllocator;
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::contract::{align_up, MemoryContract};
use crate::render::core::device::memory::role::BufferRole;
use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::set_layout::{stages, BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc};
use crate::render::MAX_FRAMES_IN_FLIGHT;

/// 对象表容量（实例槽数）：131072 ≥ 10 万实例压测档，用尽报错不截断。
pub const OBJECT_TABLE_CAPACITY: u32 = 1 << 17;

const SHARED: vk::ShaderStageFlags =
    stages(&[vk::ShaderStageFlags::COMPUTE, vk::ShaderStageFlags::VERTEX, vk::ShaderStageFlags::FRAGMENT]);
const COMPUTE: vk::ShaderStageFlags = vk::ShaderStageFlags::COMPUTE;

/// set2 实例表（binding 含义见模块注释）：b0/b1/b2 图形与 compute 共读，b3..b9 compute 专属
/// （b5 命令环另被 `cmd_draw_indexed_indirect` 读，那是命令参数，不算描述符 stage）。
pub(crate) static INSTANCE_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "instance",
    &[
        BindingDesc::new(0, DescKind::StorageBuffer, SHARED),
        BindingDesc::new(1, DescKind::StorageBuffer, SHARED),
        BindingDesc::new(2, DescKind::StorageBuffer, SHARED),
        BindingDesc::new(3, DescKind::StorageBuffer, COMPUTE),
        BindingDesc::new(4, DescKind::StorageBuffer, COMPUTE),
        BindingDesc::new(5, DescKind::StorageBuffer, COMPUTE),
        BindingDesc::new(6, DescKind::StorageBuffer, COMPUTE),
        BindingDesc::new(7, DescKind::StorageBuffer, COMPUTE),
        BindingDesc::new(8, DescKind::StorageBuffer, COMPUTE),
        BindingDesc::new(9, DescKind::StorageBuffer, COMPUTE),
    ],
);

/// 材质槽容量（唯一材质数上界；只增不回收）。
pub(crate) const MATERIAL_TABLE_CAPACITY: u32 = 1 << 16;

/// 对象记录字节数（与 WGSL `ObjectRecord` 一致）。
pub const OBJECT_RECORD_SIZE: u64 = 48;
/// 模型环步长（mat4x4f），按实例槽索引。
pub const MODEL_STRIDE: u64 = 64;
/// 材质记录字节数（vec4 + PBR 标量 ×3 + 垫 + 贴图槽 ×4 + 采样器 + 垫，对齐 16）。
pub const MATERIAL_RECORD_SIZE: u64 = 64;

/// 贴图槽缺席哨兵：材质没挂该贴图时写本值，shader 跳过采样（base_color 总是有槽，
/// 缺图时用 fallback 白图，不用本哨兵）。
pub const NO_TEXTURE_INDEX: u32 = u32::MAX;
/// 剔除可见标记字节数（u32/实例槽）。
pub const MARK_SIZE: u64 = 4;
/// 遮挡标记字节数（u32/实例槽）。
pub(crate) const OCC_MARK_SIZE: u64 = 4;
/// indirect 命令字节数：`VkDrawIndexedIndirectCommand` 五个 u32（与 WGSL `IndirectCommand`
/// 一致，`vertex_offset` 是 i32 按 u32 位型存）。
pub const INDIRECT_COMMAND_SIZE: u64 = 20;
/// 在场环条目字节数（u32 实例槽号）。
pub(crate) const PRESENCE_ENTRY_SIZE: u64 = 4;
/// 每实例槽的 LOD 档数：0 = 基准 mesh（与对象记录相同），1..3 = 渐粗变体或缺席。
pub const MAX_LOD_LEVELS: u32 = 4;
/// LOD 变体条目字节数：first_index/index_count/vertex_base 三个 u32（同
/// [`MeshSlot`](crate::render::resident::assets::pool::MeshSlot)）；`index_count = 0` 表示该档未驻留，消费端回退更细档。
pub const LOD_ENTRY_SIZE: u64 = 12;
/// LOD 变体表步长（每实例槽 [`MAX_LOD_LEVELS`] 条，按槽号直接寻址）。
pub const LOD_TABLE_STRIDE: u64 = LOD_ENTRY_SIZE * MAX_LOD_LEVELS as u64;
/// LOD 档位环字节数（u32/实例槽，选档 compute 写的本帧档位）。
pub const LOD_LEVEL_SIZE: u64 = 4;
/// 缺口标记环字节数（u32/实例槽位图：bit L = 该槽选中档 L 未驻留）。
pub const GAP_MARK_SIZE: u64 = 4;

/// 对象记录的布局镜像：不参与运行时，只用静态断言核对 [`pack_object_record`] 的偏移。
#[repr(C)]
struct ObjectRecordLayout {
    first_index: u32,
    index_count: u32,
    /// `vkCmdDrawIndexed` 的 vertexOffset（字节偏移 ÷ 32，MeshSlot 语义）。
    vertex_base: u32,
    /// 材质环下标（材质槽，跨帧稳定）。
    material_index: u32,
    /// 局部包围盒（转换器逐点扫出；compute 侧推导世界空间包围盒）。
    aabb_min: [f32; 4],
    aabb_max: [f32; 4],
}

const _: () = {
    assert!(std::mem::offset_of!(ObjectRecordLayout, first_index) == 0);
    assert!(std::mem::offset_of!(ObjectRecordLayout, material_index) == 12);
    assert!(std::mem::offset_of!(ObjectRecordLayout, aabb_min) == 16);
    assert!(std::mem::offset_of!(ObjectRecordLayout, aabb_max) == 32);
    assert!(std::mem::size_of::<ObjectRecordLayout>() as u64 == OBJECT_RECORD_SIZE);
};

/// 材质记录布局镜像（标量语义同 Filament：roughness 存 perceptual 值，clamp 与 α=p²
/// 在 shader 做；reflectance 默认 0.5，介电体 F0 = 0.16·reflectance²）。
#[repr(C)]
struct MaterialRecordLayout {
    base_color: [f32; 4],
    metallic: f32,
    perceptual_roughness: f32,
    reflectance: f32,
    _pad0: u32,
    tex_base_color: u32,
    /// 缺席写 [`NO_TEXTURE_INDEX`] 哨兵，shader 端分支跳过采样。
    tex_normal: u32,
    tex_metallic_roughness: u32,
    tex_occlusion: u32,
    sampler_index: u32,
    _pad: [u32; 3],
}

const _: () = {
    assert!(std::mem::offset_of!(MaterialRecordLayout, base_color) == 0);
    assert!(std::mem::offset_of!(MaterialRecordLayout, metallic) == 16);
    assert!(std::mem::offset_of!(MaterialRecordLayout, perceptual_roughness) == 20);
    assert!(std::mem::offset_of!(MaterialRecordLayout, reflectance) == 24);
    assert!(std::mem::offset_of!(MaterialRecordLayout, tex_base_color) == 32);
    assert!(std::mem::offset_of!(MaterialRecordLayout, tex_normal) == 36);
    assert!(std::mem::offset_of!(MaterialRecordLayout, tex_metallic_roughness) == 40);
    assert!(std::mem::offset_of!(MaterialRecordLayout, tex_occlusion) == 44);
    assert!(std::mem::offset_of!(MaterialRecordLayout, sampler_index) == 48);
    assert!(std::mem::size_of::<MaterialRecordLayout>() as u64 == MATERIAL_RECORD_SIZE);
};

/// 对象记录 → 48B 字节（偏移见 [`ObjectRecordLayout`]；aabb 的 w 保持 0）。
#[must_use]
pub fn pack_object_record(
    first_index: u32,
    index_count: u32,
    vertex_base: u32,
    material_index: u32,
    aabb_min: [f32; 3],
    aabb_max: [f32; 3],
) -> [u8; OBJECT_RECORD_SIZE as usize] {
    let mut out = [0u8; OBJECT_RECORD_SIZE as usize];
    put_u32s(&mut out, 0, &[first_index, index_count, vertex_base, material_index]);
    put_f32s(&mut out, 16, &aabb_min);
    put_f32s(&mut out, 32, &aabb_max);
    out
}

/// 材质记录 → 64B 字节（偏移见 [`MaterialRecordLayout`]）。
#[must_use]
pub fn pack_material_record(
    base_color: [f32; 4],
    metallic: f32,
    perceptual_roughness: f32,
    reflectance: f32,
    tex_slots: [u32; 4],
    sampler_index: u32,
) -> [u8; MATERIAL_RECORD_SIZE as usize] {
    let mut out = [0u8; MATERIAL_RECORD_SIZE as usize];
    put_vec4(&mut out, 0, &base_color);
    put_f32s(&mut out, 16, &[metallic, perceptual_roughness, reflectance]);
    put_u32s(&mut out, 32, &tex_slots);
    put_u32(&mut out, 48, sampler_index);
    out
}

/// LOD 变体条目 → 12B 字节（first_index/index_count/vertex_base，与 WGSL `LodEntry` 同序）。
#[must_use]
pub fn pack_lod_entry(first_index: u32, index_count: u32, vertex_base: u32) -> [u8; LOD_ENTRY_SIZE as usize] {
    let mut out = [0u8; LOD_ENTRY_SIZE as usize];
    put_u32s(&mut out, 0, &[first_index, index_count, vertex_base]);
    out
}

/// 模型矩阵 → 64B 模型环字节（列主序）。
#[must_use]
pub fn pack_model(model: &Mat4) -> [u8; MODEL_STRIDE as usize] {
    let mut out = [0u8; MODEL_STRIDE as usize];
    put_mat4(&mut out, 0, model);
    out
}

/// 局部包围盒经 model 变换后的世界空间保守包围盒：8 个角点全部变换后重新取 min/max。
/// 包住全部角点即保守，不追求最紧。
#[must_use]
pub fn conservative_world_aabb(
    model: &Mat4,
    local_min: [f32; 3],
    local_max: [f32; 3],
) -> ([f32; 3], [f32; 3]) {
    use bevy::math::Vec3;
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for &x in &[local_min[0], local_max[0]] {
        for &y in &[local_min[1], local_max[1]] {
            for &z in &[local_min[2], local_max[2]] {
                let p = model.transform_point3(Vec3::new(x, y, z));
                for k in 0..3 {
                    min[k] = min[k].min(p[k]);
                    max[k] = max[k].max(p[k]);
                }
            }
        }
    }
    (min, max)
}

/// GPU 常驻绘制数据（对象表 + 九种帧槽环，写法见模块注释）+ 实例槽账 + set2 描述符。
#[derive(bevy::prelude::Resource)]
pub struct InstanceTables {
    device: Device,
    object_buffer: GpuBuffer,
    model_rings: Vec<GpuBuffer>,
    material_rings: Vec<GpuBuffer>,
    mark_rings: Vec<GpuBuffer>,
    occ_mark_rings: Vec<GpuBuffer>,
    presence_rings: Vec<GpuBuffer>,
    command_rings: Vec<GpuBuffer>,
    lod_table_rings: Vec<GpuBuffer>,
    lod_level_rings: Vec<GpuBuffer>,
    gap_mark_rings: Vec<GpuBuffer>,
    /// 实例槽。删行/换绑换下的旧槽延迟归还：在飞 draw 可能还在读，等全部帧槽 fence 后才复用。
    object_slots: SlotAllocator,
    /// 材质槽（只增不回收）。
    material_slots: HashMap<AssetId<StandardMaterial>, u32>,
    used_materials: u32,
    set2_layout: vk::DescriptorSetLayout,
    /// set2 每帧槽一套（b1..b9 指向各自帧槽的环），建表时写一次。
    frame_sets: Vec<vk::DescriptorSet>,
    pool: vk::DescriptorPool,
    /// 延迟回收累计归还的实例槽数。
    slots_retired_total: u64,
}

impl InstanceTables {
    /// 建表：对象表（CONCURRENT 双族：transfer 写、graphics 读）+ 九种环各
    /// `MAX_FRAMES_IN_FLIGHT` 份 → set2 layout/pool/每帧槽 set → 描述符写一次 → 档位环
    /// 零填充。任一步失败，已建对象全部回收。
    ///
    /// # Errors
    /// buffer/描述符创建失败、档位环写入失败。
    pub fn new(
        device: &Device,
        contract: &MemoryContract,
        sharing_families: &[u32],
    ) -> Result<Self> {
        const MIB: u64 = 1024 * 1024;
        // 每实例槽 `stride` 字节的整表大小，向上对齐到 1MiB
        let per_object = |stride: u64| align_up(u64::from(OBJECT_TABLE_CAPACITY) * stride, MIB);
        let object_bytes = per_object(OBJECT_RECORD_SIZE);
        let model_bytes = per_object(MODEL_STRIDE);
        let material_bytes = align_up(u64::from(MATERIAL_TABLE_CAPACITY) * MATERIAL_RECORD_SIZE, MIB);
        let mark_bytes = per_object(MARK_SIZE);
        let occ_mark_bytes = per_object(OCC_MARK_SIZE);
        let presence_bytes = per_object(PRESENCE_ENTRY_SIZE);
        let command_bytes = per_object(INDIRECT_COMMAND_SIZE);
        let lod_table_bytes = per_object(LOD_TABLE_STRIDE);
        let lod_level_bytes = per_object(LOD_LEVEL_SIZE);
        let gap_mark_bytes = per_object(GAP_MARK_SIZE);
        // 与顶点/索引池同样 CONCURRENT 共享，不做所有权转移；跨队列可见性由图形提交等
        // 上传票据保证
        let object_buffer = GpuBuffer::create_with_families(
            device,
            contract,
            object_bytes,
            BufferRole::DeviceStorage,
            sharing_families,
        )?;
        // 失败即 `?` 早退：已建 buffer 还没进 Self，随局部变量 Drop
        let ring = |bytes: u64, role: BufferRole| -> Result<Vec<GpuBuffer>> {
            (0..MAX_FRAMES_IN_FLIGHT)
                .map(|_| GpuBuffer::create(device, contract, bytes, role))
                .collect::<Result<_>>()
        };
        let model_rings = ring(model_bytes, BufferRole::Storage)?;
        let material_rings = ring(material_bytes, BufferRole::Storage)?;
        let mark_rings = ring(mark_bytes, BufferRole::Storage)?;
        let occ_mark_rings = ring(occ_mark_bytes, BufferRole::Storage)?;
        let presence_rings = ring(presence_bytes, BufferRole::Storage)?;
        let command_rings = ring(command_bytes, BufferRole::IndirectCommands)?;
        let lod_table_rings = ring(lod_table_bytes, BufferRole::Storage)?;
        let lod_level_rings = ring(lod_level_bytes, BufferRole::Storage)?;
        let gap_mark_rings = ring(gap_mark_bytes, BufferRole::Storage)?;
        let (set2_layout, frame_sets, pool) = unsafe {
            Self::create_descriptor_objects(
                device,
                &object_buffer,
                &model_rings,
                &material_rings,
                &mark_rings,
                &occ_mark_rings,
                &presence_rings,
                &command_rings,
                &lod_table_rings,
                &lod_level_rings,
                &gap_mark_rings,
            )
        }?;
        // 先组装 Self 再写档位环：写失败时 Self 的 Drop 回收 pool/layout
        let mut tables = Self {
            device: device.clone(),
            object_buffer,
            model_rings,
            material_rings,
            mark_rings,
            occ_mark_rings,
            presence_rings,
            command_rings,
            lod_table_rings,
            lod_level_rings,
            gap_mark_rings,
            object_slots: SlotAllocator::new(OBJECT_TABLE_CAPACITY),
            material_slots: HashMap::new(),
            used_materials: 0,
            set2_layout,
            frame_sets,
            pool,
            slots_retired_total: 0,
        };
        // 档位环零填充：选档的迟滞要读上一帧档位，未派发前须是 0（最细档），否则首帧读到
        // 垃圾档位。变体表环不填——条目 0 本来就表示"未驻留，回退基准参数"。
        let zeros = vec![0u8; lod_level_bytes as usize];
        for ring in &mut tables.lod_level_rings {
            ring.write(0, &zeros)?;
        }
        info!(
            "GPU 常驻绘制数据就绪:对象表 {OBJECT_TABLE_CAPACITY} 槽({object_bytes}B,DEVICE_LOCAL,记录 48B/槽写一次)\
             + 模型环 {MODEL_STRIDE}B ×{MAX_FRAMES_IN_FLIGHT}({model_bytes}B/槽)\
             + 材质环 {MATERIAL_RECORD_SIZE}B ×{MAX_FRAMES_IN_FLIGHT}({material_bytes}B/槽,\
             容量 {MATERIAL_TABLE_CAPACITY} 材质槽 bump-only)\
             + 标记环 {MARK_SIZE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({mark_bytes}B/槽,剔除可见标记)\
             + 遮挡标记环 {OCC_MARK_SIZE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({occ_mark_bytes}B/槽,遮挡独立标记)\
             + 在场环 {PRESENCE_ENTRY_SIZE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({presence_bytes}B/槽,可绘行槽号列表)\
             + 命令环 {INDIRECT_COMMAND_SIZE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({command_bytes}B/槽,\
             indirect 命令,DEVICE_LOCAL)\
             + LOD 变体表环 {LOD_TABLE_STRIDE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({lod_table_bytes}B/槽,\
             槽→各档绘制引脚,{MAX_LOD_LEVELS} 档)\
             + LOD 档位环 {LOD_LEVEL_SIZE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({lod_level_bytes}B/槽,选档状态,零填充)\
             + 缺口标记环 {GAP_MARK_SIZE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({gap_mark_bytes}B/槽,逐槽缺口位图,cull 清零+build_indirect 置位);\
             set2 三条只读 storage + compute 专属 b3/b4/b5/b6/b7/b8/b9 ×{MAX_FRAMES_IN_FLIGHT} 帧,描述符 init 一次写入"
        );
        Ok(tables)
    }

    /// set2 布局 + pool + 每帧槽 set + 描述符写一次。布局/池创建失败时回收已建的部分；
    /// buffer 由调用方的局部变量 Drop。
    ///
    /// # Safety
    /// buffer 句柄须刚创建成功且寿命覆盖本函数。
    #[expect(
        clippy::too_many_arguments,
        reason = "buffer 逐份传入（每份一个 binding），便于与 set/binding 表逐项对照"
    )]
    unsafe fn create_descriptor_objects(
        device: &Device,
        object_buffer: &GpuBuffer,
        model_rings: &[GpuBuffer],
        material_rings: &[GpuBuffer],
        mark_rings: &[GpuBuffer],
        occ_mark_rings: &[GpuBuffer],
        presence_rings: &[GpuBuffer],
        command_rings: &[GpuBuffer],
        lod_table_rings: &[GpuBuffer],
        lod_level_rings: &[GpuBuffer],
        gap_mark_rings: &[GpuBuffer],
    ) -> Result<(vk::DescriptorSetLayout, Vec<vk::DescriptorSet>, vk::DescriptorPool)> {
        unsafe {
            let set2_layout = INSTANCE_SET_DESC.create(device)?;
            let instance_pool_sizes = INSTANCE_SET_DESC.pool_sizes_vec(MAX_FRAMES_IN_FLIGHT as u32);
            let pool = device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(MAX_FRAMES_IN_FLIGHT as u32)
                        .pool_sizes(&instance_pool_sizes),
                    None,
                )
                .inspect_err(|_| device.destroy_descriptor_set_layout(set2_layout, None))?;
            let sets = device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(pool)
                        .set_layouts(&[set2_layout; MAX_FRAMES_IN_FLIGHT]),
                )
                .inspect_err(|_| {
                    device.destroy_descriptor_pool(pool, None);
                    device.destroy_descriptor_set_layout(set2_layout, None);
                })?;
            // b0 对象表，b1..b9 各帧槽自己的环，全部整段绑定；写满才提交
            for (frame, set) in sets.iter().enumerate() {
                let mut writer = DescriptorWriter::new(&INSTANCE_SET_DESC, *set);
                writer.buffer(0, object_buffer.buffer(), 0, OBJECT_TABLE_CAPACITY as u64 * OBJECT_RECORD_SIZE);
                for (binding, buffer, range) in [
                    (1u32, &model_rings[frame], OBJECT_TABLE_CAPACITY as u64 * MODEL_STRIDE),
                    (2, &material_rings[frame], MATERIAL_TABLE_CAPACITY as u64 * MATERIAL_RECORD_SIZE),
                    (3, &mark_rings[frame], OBJECT_TABLE_CAPACITY as u64 * MARK_SIZE),
                    (4, &presence_rings[frame], OBJECT_TABLE_CAPACITY as u64 * PRESENCE_ENTRY_SIZE),
                    (5, &command_rings[frame], OBJECT_TABLE_CAPACITY as u64 * INDIRECT_COMMAND_SIZE),
                    (6, &occ_mark_rings[frame], OBJECT_TABLE_CAPACITY as u64 * OCC_MARK_SIZE),
                    (7, &lod_table_rings[frame], OBJECT_TABLE_CAPACITY as u64 * LOD_TABLE_STRIDE),
                    (8, &lod_level_rings[frame], OBJECT_TABLE_CAPACITY as u64 * LOD_LEVEL_SIZE),
                    (9, &gap_mark_rings[frame], OBJECT_TABLE_CAPACITY as u64 * GAP_MARK_SIZE),
                ] {
                    writer.buffer(binding, buffer.buffer(), 0, range);
                }
                writer.submit(device)?;
            }
            Ok((set2_layout, sets, pool))
        }
    }

    /// set2 布局（登记给管线组装；集与布局同源）。
    #[must_use]
    pub fn set_layout(&self) -> SetLayout {
        SetLayout::new(&INSTANCE_SET_DESC, self.set2_layout)
    }

    /// 第 `frame` 帧槽的 set2。
    #[must_use]
    pub fn frame_set2(&self, frame: usize) -> DescSet {
        DescSet::new(&INSTANCE_SET_DESC, self.frame_sets[frame])
    }

    /// 对象表 buffer 句柄（staging 批次拷贝目标）。
    #[must_use]
    pub fn object_buffer(&self) -> vk::Buffer {
        self.object_buffer.buffer()
    }

    /// 第 `frame` 帧槽的模型环（draw_frame 增量写）。
    #[must_use]
    pub fn model_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.model_rings[frame]
    }

    /// 第 `frame` 帧槽的材质环（draw_frame 全量写）。
    #[must_use]
    pub fn material_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.material_rings[frame]
    }

    /// 第 `frame` 帧槽的剔除标记环（剔除关闭时 host 填全可见；宿主诊断读）。
    #[must_use]
    pub fn mark_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.mark_rings[frame]
    }

    /// 第 `frame` 帧槽的遮挡标记环（遮挡关闭时 host 填全可见；宿主诊断读）。
    #[must_use]
    pub fn occ_mark_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.occ_mark_rings[frame]
    }

    /// 第 `frame` 帧槽的在场环（draw_frame 全量写可绘行槽号列表）。
    #[must_use]
    pub fn presence_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.presence_rings[frame]
    }

    /// 第 `frame` 帧槽的 indirect 命令环句柄（纯设备侧，宿主不读写）。
    #[must_use]
    pub fn indirect_buffer(&self, frame: usize) -> vk::Buffer {
        self.command_rings[frame].buffer()
    }

    /// 第 `frame` 帧槽的 LOD 变体表环（draw_frame 增量写脏行）。
    #[must_use]
    pub fn lod_table_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.lod_table_rings[frame]
    }

    /// 第 `frame` 帧槽的 LOD 档位环（LOD 关闭时 host 填 0；宿主诊断读）。
    #[must_use]
    pub fn lod_level_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.lod_level_rings[frame]
    }

    /// 第 `frame` 帧槽的缺口标记环（宿主读到的是两帧前 compute 的输出）。
    pub fn gap_mark_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.gap_mark_rings[frame]
    }

    /// 分配一个实例槽。
    ///
    /// # Errors
    /// 实例槽容量耗尽（报错，不越界）。
    pub fn alloc_object_slot(&mut self) -> Result<u32> {
        self.object_slots.alloc().ok_or_else(|| {
            VulkanError::Capacity(format!(
                "对象表容量耗尽:已用 {}/{OBJECT_TABLE_CAPACITY}——超过压测设定的实例规模,\
                 显式报错,不静默截断",
                self.object_slots.high_water()
            ))
        })
    }

    /// 交回一个实例槽（删行/换绑换下）：先进待回收账，在飞 draw 可能还在读旧记录，
    /// [`Self::drain_graveyard`] 之后才可复用。
    pub fn retire_object_slot(&mut self, slot: u32) {
        self.object_slots.retire(slot);
    }

    /// 等 GPU 用完（`wait_draws`）后把待回收的实例槽归还复用。由 flush_uploads 调用，
    /// 与网格/贴图的回收共用同一次等待；没有待回收槽时不等待。
    ///
    /// # Errors
    /// 等待失败（待回收账保留，下帧重试）。
    pub fn drain_graveyard(&mut self, wait_draws: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        if self.object_slots.retired_count() == 0 {
            return Ok(());
        }
        wait_draws()?;
        let count = self.object_slots.drain_retired();
        self.slots_retired_total += count as u64;
        info!("对象表待回收队列排空:归还 {count} 个实例槽(图形最后使用已过,可复用)");
        Ok(())
    }

    /// 取材质槽，没有就分配。槽跨帧稳定：材质内容变化由材质环每帧重写体现，
    /// 对象记录里的材质索引不用改。
    ///
    /// # Errors
    /// 材质槽容量耗尽。
    pub fn ensure_material_slot(&mut self, id: AssetId<StandardMaterial>) -> Result<u32> {
        if let Some(&slot) = self.material_slots.get(&id) {
            return Ok(slot);
        }
        if self.used_materials >= MATERIAL_TABLE_CAPACITY {
            return Err(VulkanError::Capacity(format!(
                "材质槽容量耗尽:{}/{MATERIAL_TABLE_CAPACITY}——唯一材质数超出上界,显式报错不截断",
                self.used_materials
            )));
        }
        let slot = self.used_materials;
        self.used_materials += 1;
        self.material_slots.insert(id, slot);
        Ok(slot)
    }

    /// 只查不分配；None = 该材质还没有槽，调用方跳过。
    #[must_use]
    pub fn material_slot(&self, id: AssetId<StandardMaterial>) -> Option<u32> {
        self.material_slots.get(&id).copied()
    }

    /// 用过的最大实例槽号 + 1（含待回收和空闲的槽；复用不增长，同 MeshPool 的 used）。
    #[must_use]
    pub fn used_object_slots(&self) -> u32 {
        self.object_slots.high_water()
    }

    #[must_use]
    pub fn capacity_objects(&self) -> u32 {
        self.object_slots.capacity()
    }

    #[must_use]
    pub fn used_material_slots(&self) -> u32 {
        self.used_materials
    }

    #[must_use]
    pub fn capacity_materials(&self) -> u32 {
        MATERIAL_TABLE_CAPACITY
    }

    /// 待回收的实例槽数。
    #[must_use]
    pub fn graveyard_count(&self) -> usize {
        self.object_slots.retired_count()
    }

    /// 累计归还的实例槽数。
    #[must_use]
    pub fn slots_retired_total(&self) -> u64 {
        self.slots_retired_total
    }
}

impl Drop for InstanceTables {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown_vulkan 已先 device_wait_idle。pool 连带释放 set；
        // buffer 随字段 Drop。
        unsafe {
            self.device.destroy_descriptor_pool(self.pool, None);
            self.device.destroy_descriptor_set_layout(self.set2_layout, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        conservative_world_aabb, pack_lod_entry, pack_material_record, pack_model,
        pack_object_record, LOD_ENTRY_SIZE, MATERIAL_RECORD_SIZE, OBJECT_RECORD_SIZE,
    };
    use bevy::math::{Mat4, Vec3};

    /// 非均匀缩放 + 旋转下包围盒保守包住几何:任取盒内样本点,变换后必在
    /// 保守盒内。
    #[test]
    fn conservative_transform_bounds_sampled_points() {
        // 单位盒 × 非均匀缩放 (2, 0.5, 3) × 绕 Y 旋转 30°
        let model = Mat4::from_rotation_y(std::f32::consts::FRAC_PI_6)
            * Mat4::from_scale(Vec3::new(2.0, 0.5, 3.0));
        let (wmin, wmax) = conservative_world_aabb(&model, [-1.0; 3], [1.0; 3]);
        // 盒内采样点(角点/棱中点/内部)变换后必须落在保守盒内
        let samples = [
            Vec3::new(-1.0, -1.0, -1.0),
            Vec3::new(1.0, 1.0, 1.0),
            Vec3::new(0.3, -0.7, 0.9),
            Vec3::new(-0.9, 0.5, -0.2),
        ];
        for s in samples {
            let p = model.transform_point3(s);
            for k in 0..3 {
                assert!(p[k] >= wmin[k] - 1e-5, "样本 {s:?} 分量 {k} 低于保守盒下界");
                assert!(p[k] <= wmax[k] + 1e-5, "样本 {s:?} 分量 {k} 高于保守盒上界");
            }
        }
        // 全部 8 角点都在内是"保守"的直接含义
        for &x in &[-1.0, 1.0] {
            for &y in &[-1.0, 1.0] {
                for &z in &[-1.0, 1.0] {
                    let p = model.transform_point3(Vec3::new(x, y, z));
                    for k in 0..3 {
                        assert!(p[k] >= wmin[k] - 1e-5 && p[k] <= wmax[k] + 1e-5);
                    }
                }
            }
        }
    }

    /// 恒等变换下保守盒 = 局部盒本身(不放大不偏移)。
    #[test]
    fn identity_transform_is_exact() {
        let (wmin, wmax) = conservative_world_aabb(&Mat4::IDENTITY, [-1.0, 0.0, -2.0], [1.0, 3.0, 2.0]);
        assert_eq!(wmin, [-1.0, 0.0, -2.0]);
        assert_eq!(wmax, [1.0, 3.0, 2.0]);
    }

    /// 对象记录 48B 打包与布局断言的一致性抽查。
    #[test]
    fn pack_object_record_layout() {
        let bytes = pack_object_record(7, 99, 11, 3, [0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        assert_eq!(bytes.len(), OBJECT_RECORD_SIZE as usize);
        let read_u32 = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().expect("4B"));
        assert_eq!(read_u32(0), 7);
        assert_eq!(read_u32(4), 99);
        assert_eq!(read_u32(8), 11);
        assert_eq!(read_u32(12), 3);
        // aabb_min 的 w 垫保持 0
        let read_f32 = |o: usize| f32::from_le_bytes(bytes[o..o + 4].try_into().expect("4B"));
        assert_eq!(read_f32(28), 0.0, "aabb_min 的 w 垫必须保持 0");
    }

    /// 模型环字节的往返:pack 后按列主序还原等于原矩阵。
    #[test]
    fn pack_model_roundtrip() {
        let m = Mat4::from_translation(Vec3::new(1.0, 2.0, 3.0));
        let bytes = pack_model(&m);
        assert_eq!(bytes.len(), 64);
        let mut cols = [0f32; 16];
        for (i, c) in cols.iter_mut().enumerate() {
            *c = f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().expect("4B"));
        }
        assert_eq!(m.to_cols_array(), cols);
    }

    /// 材质记录 64B 布局：PBR 三标量 + 四贴图槽 + 采样器偏移
    /// 逐项互证，缺席哨兵按位写入。
    #[test]
    fn pack_material_record_layout() {
        let bytes = pack_material_record(
            [0.1, 0.2, 0.3, 1.0],
            0.75,
            0.5,
            0.5,
            [7, u32::MAX, 9, u32::MAX],
            3,
        );
        assert_eq!(bytes.len(), MATERIAL_RECORD_SIZE as usize);
        let read_f32 = |o: usize| f32::from_le_bytes(bytes[o..o + 4].try_into().expect("4B"));
        let read_u32 = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().expect("4B"));
        assert_eq!(read_f32(4), 0.2);
        assert_eq!(read_f32(16), 0.75);
        assert_eq!(read_f32(20), 0.5);
        assert_eq!(read_f32(24), 0.5, "reflectance 记录原值（clamp 换算在 shader 端）");
        assert_eq!(read_u32(32), 7);
        assert_eq!(read_u32(36), u32::MAX, "normal 缺席哨兵");
        assert_eq!(read_u32(40), 9);
        assert_eq!(read_u32(44), u32::MAX, "occlusion 缺席哨兵");
        assert_eq!(read_u32(48), 3);
        // 垫区保持 0
        assert_eq!(read_u32(28), 0);
        assert_eq!(read_u32(52), 0);
    }

    /// LOD 变体条目 12B 布局：first_index/index_count/vertex_base 同序
    /// 互证（与 WGSL `LodEntry` 同源；0 哨兵 = index_count 为 0 的条目）。
    #[test]
    fn pack_lod_entry_layout() {
        let bytes = pack_lod_entry(7, 99, 11);
        assert_eq!(bytes.len(), LOD_ENTRY_SIZE as usize);
        let read_u32 = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().expect("4B"));
        assert_eq!(read_u32(0), 7);
        assert_eq!(read_u32(4), 99);
        assert_eq!(read_u32(8), 11);
        // 哨兵条目 = index_count 0（其余字段语义无关）
        let sentinel = pack_lod_entry(0, 0, 0);
        assert_eq!(u32::from_le_bytes(sentinel[4..8].try_into().expect("4B")), 0);
    }
}
