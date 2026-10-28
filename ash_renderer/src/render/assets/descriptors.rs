//! 描述符对象与槽位发布。
//!
//! 职责三件：
//! 1. **限额对账**：UAB 轨五项限额与候选容量 [`TABLE_CAPACITY`] 的显式对账。UAB 轨与
//!    普通轨分开记账，普通轨素材（maxPerStageDescriptorSampledImages）不作数；
//!    容量不够 = `Init` 冒泡出局，不静默截断、不写越界。
//! 2. **描述符对象**：set0 常驻双表（b0 SAMPLED_IMAGE×容量 / b1
//!    SAMPLER×容量，UAB+PARTIALLY_BOUND）+ set1 每帧 UBO。UAB 三层配套
//!    在此收齐后两层：binding 旗标 + layout 创建旗标（`UPDATE_AFTER_BIND_POOL`，
//!    VUID-03000）+ pool 旗标（`UPDATE_AFTER_BIND`）；feature 位第一层在
//!    [`crate::render::context`]。
//! 3. **槽位分配与发布**：free list 分槽、采样器按 [`sampler_key`]
//!    功能参数去重、fallback 槽 0（白 1×1 + 线性采样器）、发布接上传票据。
//!
//! set/binding 表（本模块是 CPU 侧真身）：set0 b0 = `texture_2d` 数组（runtime，
//! 无长度）、b1 = sampler 数组；set1 b0 = 每帧 UBO（[`FRAME_UBO_SIZE`]）；set2 归
//! [`super::instance_table`]。场景 pipeline layout 无 push range。
//!
//! stage flags 取 `COMPUTE|VERTEX|FRAGMENT` 超集：探针走 compute、主渲染走
//! graphics，超集声明只影响驱动的描述符优化面，不改变合法性。
//!
//! **覆盖安全**：[`Self::publish`] 只写排空点归还过的**新槽**
//! （"图形最后使用"已过）——UAB 只授权
//! "提交前后可更新可观察"，不授权覆盖仍被未完成命令消费的描述符；内容重传
//! 改走"新槽发布 + 旧槽延迟回收"（编排层 upload.rs）。回收侧
//! [`Self::retire_texture`]/[`Self::retire_sampler`] 的安全契约写在各自 doc 里，
//! 排空点统一收口。

use std::collections::HashMap;

use ash::{vk, Device, Instance};
use bevy::image::ImageSamplerDescriptor;
use bevy::log::info;

use crate::common::bytes::{put_mat4, put_u32, put_vec4};
use crate::common::error::{Result, VulkanError};

use super::images::{sampler_key, GpuImage, ImageSpec};
use crate::render::resources::{BufferRole, GpuBuffer, MemoryContract};
use super::uploader::{StagingImageCopy, UploadBatch, Uploader};
use crate::render::MAX_FRAMES_IN_FLIGHT;

/// 常驻表容量。`BindlessTables::new` 按设备 UAB 轨五项限额核对，不够即报错——
/// 不静默降容量。纹理表与采样器表同容量（双数组布局下无各自异容的必要；异容
/// 需求出现时拆两个常量）。
pub const TABLE_CAPACITY: u32 = 1024;

/// 每帧 UBO 字节数（set1 b0）。208B（mat4 view_proj + 灯光参数 + mode + mat4
/// inv_view_proj + vec4 camera_pos，偏移见 `FrameUniformsLayout` 静态断言表）；
/// binding 形状（b0 UNIFORM_BUFFER）与 pipeline layout 不随尺寸变，range 在写入
/// 时的 `DescriptorBufferInfo`，扩容不动 ABI。
pub const FRAME_UBO_SIZE: u64 = 208;

