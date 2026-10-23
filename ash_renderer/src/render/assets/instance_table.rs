//! GPU 常驻绘制数据：对象表 + 九种帧槽环 + set2 描述符 + 实例槽账。
//!
//! 各表按"写形状"分家，各自 hazard 闭合（本模块的核心设计，与责任边界
//! 契约同源——"覆盖仍被在飞命令读取的内容"一律不走）：
//!
//! | 表 | 内存 | 写形状 | 消费者 |
//! |---|---|---|---|
//! | 对象表 | DEVICE_LOCAL（[`BufferRole::DeviceStorage`]） | **每槽写一次**：分配经 staging 批次写入，更新（换绑/换内容）走"新槽 + 旧槽停尸房"，从不原地覆盖 | VS 读范围/材质索引；compute 读局部包围盒 |
//! | 模型环 | HOST_VISIBLE（Storage 角色）×`MAX_FRAMES_IN_FLIGHT` | **draw_frame 每帧增量重写**（只写改动行；新槽两帧写满两个帧槽），复用安全 = `wait_for_slot` fence（与帧 UBO 同一纪律） | VS 读 model |
//! | 材质环 | 同上 | **draw_frame 每帧全量重写**（量级 = 可绘唯一材质数），内容每帧从资产容器现读——材质/贴图内容变化自动次帧生效，无需任何失效通道 | FS 读 base_color/贴图槽 |
//! | 标记环 | 同上 | **compute 每帧全量重写**（dispatch 上界 = 实例槽 bump bound）；A/B 关闭时 host 填全可见——同一 fence 纪律 | compute 写（cull.wgsl），build_indirect 读，宿主诊断读数 |
//! | 遮挡标记环 | 同上 | **遮挡测试 compute 每帧重写**；fallback 时 host 填全可见——同一 fence 纪律 | 遮挡独立标记位，宿主诊断读数（A/B 归因） |
//! | 在场环 | 同上 | **draw_frame 每帧全量重写**（可绘行的实例槽号列表，量级 = 可绘行数）——DrawList 槽位枚举的 GPU 形态 | compute 读（build_indirect.wgsl） |
//! | 命令环 | DEVICE_LOCAL（[`BufferRole::IndirectCommands`]）×`MAX_FRAMES_IN_FLIGHT` | **compute 每帧全量重写**（在场槽逐行一条 `VkDrawIndexedIndirectCommand`，不可见 `instanceCount=0`） | `cmd_draw_indexed_indirect`（DRAW_INDIRECT 阶段读；纯设备侧，宿主零参与） |
//! | LOD 变体表环 | HOST_VISIBLE（Storage 角色）×`MAX_FRAMES_IN_FLIGHT` | **draw_frame 对脏行增量重写**（槽 → 各档绘制引脚 48B/槽，模型环同款两帧奇偶纪律；未写条目 = 0 哨兵 → 消费端回退对象记录基准引脚） | compute 读（build_indirect.wgsl b7，按选档取引脚） |
//! | LOD 档位环 | 同上 | **选档 compute 每帧重写**（本帧档位 u32/槽，迟滞状态载体）；LOD 关闭时 host 填 0 = 最细档 | 选档 compute 读写（b8），build_indirect 读 |
//! | 缺口标记环 | 同上 | **cull 清零（dispatch 覆盖全部槽）+ build_indirect 置位**（选中档未驻留的在场槽写 `1<<level` 位图）——标记环同款两帧奇偶纪律 | 宿主诊断读数/BRP census/驻留管理器缺口合并（fence 纪律内读本帧槽 = 两帧前输出） |
//!
//! 槽位纪律：实例槽（对象表下标）first-fit free list + bump，回收经停尸房
//! （等"图形最后使用"——在飞 draw 可能还在读旧记录，排空后才能复用）；
//! 材质槽 bump-only 不回收（量级 = 唯一材质数，泄漏有界，材质 Removed 无
//! 驻留内容不进回收账）。
//!
//! 描述符：set2 = 三条图形+compute 共读的只读 storage binding（b0 对象表 /
//! b1 本帧模型环 / b2 本帧材质环）+ 七条 compute 专属 storage binding（b3 剔除
//! 标记 read_write / b4 在场环 / b5 indirect 命令环 / b6 遮挡标记 read_write /
//! b7 LOD 变体表 / b8 LOD 档位环 read_write / b9 缺口标记 read_write），按帧槽
//! 各一套，**描述符 init 一次写入永不更新**（buffer 固定容量不重建——槽位用尽报
//! `Capacity` 出局，与 TABLE_CAPACITY 同纪律）。自建 pool 不占 BindlessTables
//! 的 UAB pool——生命周期与消费面都不同，分开建。layout 的 b0/b1/b2 声明
//! `VERTEX|FRAGMENT|COMPUTE` 超集，b3..b9 收窄到 COMPUTE（b5 的另一读者是
//! cmd_draw_indexed_indirect，走命令参数不进描述符 stage）。Hi-Z 金字塔采样视图
//! 不进 set2：它随 swapchain 重建整体换 image，"init 一次写入"纪律不为它开口，
//! 独立小布局收在 hiz 模块。
//!
//! 场景绘制无 push 常量：实例槽号经 indirect 命令的 `firstInstance` 携带，shader
//! 按实例索引读表——逐对象绘制参数不走 CPU 推送。

