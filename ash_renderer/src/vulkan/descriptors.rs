//! 描述符对象与槽位发布（施工 3.3.2 限额对账 + 3.3.3 描述符对象 + 3.3.4 槽位分配）。
//!
//! 职责三件：
//! 1. **限额对账（3.3.2）**：UAB 轨五项限额与候选容量 1024 的显式对账。UAB 轨与
//!    普通轨分开记账（特性族篇 §4），普通轨实测素材（maxPerStageDescriptorSampledImages
//!    = 1048576）不作数；容量不够 = `Init` 冒泡出局，不静默截断（施工计划 §2
//!    "容量不足：不写越界、不静默截断"）。
//! 2. **描述符对象（3.3.3）**：set0 常驻双表（b0 SAMPLED_IMAGE×容量 / b1
//!    SAMPLER×容量，UAB+PARTIALLY_BOUND）+ set1 每帧 UBO。三层配套（特性族篇 §3）
//!    在此收齐后两层：binding 旗标 + layout 创建旗标（`UPDATE_AFTER_BIND_POOL`，
//!    VUID-03000）+ pool 旗标（`UPDATE_AFTER_BIND`）；feature 位第一层在
//!    [`super::context`]（3.3.2 已启用）。
//! 3. **槽位分配与发布（3.3.4）**：free list 分槽、采样器按 [`sampler_key`]
//!    功能参数去重、fallback 槽 0（白 1×1 + 线性采样器）、发布接上传票据。
//!
//! set/binding 表（前置闸门冻结，本模块是 CPU 侧真身）：set0 b0 = `texture_2d`
//! 数组（runtime，无长度）、b1 = sampler 数组；set1 b0 = 每帧 UBO 64B。push 96B
//! 偏移 0/64/68/80 与 pipeline layout 不在本模块——归 3.4 管线施工。
//!
//! stage flags 取 `COMPUTE|VERTEX|FRAGMENT` 超集：3.3 探针走 compute、3.4 走
//! graphics，超集声明只影响驱动的描述符优化面，不改变合法性。
//!
//! **覆盖安全**（3.3.4 钉号"不覆盖 pending draw 仍可能读取的槽"）：[`Self::publish`]
//! 只写 free list 给出的**从未在飞**的新槽，M2 不改写已发布槽位，结构上不存在
//! 覆盖竞态；真正的覆盖风险在槽回收——[`Self::retire_texture`]/[`Self::retire_sampler`]
//! 是 M2 不调用的释放入口，前置条件（安全契约）写在各自 doc 里，运行时淘汰归
//! 步骤 4（施工计划 §3"M2 可不做运行时淘汰，但必须明确释放入口的安全条件"）。

use std::collections::HashMap;

use ash::{vk, Device, Instance};
use bevy::image::ImageSamplerDescriptor;
use bevy::log::info;

use crate::error::VulkanError;

use super::images::{sampler_key, GpuImage, ImageSpec};
use super::resources::{BufferRole, GpuBuffer, MemoryContract};
use super::uploader::{StagingImageCopy, Ticket, UploadBatch, Uploader};
use super::MAX_FRAMES_IN_FLIGHT;

/// 常驻表容量（3.3.2 候选定案值）。`BindlessTables::new` 按设备 UAB 轨五项限额
/// 核对，不够即报错——不静默降容量。纹理表与采样器表同容量（闸门冻结的双数组
/// 布局事实上排除了各自异容的必要；异容需求出现时拆两个常量再改）。
pub const TABLE_CAPACITY: u32 = 1024;

/// 每帧 UBO 字节数（set1 b0）。前置闸门冻结的 64B（mat4 view_proj）在 3.5.1 扩为
/// 128B 灯光版——扩容已随 3.5 定案登记进冻结记录；binding 形状（b0
/// UNIFORM_BUFFER）与 pipeline layout 不变，range 在写入时的
/// `DescriptorBufferInfo`，扩容不动 ABI。
pub const FRAME_UBO_SIZE: u64 = 128;