/// 帧数据打包模式（UBO `mode` 字段；4 起为合成 pass 专属的 G-buffer 通道仪器；
/// 语义见 `gbuffer_composite.wgsl` 与 host 侧材质模式说明）。
pub const FRAME_MODE_LAMBERT: u32 = 0;
pub const FRAME_MODE_UNLIT: u32 = 1;
pub const FRAME_MODE_NORMAL: u32 = 2;
/// 世界位置可视化：G-buffer 深度反投影重建的世界坐标编码直出——
/// 深度重建机制的验收仪器（仅合成 pass 消费）。
pub const FRAME_MODE_POSITION: u32 = 3;
/// G-buffer 调试仪器：matparam 单通道灰度直出——mutate 材质热改
///（metallic/roughness）的写后读验收口。
pub const FRAME_MODE_METALLIC: u32 = 4;
pub const FRAME_MODE_ROUGHNESS: u32 = 5;
pub const FRAME_MODE_AO: u32 = 6;
pub const FRAME_MODE_REFLECTANCE: u32 = 7;
/// reverse-Z 深度线性化灰度（clip.w / far——透视 w = 视空间深度）。
pub const FRAME_MODE_DEPTH: u32 = 8;

/// 每帧 UBO 的宿主侧取数产物（`draw_frame` 组装，[`pack_frame_uniforms`] 排布）。
pub struct FrameUniformsData {
    /// view × projection（列主序展平约定同 push 的 model）。
    pub view_proj: bevy::math::Mat4,
    /// inv(view_proj)：clip→world 反投影——G-buffer 深度重建世界
    /// 位置的常量面；相机缺席帧 identity 兜底（与 view_proj 同源同帧）。
    pub inv_view_proj: bevy::math::Mat4,
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
    /// 相机世界位置（xyz：光照累积的 V = normalize(camera_pos − world)
    /// 取数面；w 未用）。相机缺席帧 zeros 兜底（与 view_proj 同源同帧）。
    pub camera_pos: [f32; 4],
}

/// [`FrameUniformsData`] 的 WGSL 布局镜像（uniform address space）：本类型不参与
/// 运行时（字节由 [`pack_frame_uniforms`] 按偏移手工排布），`offset_of!` 静态断言
/// 把 WGSL/描述符 range/CPU 三方钉在一起。uniform 对齐 16：mode 后垫到 128。
#[repr(C)]
struct FrameUniformsLayout {
    view_proj: [f32; 16],
    dir_to_light: [f32; 4],
    ambient_color: [f32; 4],
    light_color: [f32; 4],
    mode: u32,
    _pad: [u32; 3],
    inv_view_proj: [f32; 16],
    camera_pos: [f32; 4],
}

const _: () = {
    assert!(std::mem::offset_of!(FrameUniformsLayout, view_proj) == 0);
    assert!(std::mem::offset_of!(FrameUniformsLayout, dir_to_light) == 64);
    assert!(std::mem::offset_of!(FrameUniformsLayout, ambient_color) == 80);
    assert!(std::mem::offset_of!(FrameUniformsLayout, light_color) == 96);
    assert!(std::mem::offset_of!(FrameUniformsLayout, mode) == 112);
    assert!(std::mem::offset_of!(FrameUniformsLayout, inv_view_proj) == 128);
    assert!(std::mem::offset_of!(FrameUniformsLayout, camera_pos) == 192);
    assert!(std::mem::size_of::<FrameUniformsLayout>() == 208);
    assert!(FRAME_UBO_SIZE == 208);
};

/// [`FrameUniformsData`] → 208B uniform 字节（偏移按 `FrameUniformsLayout` 的
/// 静态断言表手工排布）。
#[must_use]
pub fn pack_frame_uniforms(d: &FrameUniformsData) -> [u8; FRAME_UBO_SIZE as usize] {
    let mut out = [0u8; FRAME_UBO_SIZE as usize];
    put_mat4(&mut out, 0, &d.view_proj);
    put_vec4(&mut out, 64, &d.dir_to_light);
    put_vec4(&mut out, 80, &d.ambient_color);
    put_vec4(&mut out, 96, &d.light_color);
    put_u32(&mut out, 112, d.mode);
    // 116..128 对齐垫保持 0
    put_mat4(&mut out, 128, &d.inv_view_proj);
    put_vec4(&mut out, 192, &d.camera_pos);
    out
}