use std::collections::HashMap;

use ash::{vk, Device};
use bevy::asset::AssetId;
use bevy::log::info;
use bevy::math::Mat4;
use bevy::pbr::StandardMaterial;

use crate::common::bytes::{put_f32s, put_mat4, put_u32, put_u32s, put_vec4};
use crate::common::error::{Result, VulkanError};

use crate::render::resources::{align_up, BufferRole, GpuBuffer, MemoryContract};
use crate::render::MAX_FRAMES_IN_FLIGHT;

/// 对象表容量（实例槽数）。覆盖 10 万实例压测档（131072 ≥ 100000）；
/// 用尽报错出局不静默截断（TABLE_CAPACITY 同纪律）。48B/记录。
pub const OBJECT_TABLE_CAPACITY: u32 = 1 << 17;

/// 材质槽容量（唯一材质数上界；bump-only 不回收）。
pub(crate) const MATERIAL_TABLE_CAPACITY: u32 = 1 << 16;

/// 对象记录字节数（storage 布局，与 WGSL `ObjectRecord` 互证）。
pub const OBJECT_RECORD_SIZE: u64 = 48;
/// 模型环步长（mat4x4f）。按实例槽索引，与对象表同一索引空间。
pub const MODEL_STRIDE: u64 = 64;
/// 材质记录字节数（vec4 + PBR 标量 ×3 + 垫 + 贴图槽 ×4 + 采样器 + 垫，对齐 16）。
pub const MATERIAL_RECORD_SIZE: u64 = 64;

/// 贴图槽缺席哨兵：材质未挂该贴图时槽位字段写本值，shader 端
/// 分支跳过采样（base_color 恒在场走 fallback 白图，不适用本哨兵）。
pub const NO_TEXTURE_INDEX: u32 = u32::MAX;
/// 剔除可见标记字节数（u32/实例槽）。
pub const MARK_SIZE: u64 = 4;
/// 遮挡标记字节数（u32/实例槽——遮挡独立标记位，A/B 归因读数）。
pub(crate) const OCC_MARK_SIZE: u64 = 4;
/// indirect 命令字节数（`VkDrawIndexedIndirectCommand`：indexCount/instanceCount/
/// firstIndex/vertexOffset/firstInstance 五个 u32——与 WGSL `IndirectCommand`
/// 布局互证，`vertex_offset` 为 i32 位型按 u32 存储）。
pub const INDIRECT_COMMAND_SIZE: u64 = 20;
/// 在场环条目字节数（u32 实例槽号/可绘行）。
pub(crate) const PRESENCE_ENTRY_SIZE: u64 = 4;
/// LOD 档位上限：每实例槽的 LOD 变体条目数。level 0 = 基准 mesh
///（与对象记录同引脚），1..3 = 渐粗变体（或缺席哨兵）。
pub const MAX_LOD_LEVELS: u32 = 4;
/// LOD 变体条目字节数（3×u32：first_index/index_count/vertex_base——与
/// [`MeshSlot`](super::pool::MeshSlot) 引脚同源；`index_count = 0` = 该档
/// 未驻留（哨兵，消费端向更细档回退））。
pub const LOD_ENTRY_SIZE: u64 = 12;
/// LOD 变体表步长（每实例槽 [`MAX_LOD_LEVELS`] 条——按槽直址的
/// "槽 → 各档绘制引脚"表，host 每帧槽环增量写）。
pub const LOD_TABLE_STRIDE: u64 = LOD_ENTRY_SIZE * MAX_LOD_LEVELS as u64;
/// LOD 档位环字节数（u32/实例槽——选档 compute 写的本帧档位，迟滞状态
/// 的载体；host 关闭档填充 0 = 最细档）。
pub const LOD_LEVEL_SIZE: u64 = 4;
/// 缺口标记环字节数（u32/实例槽——逐槽位图：bit L = 该槽选中档 L
/// 未驻留。cull 清零（dispatch 覆盖全部槽）+ build_indirect 置位（在场行
/// 查引脚处），宿主按帧奇偶纪律读两帧前快照——GPU 一手缺口证据的读回通道）。
pub const GAP_MARK_SIZE: u64 = 4;

