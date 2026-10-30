//! set0 bindless 常驻表：全部贴图和采样器各放进一个 [`TABLE_CAPACITY`] 长的描述符数组，
//! shader 按 per-draw 参数里的下标（[`SlotBinding`]）取用。set1 归
//! [`super::frame_uniforms`]，set2 归 [`super::instance_table`]。
//!
//! - **建表先核对限额**：设备的 UPDATE_AFTER_BIND 限额是单独一套（不看普通的
//!   `maxPerStageDescriptorSampledImages`），不够 [`TABLE_CAPACITY`] 就报错，不缩容量。
//! - **UPDATE_AFTER_BIND 要三处一起开**：binding 旗标、layout 的 `UPDATE_AFTER_BIND_POOL`、
//!   pool 的 `UPDATE_AFTER_BIND`，漏一处报 VUID-03000；设备 feature 在 [`crate::render::Context`]。
//! - **槽 0 是 fallback**：白色 1×1 + 线性采样器，建表时占好、永不回收。缺贴图的 draw 用它，
//!   因为 PARTIALLY_BOUND 的空槽不能访问。
//! - **只往新槽写**：UPDATE_AFTER_BIND 允许绑定后更新描述符，但不允许覆盖在飞 draw 还在读的
//!   描述符。所以贴图换内容是"发布到新槽 + 旧槽交回"，旧槽等 GPU 用完才回到空闲栈。
//! - **采样器按参数去重并计引用**：同参数的贴图共用一个采样器槽，最后一次发布交回时才回收。
//!
//! 坑：`ImageSampler::Default` 解析成线性采样器，和槽 0 同参数，所以普通贴图、fallback、
//! egui 图集会共用采样器槽 0。采样器能不能回收只能由本表的引用数判断，调用方看不全用户。

use std::collections::HashMap;

use ash::{vk, Device, Instance};
use bevy::image::ImageSamplerDescriptor;
use bevy::log::info;

use super::images::{sampler_key, GpuImage, ImageSpec, SamplerKey};
use super::slot_allocator::SlotAllocator;
use super::uploader::{StagingImageCopy, UploadBatch, Uploader};
use crate::common::error::{Result, VulkanError};
use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::set_layout::{stages, BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc};
use crate::render::core::device::memory::contract::MemoryContract;

/// set0 两个数组的长度：贴图槽数 = 采样器槽数。
pub const TABLE_CAPACITY: u32 = 1024;

/// 取超集：探针走 compute、主渲染走 graphics；只影响驱动优化，不影响合法性。
const ALL_STAGES: vk::ShaderStageFlags =
    stages(&[vk::ShaderStageFlags::COMPUTE, vk::ShaderStageFlags::VERTEX, vk::ShaderStageFlags::FRAGMENT]);
const UAB_PARTIALLY: vk::DescriptorBindingFlags = vk::DescriptorBindingFlags::from_raw(
    vk::DescriptorBindingFlags::UPDATE_AFTER_BIND.as_raw() | vk::DescriptorBindingFlags::PARTIALLY_BOUND.as_raw(),
);

/// set0：b0 贴图数组、b1 采样器数组。
pub(crate) static BINDLESS_SET: SetLayoutDesc = SetLayoutDesc {
    name: "bindless",
    bindings: &[
        BindingDesc::new(0, DescKind::SampledImage, ALL_STAGES).array(TABLE_CAPACITY, UAB_PARTIALLY),
        BindingDesc::new(1, DescKind::Sampler, ALL_STAGES).array(TABLE_CAPACITY, UAB_PARTIALLY),
    ],
    flags: vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL,
};

/// 一张贴图在 set0 里的两个下标（per-draw 参数 `tex_index` / `sampler_index`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotBinding {
    /// b0 贴图数组下标。
    pub texture: u32,
    /// b1 采样器数组下标。
    pub sampler: u32,
}

/// 一个去重后的采样器。
struct SharedSampler {
    key: SamplerKey,
    handle: vk::Sampler,
    /// 还没交回的发布次数；归零即可回收。
    users: u32,
}