/// 帧数据打包模式（UBO `mode` 字段，与 `debug_draw.wgsl` 的 case 值逐字同源；
/// 非零值的语义见 host 侧 [`crate::host`] 的材质三模式说明）。
pub const FRAME_MODE_LAMBERT: u32 = 0;
pub const FRAME_MODE_UNLIT: u32 = 1;
pub const FRAME_MODE_NORMAL: u32 = 2;

/// 每帧 UBO 的宿主侧取数产物（`draw_frame` 组装，[`pack_frame_uniforms`] 排布）。
pub struct FrameUniformsData {
    /// view × projection（列主序展平约定同 push 的 model）。
    pub view_proj: bevy::math::Mat4,
    /// 表面到光方向（xyz 单位向量，w=0 占位）。**bevy GPU 同款语义**：
    /// `prepare_lights` 写 `dir_to_light = transform.back()`（forward 取负，
    /// "N·L 就绪"），Lambert 直接 `dot(n, dir_to_light)`，符号混淆在此斩断。
    pub dir_to_light: [f32; 4],
    /// 环境光（rgb = LinearRgba × brightness，bevy 同款；w 未用）。
    pub ambient_color: [f32; 4],
    /// 方向光（rgb = LinearRgba × illuminance，bevy 同款；w 未用）。
    pub light_color: [f32; 4],
    /// 材质模式（`FRAME_MODE_*`）。
    pub mode: u32,
}

/// [`FrameUniformsData`] 的 WGSL 布局镜像（uniform address space）：本类型不参与
/// 运行时（字节由 [`pack_frame_uniforms`] 按偏移手工排布），存在的意义与
/// `pipeline.rs::PushLayout` 同款——`offset_of!` 静态断言把 WGSL/描述符 range/CPU
/// 三方钉在一起。uniform 对齐 16：mode 后垫到 128。
#[repr(C)]
#[allow(dead_code)]
struct FrameUniformsLayout {
    view_proj: [f32; 16],
    dir_to_light: [f32; 4],
    ambient_color: [f32; 4],
    light_color: [f32; 4],
    mode: u32,
    _pad: [u32; 3],
}

const _: () = {
    assert!(std::mem::offset_of!(FrameUniformsLayout, view_proj) == 0);
    assert!(std::mem::offset_of!(FrameUniformsLayout, dir_to_light) == 64);
    assert!(std::mem::offset_of!(FrameUniformsLayout, ambient_color) == 80);
    assert!(std::mem::offset_of!(FrameUniformsLayout, light_color) == 96);
    assert!(std::mem::offset_of!(FrameUniformsLayout, mode) == 112);
    assert!(std::mem::size_of::<FrameUniformsLayout>() == 128);
    assert!(FRAME_UBO_SIZE == 128);
};

/// [`FrameUniformsData`] → 128B uniform 字节（偏移按 [`FrameUniformsLayout`] 的
/// 静态断言表手工排布，与 `pipeline.rs::pack_push` 同一纪律）。
#[must_use]
pub fn pack_frame_uniforms(d: &FrameUniformsData) -> [u8; FRAME_UBO_SIZE as usize] {
    let mut out = [0u8; FRAME_UBO_SIZE as usize];
    let mut put = |offset: usize, bytes: &[u8]| {
        out[offset..offset + bytes.len()].copy_from_slice(bytes);
    };
    for (i, v) in d.view_proj.to_cols_array().iter().enumerate() {
        put(i * 4, &v.to_le_bytes());
    }
    for (base, field) in [(64usize, &d.dir_to_light), (80, &d.ambient_color), (96, &d.light_color)] {
        for (i, v) in field.iter().enumerate() {
            put(base + i * 4, &v.to_le_bytes());
        }
    }
    put(112, &d.mode.to_le_bytes());
    // 116..128 对齐垫保持 0
    out
}

/// 一张贴图的描述符槽位绑定（3.4 per-draw 参数 `tex_index/sampler_index` 的取值）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotBinding {
    /// set0 b0（SAMPLED_IMAGE 数组）的下标。
    pub texture: u32,
    /// set0 b1（SAMPLER 数组）的下标。
    pub sampler: u32,
}