/// 对象记录的 Rust 布局镜像（[`pack_object_record`] 手工排布的对账表：
/// 不参与运行时，静态断言钉 WGSL/CPU 两方）。
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

/// 材质记录布局镜像（标量域语义照 Filament——roughness 记 perceptual 域，
/// clamp 与 α=p² 换算在 shader 端做；reflectance 默认 0.5，介电体
/// F0 = 0.16·reflectance²）。
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

/// 对象记录 → 48B storage 字节（偏移按 [`ObjectRecordLayout`] 静态断言表手工
/// 排布；aabb 的 w 垫保持 0）。
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

/// 材质记录 → 64B storage 字节（PBR 三标量 + 四贴图槽，偏移按
/// [`MaterialRecordLayout`] 静态断言表手工排布）。
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

/// LOD 变体条目 → 12B storage 字节（first_index/index_count/vertex_base，
/// 与 WGSL `LodEntry` 同序互证）。
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

/// 局部包围盒经 model 的世界空间保守包围盒：8 角点全变换后重收紧。
/// 保守性由构造保证（全部角点在内），不追求紧致最优；非均匀缩放 + 旋转
/// 正确性由本模块单测钉住。
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

/// GPU 常驻绘制数据（对象表 + 九种帧槽环）+ 实例槽账本 + set2 描述符。
#[derive(bevy::prelude::Resource)]
pub struct InstanceTables {
    device: Device,
    /// 对象表本体（DEVICE_LOCAL；记录按槽写一次，更新走新槽 + 停尸房）。
    object_buffer: GpuBuffer,
    /// 模型环 ×`MAX_FRAMES_IN_FLIGHT`（HOST_VISIBLE，draw_frame 增量重写）。
    model_rings: Vec<GpuBuffer>,
    /// 材质环 ×`MAX_FRAMES_IN_FLIGHT`（HOST_VISIBLE，draw_frame 全量重写）。
    material_rings: Vec<GpuBuffer>,
    /// 剔除标记环 ×`MAX_FRAMES_IN_FLIGHT`（HOST_VISIBLE，compute 每帧重写 /
    /// A/B 关闭时 host 填全可见；宿主诊断直读）。见 cull 模块文档。
    mark_rings: Vec<GpuBuffer>,
    /// 遮挡标记环 ×`MAX_FRAMES_IN_FLIGHT`（HOST_VISIBLE：遮挡测试
    /// compute 写 / fallback 时 host 填全可见 / 宿主诊断直读——A/B 归因读数口）。
    occ_mark_rings: Vec<GpuBuffer>,
    /// 在场环 ×`MAX_FRAMES_IN_FLIGHT`（HOST_VISIBLE，draw_frame 每帧全量重写
    /// 可绘行槽号列表——DrawList 槽位枚举进 GPU 的通道）。
    presence_rings: Vec<GpuBuffer>,
    /// indirect 命令环 ×`MAX_FRAMES_IN_FLIGHT`（DEVICE_LOCAL，build_indirect
    /// 每帧重写、`cmd_draw_indexed_indirect` 消费；纯设备侧）。
    command_rings: Vec<GpuBuffer>,
    /// LOD 变体表环 ×`MAX_FRAMES_IN_FLIGHT`（HOST_VISIBLE：draw_frame
    /// 对脏行增量重写"槽 → 各档绘制引脚"——模型环同款增量纪律，未写槽的
    /// 条目 = 0 哨兵，消费端回退对象记录基准引脚）。
    lod_table_rings: Vec<GpuBuffer>,
    /// LOD 档位环 ×`MAX_FRAMES_IN_FLIGHT`（HOST_VISIBLE：选档 compute
    /// 写本帧档位 / LOD 关闭时 host 填 0 / 宿主诊断直读；迟滞状态随环轮转）。
    lod_level_rings: Vec<GpuBuffer>,
    /// 缺口标记环 ×`MAX_FRAMES_IN_FLIGHT`（HOST_VISIBLE：cull 清零 +
    /// build_indirect 置位 / 宿主诊断读两帧前快照）。
    gap_mark_rings: Vec<GpuBuffer>,
    capacity_objects: u32,
    capacity_materials: u32,
    /// 实例槽：bump 游标 + free list（排空点归还后复用）。
    used_objects: u32,
    free_objects: Vec<u32>,
    /// 回收停尸房：行删除/换绑换下的旧槽，在飞 draw 可能还在读——排空
    /// （等全部帧槽 fence）前不得复用。
    graveyard: Vec<u32>,
    /// 材质槽（bump-only，不回收——量级 = 唯一材质数）。
    material_slots: HashMap<AssetId<StandardMaterial>, u32>,
    used_materials: u32,
    /// set2 布局（图形管线 layout 的素材；集与布局同源）。
    set2_layout: vk::DescriptorSetLayout,
    /// set2 每帧槽一套（b1/b2 指向各自帧槽的环），init 一次写入。
    frame_sets: Vec<vk::DescriptorSet>,
    pool: vk::DescriptorPool,
    /// 停尸房排空累计（回收计量）。
    slots_retired_total: u64,
}