/// set0 常驻表与它的槽位账。
#[derive(bevy::prelude::Resource)]
pub struct BindlessTables {
    device: Device,
    layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    /// 全程只有这一个集，槽位在运行中直接更新。
    set: vk::DescriptorSet,
    texture_slots: SlotAllocator,
    sampler_slots: SlotAllocator,
    sampler_slot_by_key: HashMap<SamplerKey, u32>,
    samplers: HashMap<u32, SharedSampler>,
    /// 引用已归零、槽已进待回收账的采样器，排空时和槽一起销毁。
    retired_samplers: Vec<vk::Sampler>,
    /// 槽 0 引用的白图，随表同寿。
    fallback: GpuImage,
}

impl BindlessTables {
    /// 建表：核对限额 → 上传 fallback 白图 → 建 layout / pool / set → 发布 fallback 占槽 0。
    /// 任一步失败，已建的对象由表的 Drop 回收。
    ///
    /// `capacity` 是槽位预算，只有测试耗尽路径时才小于 [`TABLE_CAPACITY`]；限额和 pool
    /// 一律按布局声明的长度算。
    ///
    /// # Errors
    /// 预算超过布局长度、设备限额不够、Vulkan 创建失败、fallback 上传失败。
    pub fn new(
        device: &Device,
        instance: &Instance,
        physical_device: vk::PhysicalDevice,
        contract: &MemoryContract,
        uploader: &mut Uploader,
        graphics_family: u32,
        capacity: u32,
    ) -> Result<Self> {
        if capacity > TABLE_CAPACITY {
            return Err(VulkanError::Init(format!("槽位预算 {capacity} 超过 set0 布局长度 {TABLE_CAPACITY}")));
        }
        check_uab_limits(instance, physical_device)?;
        let fallback = upload_fallback(device, instance, physical_device, contract, uploader, graphics_family)?;
        let mut tables = Self {
            device: device.clone(),
            layout: BINDLESS_SET.create(device)?,
            pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
            texture_slots: SlotAllocator::new(capacity),
            sampler_slots: SlotAllocator::new(capacity),
            sampler_slot_by_key: HashMap::new(),
            samplers: HashMap::new(),
            retired_samplers: Vec::new(),
            fallback,
        };
        let pool_flags = if BINDLESS_SET.needs_uab_pool() {
            vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND
        } else {
            vk::DescriptorPoolCreateFlags::empty()
        };
        tables.pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .flags(pool_flags)
                    .max_sets(1)
                    .pool_sizes(&BINDLESS_SET.pool_sizes_vec(1)),
                None,
            )
        }?;
        tables.set = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(tables.pool)
                    .set_layouts(&[tables.layout]),
            )
        }
        .map_err(VulkanError::from)?[0];
        let slots = tables.publish(tables.fallback.view(), &sampler_key(&ImageSamplerDescriptor::linear()))?;
        debug_assert_eq!(slots, Self::fallback_slots(), "fallback 必须占槽 0");
        info!(
            "常驻描述符表就绪:set0 双表(容量 {capacity}×2,UAB+PARTIALLY_BOUND);fallback 白图占槽 0"
        );
        Ok(tables)
    }

    /// set0 布局（登记给管线组装；集与布局同源，不可另建）。
    #[must_use]
    pub fn set_layout(&self) -> SetLayout {
        SetLayout::new(&BINDLESS_SET, self.layout)
    }

    /// set0 描述符集（帧录制绑定）。
    #[must_use]
    pub fn descriptor_set(&self) -> DescSet {
        DescSet::new(&BINDLESS_SET, self.set)
    }

    /// 缺贴图时 per-draw 参数用的槽位。
    #[must_use]
    pub fn fallback_slots() -> SlotBinding {
        SlotBinding {
            texture: 0,
            sampler: 0,
        }
    }

    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.texture_slots.capacity()
    }

    /// 用过的贴图槽数（含 fallback 和待回收的槽）。
    #[must_use]
    pub fn used_texture_slots(&self) -> u32 {
        self.texture_slots.high_water()
    }

    /// 用过的采样器槽数（含待回收的槽）；和贴图张数对比可看出去重效果。
    #[must_use]
    pub fn used_sampler_slots(&self) -> u32 {
        self.sampler_slots.high_water()
    }

    /// 已交回、还没排空的槽数（贴图槽 + 采样器槽）。
    #[must_use]
    pub fn retired_slot_count(&self) -> usize {
        self.texture_slots.retired_count() + self.sampler_slots.retired_count()
    }

    /// 发布一张贴图：分一个新贴图槽，采样器按参数复用或新建，写两条描述符。
    /// 返回值卸载时必须交回 [`Self::retire`]，否则采样器引用数不会归零。
    ///
    /// 坑：`view` 只是借用，它的图必须活到这个槽被排空之后，否则描述符悬空；
    /// 由 ImageCache / 图集的待销毁队列保证。
    ///
    /// # Errors
    /// 槽容量用完、采样器创建失败（已分的槽原样归还）。
    pub fn publish(&mut self, view: vk::ImageView, key: &SamplerKey) -> Result<SlotBinding> {
        let texture = self.texture_slots.alloc().ok_or_else(|| capacity_error("贴图", &self.texture_slots))?;
        let (sampler, handle) = match self.acquire_sampler(key) {
            Ok(acquired) => acquired,
            Err(e) => {
                self.texture_slots.release(texture);
                return Err(e);
            }
        };
        // 写入和声明对不上才会失败，属于代码错误，不回滚槽位
        let mut writer = DescriptorWriter::new(&BINDLESS_SET, self.set);
        writer.image_at(0, texture, view, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        writer.sampler_at(1, sampler, handle);
        writer.submit(&self.device)?;
        Ok(SlotBinding { texture, sampler })
    }

    /// 交回一次发布：贴图槽进待回收账；采样器引用减一，归零时槽和句柄一起进待回收账。
    /// 槽要等 [`Self::drain_retired_slots`] 之后才会被复用。fallback 槽位不要交回。
    pub fn retire(&mut self, slots: SlotBinding) {
        debug_assert_ne!(slots, Self::fallback_slots(), "fallback 槽位永不回收");
        self.texture_slots.retire(slots.texture);
        if let Some(shared) = self.samplers.get_mut(&slots.sampler) {
            shared.users -= 1;
            if shared.users == 0
                && let Some(shared) = self.samplers.remove(&slots.sampler)
            {
                self.sampler_slot_by_key.remove(&shared.key);
                self.sampler_slots.retire(slots.sampler);
                self.retired_samplers.push(shared.handle);
            }
        }
    }

    /// 等 GPU 用完后把待回收的槽归还空闲栈，并销毁引用归零的采样器；没有待回收的槽时
    /// 不等待。返回归还的槽数。
    ///
    /// # Errors
    /// 等待失败（待回收账原样保留，下次重试）。
    pub fn drain_retired_slots(&mut self, wait_draws: &mut dyn FnMut() -> Result<()>) -> Result<usize> {
        if self.retired_slot_count() == 0 {
            return Ok(0);
        }
        wait_draws()?;
        for handle in self.retired_samplers.drain(..) {
            unsafe { self.device.destroy_sampler(handle, None) };
        }
        Ok(self.texture_slots.drain_retired() + self.sampler_slots.drain_retired())
    }

    /// 同参数的采样器已有槽就加一次引用，否则分新槽并创建句柄。
    fn acquire_sampler(&mut self, key: &SamplerKey) -> Result<(u32, vk::Sampler)> {
        if let Some(&slot) = self.sampler_slot_by_key.get(key)
            && let Some(shared) = self.samplers.get_mut(&slot)
        {
            shared.users += 1;
            return Ok((slot, shared.handle));
        }
        let slot = self.sampler_slots.alloc().ok_or_else(|| capacity_error("采样器", &self.sampler_slots))?;
        let handle = match key.create_sampler(&self.device) {
            Ok(handle) => handle,
            Err(e) => {
                self.sampler_slots.release(slot);
                return Err(e);
            }
        };
        self.sampler_slot_by_key.insert(*key, slot);
        self.samplers.insert(slot, SharedSampler { key: *key, handle, users: 1 });
        Ok((slot, handle))
    }
}