/// 单表槽位分配器：bump 游标 + 回收栈（free list）。分配取回收栈顶，栈空取
/// bump；`capacity` 用尽报错不越界。回收仅在 [`BindlessTables::retire_*`] 发生，
/// M2 主路径是纯 bump（静态资产一次收齐）。
struct SlotAllocator {
    capacity: u32,
    used: u32,
    free: Vec<u32>,
}

impl SlotAllocator {
    fn new(capacity: u32) -> Self {
        Self {
            capacity,
            used: 0,
            free: Vec::new(),
        }
    }

    fn alloc(&mut self) -> Result<u32, VulkanError> {
        if let Some(slot) = self.free.pop() {
            return Ok(slot);
        }
        if self.used < self.capacity {
            let slot = self.used;
            self.used += 1;
            Ok(slot)
        } else {
            Err(VulkanError::Upload(format!(
                "描述符表容量耗尽：已用 {used}/{capacity}（容量由 TABLE_CAPACITY 与设备限额对账定，\
                 静态资产超容按施工计划走显式受控扩容，不写越界不截断）",
                used = self.used,
                capacity = self.capacity,
            )))
        }
    }

    fn used(&self) -> u32 {
        self.used
    }
}

/// 常驻描述符表（set0 双表 + set1 每帧 UBO）与槽位账本。
#[derive(bevy::prelude::Resource)]
pub struct BindlessTables {
    device: Device,
    set0_layout: vk::DescriptorSetLayout,
    set1_layout: vk::DescriptorSetLayout,
    /// UAB pool：set0（UAB binding）与 set1（普通 binding）同池分配——pool 旗标
    /// 只是"允许"UAB，不强制。
    pool: vk::DescriptorPool,
    /// set0 常驻集：全期绑定一次，槽位运行中更新（UAB 的存在意义）。
    resident_set: vk::DescriptorSet,
    /// set1 每帧集：按帧槽各一套，3.4 按帧槽绑定。
    frame_sets: Vec<vk::DescriptorSet>,
    /// 每帧 UBO 本体（与 frame_sets 一一对应；3.5 接相机/灯光数据）。
    frame_ubos: Vec<GpuBuffer>,
    /// 建表容量(对账定案的冻结值)。
    capacity: u32,
    /// 纹理槽分配器（fallback 占 0 号）。
    textures: SlotAllocator,
    /// 采样器槽分配器（fallback 占 0 号）。
    samplers: SlotAllocator,
    /// 功能参数键 → 采样器槽（去重账本）。
    sampler_slots: HashMap<super::images::SamplerKey, u32>,
    /// fallback 贴图本体（白 1×1；槽 0 的 view/sampler 来源，随表同寿）。
    fallback: GpuImage,
    /// fallback 上传批次票据（M2 只留账；销毁前的等待由退出排空统一覆盖）。
    fallback_ticket: Ticket,
}