/// 一张贴图的描述符槽位绑定（per-draw 参数 `tex_index/sampler_index` 的取值）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotBinding {
    /// set0 b0（SAMPLED_IMAGE 数组）的下标。
    pub texture: u32,
    /// set0 b1（SAMPLER 数组）的下标。
    pub sampler: u32,
}

/// 单表槽位分配器：bump 游标 + 回收栈（free list）。分配取回收栈顶，栈空取
/// bump；`capacity` 用尽报错不越界。回收仅在 [`BindlessTables::retire_*`] 发生，
/// 静态资产的主路径是纯 bump（一次收齐）。
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

    fn alloc(&mut self) -> Result<u32> {
        if let Some(slot) = self.free.pop() {
            return Ok(slot);
        }
        if self.used < self.capacity {
            let slot = self.used;
            self.used += 1;
            Ok(slot)
        } else {
            Err(VulkanError::Capacity(format!(
                "描述符表容量耗尽：已用 {used}/{capacity}（容量由 TABLE_CAPACITY 与设备限额对账定，\
                 静态资产超容走显式受控扩容，不写越界不截断）",
                used = self.used,
                capacity = self.capacity,
            )))
        }
    }

    fn retire(&mut self, slot: u32) {
        self.free.push(slot);
    }

    fn used(&self) -> u32 {
        self.used
    }
}

struct BindlessBuildGuard<'a> {
    device: &'a Device,
    set0_layout: vk::DescriptorSetLayout,
    set1_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    committed: bool,
}