impl Drop for BindlessTables {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown_vulkan 已先 device_wait_idle。
        // 建表中途失败时 pool / layout 可能是空句柄，销毁空句柄是合法的空操作。
        unsafe {
            self.device.destroy_descriptor_pool(self.pool, None);
            self.device.destroy_descriptor_set_layout(self.layout, None);
            for shared in self.samplers.values() {
                self.device.destroy_sampler(shared.handle, None);
            }
            for handle in self.retired_samplers.drain(..) {
                self.device.destroy_sampler(handle, None);
            }
        }
    }
}

fn capacity_error(what: &str, slots: &SlotAllocator) -> VulkanError {
    VulkanError::Capacity(format!(
        "描述符表{what}槽容量耗尽：已用 {}/{}（不越界、不截断）",
        slots.high_water(),
        slots.capacity(),
    ))
}

/// 核对设备的 UPDATE_AFTER_BIND 限额是否够布局长度，逐项落日志。
///
/// 坑：这组限额不在普通 `Limits` 里，要经 `PhysicalDeviceProperties2` 挂
/// `PhysicalDeviceDescriptorIndexingProperties` 查。
fn check_uab_limits(instance: &Instance, physical_device: vk::PhysicalDevice) -> Result<()> {
    let mut di_props = vk::PhysicalDeviceDescriptorIndexingProperties::default();
    let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut di_props);
    unsafe { instance.get_physical_device_properties2(physical_device, &mut props2) };
    let need = u64::from(TABLE_CAPACITY);
    // 每阶段和每 set 是两道独立的限制；全池总额按两张表满载算
    let checks = [
        ("每阶段 sampledImages", di_props.max_per_stage_descriptor_update_after_bind_sampled_images, need),
        ("每 set sampledImages", di_props.max_descriptor_set_update_after_bind_sampled_images, need),
        ("每阶段 samplers", di_props.max_per_stage_descriptor_update_after_bind_samplers, need),
        ("每 set samplers", di_props.max_descriptor_set_update_after_bind_samplers, need),
        ("全池总额", di_props.max_update_after_bind_descriptors_in_all_pools, need * 2),
    ];
    let report: Vec<String> = checks.iter().map(|(name, have, need)| format!("{name} {have}/{need}")).collect();
    info!("UAB 限额对账(实测/需要):{}", report.join(" | "));
    if let Some((name, have, need)) = checks.iter().find(|(_, have, need)| u64::from(*have) < *need) {
        return Err(VulkanError::Init(format!(
            "UAB 限额不够布局长度 {TABLE_CAPACITY}:{name} 实测 {have} < 需要 {need}"
        )));
    }
    Ok(())
}