impl BindlessTables {
    /// 建表链：限额对账 → 双 layout（三层配套的后两层）→ UAB pool → 分配
    /// 1+MAX_FRAMES_IN_FLIGHT 个 set → 每帧 UBO 与 set1 写入 → fallback 贴图
    /// 上传并占 0 号双槽。任何失败回收已建对象（全有或全无）。
    ///
    /// # Errors
    /// UAB 限额不满足容量、Vulkan 创建/分配失败、fallback 上传失败。
    pub fn new(
        device: &Device,
        instance: &Instance,
        physical_device: vk::PhysicalDevice,
        contract: &MemoryContract,
        uploader: &mut Uploader,
        graphics_family: u32,
        capacity: u32,
    ) -> Result<Self, VulkanError> {        // ---- 1) 限额对账（3.3.2）：UAB 轨五项,逐项落日志留证据。注意这组限额
        //不在普通 Limits 里——它们是 descriptorIndexing 的 properties2 扩展
        //（PhysicalDeviceDescriptorIndexingProperties,经 Properties2 push_next 查询,
        //与 Features2 查支持同一形状）----
        let mut di_props = vk::PhysicalDeviceDescriptorIndexingProperties::default();
        let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut di_props);
        // # Safety:physical_device 来自成功枚举、instance 未销毁(ash 约定)
        unsafe { instance.get_physical_device_properties2(physical_device, &mut props2) };
        let per_stage_smp = di_props.max_per_stage_descriptor_update_after_bind_sampled_images;
        let per_set_smp = di_props.max_descriptor_set_update_after_bind_sampled_images;
        let per_stage_sam = di_props.max_per_stage_descriptor_update_after_bind_samplers;
        let per_set_sam = di_props.max_descriptor_set_update_after_bind_samplers;
        let all_pools = di_props.max_update_after_bind_descriptors_in_all_pools;
        info!(
            "UAB 轨限额对账(候选容量 {capacity}): 每阶段 sampledImages {per_stage_smp} / 每 set {per_set_smp} \
             | 每阶段 samplers {per_stage_sam} / 每 set {per_set_sam} | 全池总额 {all_pools}",
        );
        // 每阶段与每 set 是两道独立闸(特性族篇 §4);全池总额按双表满载 + 每帧 UBO 计
        let pool_total = u64::from(capacity) * 2 + MAX_FRAMES_IN_FLIGHT as u64;
        let checks = [
            ("每阶段 sampledImages", u64::from(per_stage_smp), u64::from(capacity)),
            ("每 set sampledImages", u64::from(per_set_smp), u64::from(capacity)),
            ("每阶段 samplers", u64::from(per_stage_sam), u64::from(capacity)),
            ("每 set samplers", u64::from(per_set_sam), u64::from(capacity)),
            ("全池 UAB 总额", u64::from(all_pools), pool_total),
        ];
        if let Some((name, have, need)) = checks.iter().find(|(_, have, need)| have < need) {
            return Err(VulkanError::Init(format!(
                "UAB 轨限额不够容量 {capacity}:{name} 实测 {have} < 需要 {need}——\
                 按施工计划显式报错,不静默截断;调容量须回 set/binding 表文档对账"
            )));
        }
        info!(
            "限额对账通过:容量 {capacity}×2 张表 + {MAX_FRAMES_IN_FLIGHT} 帧 UBO = {pool_total} 描述符,五项限额全满足"
        );