impl Drop for BindlessBuildGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        unsafe {
            if self.pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.pool, None);
            }
            if self.set1_layout != vk::DescriptorSetLayout::null() {
                self.device.destroy_descriptor_set_layout(self.set1_layout, None);
            }
            if self.set0_layout != vk::DescriptorSetLayout::null() {
                self.device.destroy_descriptor_set_layout(self.set0_layout, None);
            }
        }
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
    /// set1 每帧集：按帧槽各一套，按帧槽绑定。
    frame_sets: Vec<vk::DescriptorSet>,
    /// 每帧 UBO 本体（与 frame_sets 一一对应；写相机/灯光数据的载体）。
    frame_ubos: Vec<GpuBuffer>,
    /// 建表容量(对账定案的冻结值)。
    capacity: u32,
    /// 纹理槽分配器（fallback 占 0 号）。
    textures: SlotAllocator,
    /// 采样器槽分配器（fallback 占 0 号）。
    samplers: SlotAllocator,
    /// 功能参数键 → 采样器槽（去重账本）。
    sampler_slots: HashMap<super::images::SamplerKey, u32>,
    /// 采样器本体：槽号 → 本表创建并持有的 VkSampler。槽内描述符
    /// 引用的句柄与本槽同寿，不随任何一张贴图的卸载悬空——同键共享方全部
    /// 退场后槽进待回收账，本体在排空点销毁。
    sampler_bodies: HashMap<u32, vk::Sampler>,
    /// 卸载请求的待回收槽账：retire_* 只把槽记进此账、**不进**
    /// free list——在飞 draw 仍可能正采样这些槽的描述符，槽复用必须等
    /// "图形最后使用"（排空点=[`Self::drain_retired_slots`]，由 flush 停尸房
    /// 段在帧侧等待后调用，与 GpuImage/池区间共用同一次等待）。采样器槽的
    /// 去重账目（sampler_slots）在请求时即移除：新发布者拿新槽，不与待回收
    /// 槽交叠（前置条件③由排空点+此移除共同保证）。
    retired_textures: Vec<u32>,
    retired_samplers: Vec<u32>,
    /// 待销毁的采样器本体（已从 sampler_bodies 摘出，等排空点）。
    retired_sampler_bodies: Vec<(u32, vk::Sampler)>,
    /// fallback 贴图本体（白 1×1；槽 0 的 view 来源，随表同寿）。
    fallback: GpuImage,
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
    ) -> Result<Self> {
        // ---- 1) 限额对账：UAB 轨五项,逐项落日志留证据。注意这组限额
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
        // 每阶段与每 set 是两道独立闸;全池总额按双表满载 + 每帧 UBO 计
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
                 显式报错,不静默截断;调容量须回 set/binding 表文档对账"
            )));
        }
        info!(
            "限额对账通过:容量 {capacity}×2 张表 + {MAX_FRAMES_IN_FLIGHT} 帧 UBO = {pool_total} 描述符,五项限额全满足"
        );

        // ---- 2) 双 layout:binding 旗标经 BindingFlagsCreateInfo 扩展结构挂入
        //（ash 0.38 的 DescriptorSetLayoutBinding 无旗标字段）,
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
        let mut guard = BindlessBuildGuard {
            device,
            set0_layout,
            set1_layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            committed: false,
        };
        guard.set1_layout = unsafe {
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
                guard.set1_layout,
                &mut guard,
            )
        };
        if result.is_ok() {
            guard.committed = true;
        }
        result
    }

    /// 建表链后半段(layout 已建);UBO/fallback 半成品随局部变量 Drop。
    ///
    /// 坑:失败路径回收不全——只有 set 分配失败回收 pool;UBO/fallback 段失败
    /// pool 不回收;fallback 发布失败时 `tables` 的 Drop 已拆两个 layout,外层
    /// [`Self::new`] 会再拆一次。均只在 init 失败(随即 Tier② 退出)时触发。
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
        guard: &mut BindlessBuildGuard<'_>,
    ) -> Result<Self> {
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
        guard.pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND)
                    .max_sets(1 + MAX_FRAMES_IN_FLIGHT as u32)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }?;
        let pool = guard.pool;
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
        let sets = sets.map_err(VulkanError::from)?;
        let mut frame_sets = sets;
        let resident_set = frame_sets.remove(0);
        // ---- 5) 每帧 UBO(帧槽一一对应)+ set1 描述符写入(初值 = identity view_proj
        // + 灯光占位(方向留非零防 NaN,直射/环境/灯色全零) + Lambert 模式;
        // 运行期每帧重写)----
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
        // 双族(与生产贴图同款) ----
        let fallback_sharing: Vec<u32> = if uploader.queue_family() != graphics_family {
            vec![uploader.queue_family(), graphics_family]
        } else {
            Vec::new()
        };
        let fallback_spec = ImageSpec {
            width: 1,
            height: 1,
            format: vk::Format::R8G8B8A8_UNORM,
            // 1×1 单级（fallback 白图，无链）
            mip_levels: 1,
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
        let fallback_ticket = uploader
            .submit_batch(UploadBatch {
                staging: vec![255, 255, 255, 255],
                image_uploads: vec![StagingImageCopy {
                    image: fallback.image(),
                    src_offset: 0,
                    width: 1,
                    height: 1,
                    mip_levels: 1,
                }],
                // 图像 CONCURRENT 双族共享,不随批次 release
                ..Default::default()
            })?
            .ok_or_else(|| VulkanError::Upload("内部矛盾:fallback 批次非空却未产生提交".into()))?;
        // 坑：必须当场等完。fallback 的 Drop 不等 GPU，后续任一步失败丢掉它时拷贝可能还在写
        uploader.wait_until(fallback_ticket)?;
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
            sampler_bodies: HashMap::new(),
            retired_textures: Vec::new(),
            retired_samplers: Vec::new(),
            retired_sampler_bodies: Vec::new(),
            fallback,
        };
        guard.pool = vk::DescriptorPool::null();
        guard.set0_layout = vk::DescriptorSetLayout::null();
        guard.set1_layout = vk::DescriptorSetLayout::null();
        guard.committed = true;
        // fallback 占双表 0 号槽(采样器去重账本同步登记 linear 键;本体
        // 由表按键创建持有,不取 GpuImage 的采样器——GpuImage 不建采样器)
        let slots = tables.publish(
            tables.fallback.view(),
            &sampler_key(&ImageSamplerDescriptor::linear()),
        )?;
        debug_assert_eq!(slots.texture, 0, "fallback 必占纹理槽 0");
        debug_assert_eq!(slots.sampler, 0, "fallback 必占采样器槽 0");
        info!(
            "常驻描述符表就绪:set0 双表(容量 {capacity}×2,UAB+PARTIALLY_BOUND 三层配套)\
             + set1 每帧 UBO×{MAX_FRAMES_IN_FLIGHT}({FRAME_UBO_SIZE}B,identity 初值);\
             fallback 白图占槽 0(texture+ sampler),fallback 票据 #{ticket}",
            ticket = fallback_ticket,
        );
        Ok(tables)
    }

    /// set0 常驻集句柄(帧录制绑定)。
    #[must_use]
    pub fn resident_set(&self) -> vk::DescriptorSet {
        self.resident_set
    }

    /// set0 布局(建 pipeline layout 的素材;集与布局同源,不可另建)。
    #[must_use]
    pub fn set0_layout(&self) -> vk::DescriptorSetLayout {
        self.set0_layout
    }

    /// set1 布局(同上)。
    #[must_use]
    pub fn set1_layout(&self) -> vk::DescriptorSetLayout {
        self.set1_layout
    }

    /// 第 `frame` 帧槽的 set1(按帧槽绑定)。
    #[must_use]
    pub fn frame_set(&self, frame: usize) -> vk::DescriptorSet {
        self.frame_sets[frame]
    }

    /// 第 `frame` 帧槽的 UBO 本体(写相机/灯光数据的入口)。
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

    /// 常驻表容量(对账与容量判断的证据口)。
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
    /// free list 给出的槽必先经排空点归还（"图形最后使用"已过），覆盖竞态
    /// 在结构上不存在；采样器本体由本表按键创建持有（槽内句柄与本槽
    /// 同寿，不随任何一张贴图的卸载悬空）。
    ///
    /// 句柄寿命契约(与 [`GpuImage::create` 接受裸参数同款纪律]):`view`
    /// 须为合法句柄,且其本体寿命覆盖本表——槽位持有的是句柄借用,
    /// 本体先行销毁而槽位仍被采样 = 悬空描述符,由驻留条目/停尸房双账兜住。
    ///
    /// # Errors
    /// 纹理/采样器槽容量耗尽、采样器创建失败（半建槽回滚，账目不漂移）。
    pub fn publish(
        &mut self,
        view: vk::ImageView,
        key: &super::images::SamplerKey,
    ) -> Result<SlotBinding> {
        let texture = self.textures.alloc()?;
        let sampler_slot = match self.sampler_slots.get(key) {
            Some(&slot) => slot,
            None => {
                let slot = self.samplers.alloc()?;
                match key.create_sampler(&self.device) {
                    Ok(sampler) => {
                        self.sampler_bodies.insert(slot, sampler);
                        self.sampler_slots.insert(*key, slot);
                    }
                    // 半建回滚:纹理/采样器两槽归还,本体不落账
                    Err(e) => {
                        self.samplers.retire(slot);
                        self.textures.free.push(texture);
                        return Err(e);
                    }
                }
                slot
            }
        };
        let image_info = vk::DescriptorImageInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let sampler_info = vk::DescriptorImageInfo::default()
            .sampler(self.sampler_bodies[&sampler_slot]);
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

    /// 槽回收请求：把槽记进待回收账，**不立即进 free list**——在飞
    /// draw 仍可能正采样这些槽（描述符在 draw 执行时被 GPU 读），立即复用 =
    /// 在飞 draw 看到新贴图（覆盖）或悬空。前置条件（编排层保证）：
    /// ① 槽内 view 本体已同步进停尸房（GpuImage 销毁由同一排空点
    /// 收口，"本体先于槽复用消失"不成立的方向被停尸房挡住）；采样器本体归
    /// 本表，随槽账同进退；
    /// ② 采样器槽仅当同键共享方全部退场才请求（请求时移除去重账目，见
    /// [`Self::retire_sampler`]）；
    /// ③ 槽真正回 free list 在 [`Self::drain_retired_slots`]（"图形最后使用"
    /// 之后），回收后同槽再发布与旧票据完成无交叠。
    pub fn retire_texture(&mut self, texture: u32) {
        self.retired_textures.push(texture);
    }

    /// 采样器槽回收请求（前置条件②见 [`Self::retire_texture`]；去重账目在
    /// 此移除——新发布者拿新槽，不与待回收槽交叠）。本体随槽进
    /// 待销毁账，排空点统一 vkDestroySampler。
    pub fn retire_sampler(&mut self, key: &super::images::SamplerKey, sampler: u32) {
        if self.sampler_slots.remove(key) == Some(sampler)
            && let Some(body) = self.sampler_bodies.remove(&sampler)
        {
            self.retired_sampler_bodies.push((sampler, body));
        }
    }

    /// 按槽号回收采样器槽：反查功能键后走 [`Self::retire_sampler`]
    /// ——调用方（ImageCache）持驻留条目的槽位绑定，功能键在表内；查无 =
    /// 静默 no-op（fallback 槽 0 之外不该发生，发生即账目漂移的可见信号）。
    pub fn retire_sampler_by_slot(&mut self, sampler: u32) {
        if let Some(key) = self
            .sampler_slots
            .iter()
            .find(|(_, s)| **s == sampler)
            .map(|(k, _)| *k)
        {
            self.retire_sampler(&key, sampler);
        }
    }

    /// 排空待回收槽：调用点 = flush 停尸房段（帧侧等待"图形最后
    /// 使用"通过后）——此刻无在飞 draw 再采样这些槽，归还 free list 可复用；
    /// 采样器本体同点销毁。返回归还的槽总数（读数口）。
    pub fn drain_retired_slots(&mut self) -> usize {
        let count = self.retired_textures.len() + self.retired_sampler_bodies.len();
        for (slot, sampler) in self.retired_sampler_bodies.drain(..) {
            unsafe { self.device.destroy_sampler(sampler, None) };
            self.retired_samplers.push(slot);
        }
        self.textures.free.append(&mut self.retired_textures);
        self.samplers.free.append(&mut self.retired_samplers);
        count
    }

    /// 待回收槽数（排空判定的统计口）。
    #[must_use]
    pub fn retired_slot_count(&self) -> usize {
        self.retired_textures.len() + self.retired_sampler_bodies.len()
    }
}