/// 建 fallback 白图（UNORM 白在任何色彩空间下都是安全缺省）并当场等上传完成。
/// 上传队列和图形队列不同族时按两族 CONCURRENT 共享，和生产贴图一致。
fn upload_fallback(
    device: &Device,
    instance: &Instance,
    physical_device: vk::PhysicalDevice,
    contract: &MemoryContract,
    uploader: &mut Uploader,
    graphics_family: u32,
) -> Result<GpuImage> {
    let sharing: Vec<u32> = if uploader.queue_family() == graphics_family {
        Vec::new()
    } else {
        vec![uploader.queue_family(), graphics_family]
    };
    let spec = ImageSpec {
        width: 1,
        height: 1,
        format: vk::Format::R8G8B8A8_UNORM,
        mip_levels: 1,
        sampler: ImageSamplerDescriptor::linear(),
    };
    let image = GpuImage::create(device, instance, physical_device, contract, &spec, &sharing)?;
    let ticket = uploader
        .submit_batch(UploadBatch {
            staging: vec![255, 255, 255, 255],
            image_uploads: vec![StagingImageCopy {
                image: image.image(),
                src_offset: 0,
                width: 1,
                height: 1,
                mip_levels: 1,
            }],
            // CONCURRENT 共享，不需要队列族所有权转移
            ..Default::default()
        })?
        .ok_or_else(|| VulkanError::Upload("内部矛盾:fallback 批次非空却未产生提交".into()))?;
    // 坑：必须当场等完。GpuImage 的 Drop 不等 GPU，建表后面任一步失败丢掉它时拷贝可能还在写
    uploader.wait_until(ticket)?;
    Ok(image)
}