impl InstanceTables {
    /// 建表链：十种 buffer（对象表按 CONCURRENT 双族，transfer 写 graphics 读；
    /// 九种环各 `MAX_FRAMES_IN_FLIGHT` 份，命令环 DEVICE_LOCAL、其余 host-visible）
    /// → set2 layout（三条只读 storage + 七条 compute 专属）→ pool + 每帧槽 set →
    /// 描述符一次写入 → 档位环零填充。任何失败回收已建对象（全有或全无）。
    ///
    /// # Errors
    /// buffer/描述符创建失败、档位环写入失败。
    pub fn new(
        device: &Device,
        contract: &MemoryContract,
        sharing_families: &[u32],
    ) -> Result<Self> {
        let object_bytes = align_up(
            u64::from(OBJECT_TABLE_CAPACITY) * OBJECT_RECORD_SIZE,
            1024 * 1024,
        );
        let model_bytes = align_up(u64::from(OBJECT_TABLE_CAPACITY) * MODEL_STRIDE, 1024 * 1024);
        let material_bytes =
            align_up(u64::from(MATERIAL_TABLE_CAPACITY) * MATERIAL_RECORD_SIZE, 1024 * 1024);
        let mark_bytes = align_up(u64::from(OBJECT_TABLE_CAPACITY) * MARK_SIZE, 1024 * 1024);
        let occ_mark_bytes = align_up(u64::from(OBJECT_TABLE_CAPACITY) * OCC_MARK_SIZE, 1024 * 1024);
        let presence_bytes =
            align_up(u64::from(OBJECT_TABLE_CAPACITY) * PRESENCE_ENTRY_SIZE, 1024 * 1024);
        let command_bytes =
            align_up(u64::from(OBJECT_TABLE_CAPACITY) * INDIRECT_COMMAND_SIZE, 1024 * 1024);
        let lod_table_bytes =
            align_up(u64::from(OBJECT_TABLE_CAPACITY) * LOD_TABLE_STRIDE, 1024 * 1024);
        let lod_level_bytes =
            align_up(u64::from(OBJECT_TABLE_CAPACITY) * LOD_LEVEL_SIZE, 1024 * 1024);
        let gap_mark_bytes =
            align_up(u64::from(OBJECT_TABLE_CAPACITY) * GAP_MARK_SIZE, 1024 * 1024);
        // 对象表被 transfer 反复写、graphics 长期读：与顶点/索引池同款 CONCURRENT
        // 共享定案（所有权乒乓不值得；跨队列可见性由图形提交等票据收口）
        let object_buffer = GpuBuffer::create_with_families(
            device,
            contract,
            object_bytes,
            BufferRole::DeviceStorage,
            sharing_families,
        )?;
        // 失败即 `?` 早退：已建 buffer 均未进 Self，随局部变量逆序 Drop 自拆
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
            capacity_objects: OBJECT_TABLE_CAPACITY,
            capacity_materials: MATERIAL_TABLE_CAPACITY,
            used_objects: 0,
            free_objects: Vec::new(),
            graveyard: Vec::new(),
            material_slots: HashMap::new(),
            used_materials: 0,
            set2_layout,
            frame_sets,
            pool,
            slots_retired_total: 0,
        };
        // 档位环零填充：迟滞状态的 prev 语义需要"未派发前 = 最细档 0"
        //（未定义内容会让首帧迟滞读垃圾档位）。表环不填——条目哨兵
        // 0 = 未驻留，消费端回退对象记录基准引脚，语义自洽。
        let zeros = vec![0u8; lod_level_bytes as usize];
        for ring in &mut tables.lod_level_rings {
            ring.write(0, &zeros)?;
        }
        info!(
            "GPU 常驻绘制数据就绪:对象表 {OBJECT_TABLE_CAPACITY} 槽({object_bytes}B,DEVICE_LOCAL,记录 48B/槽写一次)\
             + 模型环 {MODEL_STRIDE}B ×{MAX_FRAMES_IN_FLIGHT}({model_bytes}B/槽)\
             + 材质环 {MATERIAL_RECORD_SIZE}B ×{MAX_FRAMES_IN_FLIGHT}({material_bytes}B/槽,\
             容量 {MATERIAL_TABLE_CAPACITY} 材质槽 bump-only)\
             + 标记环 {MARK_SIZE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({mark_bytes}B/槽,5.2 剔除可见标记)\
             + 遮挡标记环 {OCC_MARK_SIZE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({occ_mark_bytes}B/槽,6.1 遮挡独立标记)\
             + 在场环 {PRESENCE_ENTRY_SIZE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({presence_bytes}B/槽,5.3 可绘行槽号列表)\
             + 命令环 {INDIRECT_COMMAND_SIZE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({command_bytes}B/槽,\
             5.3 indirect 命令,DEVICE_LOCAL)\
             + LOD 变体表环 {LOD_TABLE_STRIDE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({lod_table_bytes}B/槽,\
             6.2 槽→各档绘制引脚,{MAX_LOD_LEVELS} 档)\
             + LOD 档位环 {LOD_LEVEL_SIZE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({lod_level_bytes}B/槽,6.2 选档状态,零填充)\
             + 缺口标记环 {GAP_MARK_SIZE}B/槽 ×{MAX_FRAMES_IN_FLIGHT}({gap_mark_bytes}B/槽,6.6 逐槽缺口位图,cull 清零+build_indirect 置位);\
             set2 三条只读 storage + compute 专属 b3/b4/b5/b6/b7/b8/b9 ×{MAX_FRAMES_IN_FLIGHT} 帧,描述符 init 一次写入"
        );
        Ok(tables)
    }

    /// set2 布局 + pool + 每帧槽 set + 描述符一次写入。失败路径逐段回收
    /// （布局 → 池的逆序；buffer 由调用方局部变量 Drop）。
    ///
    /// # Safety
    /// buffer 句柄须刚创建成功且寿命覆盖本函数。
    #[expect(
        clippy::too_many_arguments,
        reason = "buffer 家族逐份传入是本函数的形态（每份一个 binding），拆参数束会藏掉与 set/binding 表的逐项对账"
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
            let stages = vk::ShaderStageFlags::COMPUTE
                | vk::ShaderStageFlags::VERTEX
                | vk::ShaderStageFlags::FRAGMENT;
            let storage = |binding: u32| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(binding)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(stages)
            };
            // b3/b4/b5/b6/b7/b8/b9 = 剔除标记(read_write)/在场环/命令环/遮挡标记
            // (read_write)/LOD 变体表(read)/LOD 档位环(read_write)/缺口标记环
            // (read_write) storage：compute 专属——图形 shader 不
            // 声明不使用，stage 收窄到 COMPUTE（b0/b1/b2 的超集声明不动）。b5 的
            // 另一读者 cmd_draw_indexed_indirect 不经描述符（命令参数直取 buffer），
            // 不在此 stage 面内。
            let compute_only = |binding: u32| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(binding)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE)
            };
            let bindings = [storage(0), storage(1), storage(2), compute_only(3), compute_only(4), compute_only(5), compute_only(6), compute_only(7), compute_only(8), compute_only(9)];
            let set2_layout = device
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .map_err(VulkanError::from)?;
            let pool = match device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(MAX_FRAMES_IN_FLIGHT as u32)
                    .pool_sizes(&[vk::DescriptorPoolSize::default()
                        .ty(vk::DescriptorType::STORAGE_BUFFER)
                        .descriptor_count((MAX_FRAMES_IN_FLIGHT * bindings.len()) as u32)]),
                None,
            ) {
                Ok(p) => p,
                Err(e) => {
                    device.destroy_descriptor_set_layout(set2_layout, None);
                    return Err(VulkanError::from(e));
                }
            };
            let sets = match device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&[set2_layout; MAX_FRAMES_IN_FLIGHT]),
            ) {
                Ok(s) => s,
                Err(e) => {
                    device.destroy_descriptor_pool(pool, None);
                    device.destroy_descriptor_set_layout(set2_layout, None);
                    return Err(VulkanError::from(e));
                }
            };
            // 描述符一次写入:b0 对象表(全量)、b1..b9 各帧槽自己的环(全量)。
            // info 先收齐再构造 write——write 持有 info 的引用,infos 之后不再变
            let mut infos = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT * bindings.len());
            let mut meta = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT * bindings.len());
            for (frame, set) in sets.iter().enumerate() {
                for (binding, buffer, range) in [
                    (
                        0u32,
                        object_buffer,
                        OBJECT_TABLE_CAPACITY as u64 * OBJECT_RECORD_SIZE,
                    ),
                    (
                        1,
                        &model_rings[frame],
                        OBJECT_TABLE_CAPACITY as u64 * MODEL_STRIDE,
                    ),
                    (
                        2,
                        &material_rings[frame],
                        MATERIAL_TABLE_CAPACITY as u64 * MATERIAL_RECORD_SIZE,
                    ),
                    (
                        3,
                        &mark_rings[frame],
                        OBJECT_TABLE_CAPACITY as u64 * MARK_SIZE,
                    ),
                    (
                        4,
                        &presence_rings[frame],
                        OBJECT_TABLE_CAPACITY as u64 * PRESENCE_ENTRY_SIZE,
                    ),
                    (
                        5,
                        &command_rings[frame],
                        OBJECT_TABLE_CAPACITY as u64 * INDIRECT_COMMAND_SIZE,
                    ),
                    (
                        6,
                        &occ_mark_rings[frame],
                        OBJECT_TABLE_CAPACITY as u64 * OCC_MARK_SIZE,
                    ),
                    (
                        7,
                        &lod_table_rings[frame],
                        OBJECT_TABLE_CAPACITY as u64 * LOD_TABLE_STRIDE,
                    ),
                    (
                        8,
                        &lod_level_rings[frame],
                        OBJECT_TABLE_CAPACITY as u64 * LOD_LEVEL_SIZE,
                    ),
                    (
                        9,
                        &gap_mark_rings[frame],
                        OBJECT_TABLE_CAPACITY as u64 * GAP_MARK_SIZE,
                    ),
                ] {
                    infos.push(
                        vk::DescriptorBufferInfo::default()
                            .buffer(buffer.buffer())
                            .offset(0)
                            .range(range),
                    );
                    meta.push((*set, binding));
                }
            }
            let writes: Vec<vk::WriteDescriptorSet> = meta
                .iter()
                .zip(&infos)
                .map(|((set, binding), info)| {
                    vk::WriteDescriptorSet::default()
                        .dst_set(*set)
                        .dst_binding(*binding)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(std::slice::from_ref(info))
                })
                .collect();
            device.update_descriptor_sets(&writes, &[]);
            Ok((set2_layout, sets.to_vec(), pool))
        }
    }

    /// set2 布局句柄（建图形管线 layout 的素材；集与布局同源）。
    #[must_use]
    pub fn set2_layout(&self) -> vk::DescriptorSetLayout {
        self.set2_layout
    }

    /// 第 `frame` 帧槽的 set2（表集,按帧槽绑定）。
    #[must_use]
    pub fn frame_set2(&self, frame: usize) -> vk::DescriptorSet {
        self.frame_sets[frame]
    }

    /// 对象表 buffer 句柄（staging 批次拷贝目标）。
    #[must_use]
    pub fn object_buffer(&self) -> vk::Buffer {
        self.object_buffer.buffer()
    }

    /// 第 `frame` 帧槽的模型环（draw_frame 增量重写的写入口）。
    #[must_use]
    pub fn model_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.model_rings[frame]
    }

    /// 第 `frame` 帧槽的材质环（draw_frame 全量重写的写入口）。
    #[must_use]
    pub fn material_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.material_rings[frame]
    }

    /// 第 `frame` 帧槽的剔除标记环（compute 写 / A/B 关闭时 host 填 /
    /// 宿主诊断读——三者都在 `wait_for_slot` fence 纪律内）。
    #[must_use]
    pub fn mark_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.mark_rings[frame]
    }

    /// 第 `frame` 帧槽的遮挡标记环（遮挡测试 compute 写 / fallback 时
    /// host 填全可见 / 宿主诊断读——A/B 归因读数口，fence 纪律同上）。
    #[must_use]
    pub fn occ_mark_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.occ_mark_rings[frame]
    }

    /// 第 `frame` 帧槽的在场环（draw_frame 全量重写可绘行槽号列表，
    /// `wait_for_slot` 之后——与帧 UBO/双环同一 fence 证据）。
    #[must_use]
    pub fn presence_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.presence_rings[frame]
    }

    /// 第 `frame` 帧槽的 indirect 命令环 buffer 句柄（build_indirect compute
    /// 写、`cmd_draw_indexed_indirect` 读——纯设备侧，宿主零访问）。
    #[must_use]
    pub fn indirect_buffer(&self, frame: usize) -> vk::Buffer {
        self.command_rings[frame].buffer()
    }

    /// 第 `frame` 帧槽的 LOD 变体表环（draw_frame 对脏行增量重写
    /// "槽 → 各档绘制引脚"，`wait_for_slot` fence 纪律同模型环）。
    #[must_use]
    pub fn lod_table_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.lod_table_rings[frame]
    }

    /// 第 `frame` 帧槽的 LOD 档位环（选档 compute 写 / 关闭档 host 填 0 /
    /// 宿主诊断直读——迟滞状态随环轮转，fence 纪律同标记环）。
    #[must_use]
    pub fn lod_level_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.lod_level_rings[frame]
    }

    /// 第 `frame` 帧槽的缺口标记环：cull 清零 + build_indirect 置位，
    /// 宿主诊断读数/BRP census 走 `read`（帧奇偶纪律 = 本帧槽内容是两帧前
    /// compute 的输出）。
    pub fn gap_mark_ring(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.gap_mark_rings[frame]
    }

    /// 分配一个实例槽（free list 优先,bump 兜底;容量用尽报错不越界）。
    ///
    /// # Errors
    /// 实例槽容量耗尽。
    pub fn alloc_object_slot(&mut self) -> Result<u32> {
        if let Some(slot) = self.free_objects.pop() {
            return Ok(slot);
        }
        if self.used_objects < self.capacity_objects {
            let slot = self.used_objects;
            self.used_objects += 1;
            Ok(slot)
        } else {
            Err(VulkanError::Capacity(format!(
                "对象表容量耗尽:已用 {}/{OBJECT_TABLE_CAPACITY}——超过压测口径的实例规模,\
                 显式报错,不静默截断",
                self.used_objects
            )))
        }
    }

    /// 回收一个实例槽（行删除/换绑换下）：进停尸房**不进 free list**——
    /// 在飞 draw 可能还在读旧记录，排空（"图形最后使用"）后才可复用。
    pub fn retire_object_slot(&mut self, slot: u32) {
        self.graveyard.push(slot);
    }

    /// 排空停尸房：等"图形最后使用"（帧侧等待回调）通过后旧槽归还 free list。
    /// 调用点 = flush_uploads 停尸房段（与网格/贴图停尸房同一节拍,共用同一次
    /// 帧侧等待）。空房直接返回不等待（稳态零成本）。
    ///
    /// # Errors
    /// 帧侧等待失败（停尸房保留,下帧重试,槽多活一帧无悬空）。
    pub fn drain_graveyard(&mut self, wait_draws: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        if self.graveyard.is_empty() {
            return Ok(());
        }
        wait_draws()?;
        let count = self.graveyard.len();
        self.free_objects.append(&mut self.graveyard);
        self.slots_retired_total += count as u64;
        info!("对象表停尸房排空:归还 {count} 个实例槽(图形最后使用已过,可复用)");
        Ok(())
    }

    /// 材质槽查取（bump-only：查无才分配,跨帧稳定——对象记录里的材质索引
    /// 不因材质内容变化而变化,内容变化由材质环每帧重写吸收）。
    ///
    /// # Errors
    /// 材质槽容量耗尽。
    pub fn ensure_material_slot(&mut self, id: AssetId<StandardMaterial>) -> Result<u32> {
        if let Some(&slot) = self.material_slots.get(&id) {
            return Ok(slot);
        }
        if self.used_materials >= self.capacity_materials {
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

    /// 材质槽只读查取（材质环写入定位用；查无 = 该材质尚无槽——可绘行的
    /// 实例槽不变式保证其材质槽必在，防御性 None 由调用方跳过）。
    #[must_use]
    pub fn material_slot(&self, id: AssetId<StandardMaterial>) -> Option<u32> {
        self.material_slots.get(&id).copied()
    }

    /// 已用实例槽数（验收统计口;含停尸房与 free list 在途——历史峰值语义
    /// 与 MeshPool 的 used 游标一致,复用不推进游标）。
    #[must_use]
    pub fn used_object_slots(&self) -> u32 {
        self.used_objects
    }

    /// 实例槽容量（验收读数口）。
    #[must_use]
    pub fn capacity_objects(&self) -> u32 {
        self.capacity_objects
    }

    /// 已分配材质槽数（验收统计口）。
    #[must_use]
    pub fn used_material_slots(&self) -> u32 {
        self.used_materials
    }

    /// 材质槽容量（验收读数口）。
    #[must_use]
    pub fn capacity_materials(&self) -> u32 {
        self.capacity_materials
    }

    /// 停尸房在房槽数（排空判定读数）。
    #[must_use]
    pub fn graveyard_count(&self) -> usize {
        self.graveyard.len()
    }

    /// 实例槽停尸房排空累计。
    #[must_use]
    pub fn slots_retired_total(&self) -> u64 {
        self.slots_retired_total
    }
}

impl Drop for InstanceTables {
    fn drop(&mut self) {
        // 契约边界:不等 GPU——退出排空由 teardown_vulkan 的 device_wait_idle
        // 先于一切资源 Drop 完成。pool 先拆(frees sets),layout 次拆;
        // 对象表与九种环(共 1 + 9×MAX_FRAMES_IN_FLIGHT 个 buffer)随字段 Drop。
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