impl Drop for BindlessTables {
    fn drop(&mut self) {
        // 契约边界(与 Uploader/GpuBuffer 同款):不等 GPU;退出排空由
        // teardown_vulkan 的 device_wait_idle 先于一切资源 Drop。
        unsafe {
            self.device.destroy_descriptor_pool(self.pool, None);
            self.device
                .destroy_descriptor_set_layout(self.set1_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.set0_layout, None);
            for sampler in self.sampler_bodies.values() {
                self.device.destroy_sampler(*sampler, None);
            }
            for (_, sampler) in self.retired_sampler_bodies.drain(..) {
                self.device.destroy_sampler(sampler, None);
            }
        }
    }
}

/// 无灯帧的方向光占位（xyz 单位化合法方向，防归一化 NaN；直射/环境/灯色全零）。
/// descriptors.rs 的 identity 初值与 frame.rs 的无灯分支双点共用此常量。
pub const PLACEHOLDER_DIR_TO_LIGHT: [f32; 4] = [0.5, 1.0, 0.3, 0.0];

/// identity 初值的帧数据（view_proj = identity、灯光占位、Lambert 模式）：
/// 表建好到首帧写入之间若被采样（理论不发生，帧循环首帧即重写），画面是
/// identity 形状而非悬空值。
fn identity_frame_uniforms() -> FrameUniformsData {
    FrameUniformsData {
        view_proj: bevy::math::Mat4::IDENTITY,
        inv_view_proj: bevy::math::Mat4::IDENTITY,
        dir_to_light: PLACEHOLDER_DIR_TO_LIGHT,
        ambient_color: [0.0; 4],
        light_color: [0.0; 4],
        mode: FRAME_MODE_LAMBERT,
        camera_pos: [0.0; 4],
    }
}