        // ---- 2) 双 layout:binding 旗标经 BindingFlagsCreateInfo 扩展结构挂入
        //（ash 0.38 的 DescriptorSetLayoutBinding 无旗标字段,闸门组 C 同款配方）,
        // layout 自身带 UPDATE_AFTER_BIND_POOL(VUID-03000 三层配套的第二层) ----
        let stages = vk::ShaderStageFlags::COMPUTE
            | vk::ShaderStageFlags::VERTEX
            | vk::ShaderStageFlags::FRAGMENT;
        let uab_partially = vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
            | vk::DescriptorBindingFlags::PARTIALLY_BOUND;
        let set0_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(capacity)
                .stage_flags(stages),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(capacity)
                .stage_flags(stages),
        ];
        let set0_binding_flags = [uab_partially, uab_partially];
        let mut set0_flags_info = vk::DescriptorSetLayoutBindingFlagsCreateInfo::default()
            .binding_flags(&set0_binding_flags);
        // # Safety:device 合法句柄,创建参数为契约常量
        let set0_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default()
                    .flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL)
                    .bindings(&set0_bindings)
                    .push_next(&mut set0_flags_info),
                None,
            )
        }?;
        let set1_bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
            .descriptor_count(1)
            .stage_flags(stages)];
        let set1_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&set1_bindings),
                None,
            )
        }?;
        let result = unsafe {
            Self::new_inner(
                device,
                instance,
                physical_device,
                contract,
                uploader,
                graphics_family,
                capacity,
                set0_layout,
                set1_layout,
            )
        };
        if result.is_err() {
            unsafe {
                device.destroy_descriptor_set_layout(set1_layout, None);
                device.destroy_descriptor_set_layout(set0_layout, None);
            }
        }
        result
    }

    /// 建表链后半段(layout 已建):失败路径回收 pool/sets/UBO/fallback。
    ///
    /// # Safety
    /// 两个 layout 须刚创建成功且未销毁;`uploader` 须可用(fallback 上传走它)。
    #[expect(
        clippy::too_many_arguments,
        reason = "Vulkan 创建链的参数表即依赖清单:设备/实例/物理设备/契约/上传器/族号/容量/两个 layout,逐项显式"
    )]
    unsafe fn new_inner(
        device: &Device,
        instance: &Instance,
        physical_device: vk::PhysicalDevice,
        contract: &MemoryContract,
        uploader: &mut Uploader,
        graphics_family: u32,
        capacity: u32,
        set0_layout: vk::DescriptorSetLayout,
        set1_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, VulkanError> {
        // ---- 3) UAB pool(三层配套的第三层;pool size 覆盖双表满载 + 每帧 UBO)----
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(capacity),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(capacity),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(MAX_FRAMES_IN_FLIGHT as u32),
        ];
        let pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND)
                    .max_sets(1 + MAX_FRAMES_IN_FLIGHT as u32)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }?;
        // ---- 4) 分配 set0×1 + set1×MAX_FRAMES(一次 allocate,普通 set 与 UAB set 同池合法)----
        let mut layouts = vec![set0_layout];
        layouts.extend(std::iter::repeat_n(set1_layout, MAX_FRAMES_IN_FLIGHT));
        let sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts),
            )
        };
        let sets = match sets {
            Ok(s) => s,
            Err(e) => {
                unsafe { device.destroy_descriptor_pool(pool, None) };
                return Err(VulkanError::Vk(e));
            }
        };
        let mut frame_sets = sets;
        let resident_set = frame_sets.remove(0);
        // ---- 5) 每帧 UBO(帧槽一一对应)+ set1 描述符写入(初值 = identity view_proj
        // + 灯光归零 + Lambert 模式;3.5.1 起运行期每帧重写)----
        let mut frame_ubos = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let mut ubo = GpuBuffer::create(device, contract, FRAME_UBO_SIZE, BufferRole::FrameUniform)?;
            ubo.write(0, &pack_frame_uniforms(&identity_frame_uniforms()))?;
            frame_ubos.push(ubo);
        }
        let ubo_infos: Vec<vk::DescriptorBufferInfo> = frame_ubos
            .iter()
            .map(|ubo| {
                vk::DescriptorBufferInfo::default()
                    .buffer(ubo.buffer())
                    .offset(0)
                    .range(FRAME_UBO_SIZE)
            })
            .collect();
        let writes: Vec<vk::WriteDescriptorSet> = frame_sets
            .iter()
            .zip(&ubo_infos)
            .map(|(set, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(*set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(std::slice::from_ref(info))
            })
            .collect();
        // # Safety:set 与 info 均合法,Vulkan 写入契约由参数构造保证
        unsafe { device.update_descriptor_sets(&writes, &[]) };
        // ---- 6) fallback:白 1×1(UNORM 线性白 = 任意色彩域的安全缺省)走普通
        // 上传链进 GPU,占 0 号双槽;真实贴图从 1 号起分槽。跨族设备 CONCURRENT
        // 双族(3.4 定案,与生产贴图同款) ----
        let fallback_sharing: Vec<u32> = if uploader.queue_family() != graphics_family {
            vec![uploader.queue_family(), graphics_family]
        } else {
            Vec::new()
        };
        let fallback_spec = ImageSpec {
            width: 1,
            height: 1,
            format: vk::Format::R8G8B8A8_UNORM,
            sampler: ImageSamplerDescriptor::linear(),
        };
        let fallback = GpuImage::create(
            device,
            instance,
            physical_device,
            contract,
            &fallback_spec,
            &fallback_sharing,
        )?;
        let fallback_ticket = match uploader.submit_batch(UploadBatch {
            staging: vec![255, 255, 255, 255],
            image_uploads: vec![StagingImageCopy {
                image: fallback.image(),
                src_offset: 0,
                width: 1,
                height: 1,
            }],
            // 3.4 定案:图像 CONCURRENT 双族共享,不再随批次 release
            ..Default::default()
        }) {
            Ok(Some(ticket)) => ticket,
            Ok(None) => {
                return Err(VulkanError::Upload(
                    "内部矛盾:fallback 批次非空却未产生提交".into(),
                ))
            }
            Err(e) => return Err(e),
        };
        let mut tables = Self {
            device: device.clone(),
            set0_layout,
            set1_layout,
            pool,
            resident_set,
            frame_sets,
            frame_ubos,
            capacity,
            textures: SlotAllocator::new(capacity),
            samplers: SlotAllocator::new(capacity),
            sampler_slots: HashMap::new(),
            fallback,
            fallback_ticket,
        };
        // fallback 占双表 0 号槽(采样器去重账本同步登记 linear 键)
        let slots = tables.publish(
            tables.fallback.view(),
            tables.fallback.sampler(),
            &sampler_key(&ImageSamplerDescriptor::linear()),
        )?;
        debug_assert_eq!(slots.texture, 0, "fallback 必占纹理槽 0");
        debug_assert_eq!(slots.sampler, 0, "fallback 必占采样器槽 0");
        info!(
            "常驻描述符表就绪:set0 双表(容量 {capacity}×2,UAB+PARTIALLY_BOUND 三层配套)\
             + set1 每帧 UBO×{MAX_FRAMES_IN_FLIGHT}(64B,identity 初值);\
             fallback 白图占槽 0(texture+ sampler),fallback 票据 #{ticket}",
            ticket = tables.fallback_ticket,
        );
        Ok(tables)
    }

    /// set0 常驻集句柄(3.4 帧录制绑定)。
    #[must_use]
    pub fn resident_set(&self) -> vk::DescriptorSet {
        self.resident_set
    }

    /// set0 布局(3.4 建 pipeline layout 的素材;集与布局同源,不可另建)。
    #[must_use]
    pub fn set0_layout(&self) -> vk::DescriptorSetLayout {
        self.set0_layout
    }

    /// set1 布局(同上)。
    #[must_use]
    pub fn set1_layout(&self) -> vk::DescriptorSetLayout {
        self.set1_layout
    }

    /// 第 `frame` 帧槽的 set1(3.4 按帧槽绑定;M2 内容为 identity)。
    #[must_use]
    pub fn frame_set(&self, frame: usize) -> vk::DescriptorSet {
        self.frame_sets[frame]
    }

    /// 第 `frame` 帧槽的 UBO 本体(3.5 写相机/灯光数据的入口)。
    #[must_use]
    pub fn frame_ubo(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.frame_ubos[frame]
    }

    /// fallback 槽位(缺资源时 per-draw 参数的合法缺省——partially bound 的空槽
    /// 不可访问,fallback 槽保证任何 draw 都有可采样的有效槽)。
    #[must_use]
    pub fn fallback_slots() -> SlotBinding {
        SlotBinding {
            texture: 0,
            sampler: 0,
        }
    }

    /// 常驻表容量(对账与 3.4 容量判断的证据口)。
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// 已占纹理槽数(发布日志与收账)。
    #[must_use]
    pub fn used_texture_slots(&self) -> u32 {
        self.textures.used()
    }

    /// 已占采样器槽数(去重证据:贴图张数 → 采样器槽数的收敛比)。
    #[must_use]
    pub fn used_sampler_slots(&self) -> u32 {
        self.samplers.used()
    }

    /// 发布一张贴图:纹理槽 free list 分配 + 采样器按功能键去重 → 两条描述符
    /// 写入(set0 b0/b1 各一条,dst_array_element 定位槽)。**只写新槽**——
    /// free list 给出的槽在 M2 从未进过任何在飞提交,覆盖竞态在结构上不存在。
    ///
    /// 句柄寿命契约(与 [`GpuImage::create` 接受裸参数同款纪律]):`view`/
    /// `sampler` 须为合法句柄,且其本体寿命覆盖本表——槽位持有的是句柄借用,
    /// 本体先行销毁而槽位仍被采样 = 悬空描述符,M2 靠"驻留资产与表同寿"成立,
    /// 运行时淘汰的完整账本归步骤 4(见 retire 前置条件)。
    ///
    /// # Errors
    /// 纹理/采样器槽容量耗尽(不静默截断,冒泡由调用方分流)。
    pub fn publish(
        &mut self,
        view: vk::ImageView,
        sampler: vk::Sampler,
        key: &super::images::SamplerKey,
    ) -> Result<SlotBinding, VulkanError> {
        let texture = self.textures.alloc()?;
        let sampler_slot = match self.sampler_slots.get(key) {
            Some(&slot) => slot,
            None => {
                let slot = self.samplers.alloc()?;
                self.sampler_slots.insert(*key, slot);
                slot
            }
        };
        let image_info = vk::DescriptorImageInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let sampler_info = vk::DescriptorImageInfo::default().sampler(sampler);
        // # Safety:set 常驻、binding 0/1 带 UAB 旗标,写入经 update_descriptor_sets 合法
        unsafe {
            self.device.update_descriptor_sets(
                &[
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.resident_set)
                        .dst_binding(0)
                        .dst_array_element(texture)
                        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                        .image_info(std::slice::from_ref(&image_info)),
                    vk::WriteDescriptorSet::default()
                        .dst_set(self.resident_set)
                        .dst_binding(1)
                        .dst_array_element(sampler_slot)
                        .descriptor_type(vk::DescriptorType::SAMPLER)
                        .image_info(std::slice::from_ref(&sampler_info)),
                ],
                &[],
            );
        }
        Ok(SlotBinding {
            texture,
            sampler: sampler_slot,
        })
    }

    /// 槽回收(M2 不调用的释放入口;运行时淘汰归步骤 4)。**前置条件即安全契约**:
    /// ① 该槽最后一次 GPU 使用的完成票据已等过(否则在飞 draw 采样悬空槽);
    /// ② 槽内 view/sampler 本体的宿主侧引用方已同步退出(采样器槽是多图共享的
    /// "首个发布者句柄"借用——retire 采样器槽前,同键全部贴图都得先退场);
    /// ③ 回收后同槽再发布,新资源内容与旧票据完成之间无交叠。
    /// 三条任一不满足即不得调用——验证层对"在飞改写"不保证执法(特性族篇 §0
    /// 契约类开关的责任转移),防线上只剩本契约。
    pub fn retire_texture(&mut self, texture: u32) {
        self.textures.free.push(texture);
    }

    /// 采样器槽回收(前置条件见 [`Self::retire_texture`] ②:同键共享方全部退场)。
    pub fn retire_sampler(&mut self, key: &super::images::SamplerKey, sampler: u32) {
        self.sampler_slots.remove(key);
        self.samplers.free.push(sampler);
    }
}

/// # Safety
/// 持有的 ash 句柄本质是整数;frame_ubos(GpuBuffer)与 fallback(GpuImage)已实现
/// Send/Sync,槽位账本是纯 Rust 数据;多线程共享由外部纪律保证(与 Uploader 同款:
/// 资源创建/销毁/发布只在持有方线程,方法只读句柄或经 &mut 独占)。
unsafe impl Send for BindlessTables {}
unsafe impl Sync for BindlessTables {}

impl Drop for BindlessTables {
    fn drop(&mut self) {
        // 契约边界(与 Uploader/GpuBuffer 同款):不等 GPU;退出排空由
        // teardown_vulkan 的 device_wait_idle 先于一切资源 Drop(D4 定案)。
        // pool 先拆(frees 全部 sets),layout 后拆;UBO/fallback 随字段自动 Drop。
        unsafe {
            self.device.destroy_descriptor_pool(self.pool, None);
            self.device
                .destroy_descriptor_set_layout(self.set1_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.set0_layout, None);
        }
    }
}

/// identity 初值的帧数据（view_proj = identity、灯光归零、Lambert 模式）：
/// 表建好到首帧写入之间若被采样（理论不发生，帧循环首帧即重写），画面同
/// 3.4 之前的 identity 形状而非悬空值。
fn identity_frame_uniforms() -> FrameUniformsData {
    FrameUniformsData {
        view_proj: bevy::math::Mat4::IDENTITY,
        dir_to_light: [0.5, 1.0, 0.3, 0.0],
        ambient_color: [0.0; 4],
        light_color: [0.0; 4],
        mode: FRAME_MODE_LAMBERT,
    }
}
