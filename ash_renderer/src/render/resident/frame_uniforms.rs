//! set1 每帧 UBO：相机矩阵、灯光、着色模式、相机位置。
//!
//! 按帧槽各一份 UBO + 一个 set，每帧整块重写；写之前帧槽 fence 已确认上一轮
//! 提交执行完，所以同槽重写不会撞上在飞的 draw。
//!
//! 字节布局由 [`FrameUniformsLayout`] 的 `offset_of!` 静态断言钉住，WGSL、描述符
//! range、CPU 打包三方必须一致；改字段要三处一起改。

use ash::{vk, Device};

use crate::common::bytes::{put_mat4, put_u32, put_vec4};
use crate::common::error::{Result, VulkanError};
use crate::render::core::binding::descriptor_writer::DescriptorWriter;
use crate::render::core::binding::set_layout::{stages, BindingDesc, DescKind, DescSet, SetLayout, SetLayoutDesc};
use crate::render::core::device::memory::buffer::GpuBuffer;
use crate::render::core::device::memory::contract::MemoryContract;
use crate::render::core::device::memory::role::BufferRole;
use crate::render::MAX_FRAMES_IN_FLIGHT;

/// set1 每帧 UBO（b0，[`FRAME_UBO_SIZE`]）。stage 取 `COMPUTE|VERTEX|FRAGMENT` 超集，
/// 只影响驱动优化，不影响合法性。
pub(crate) static FRAME_SET: SetLayoutDesc = SetLayoutDesc::new(
    "frame",
    &[BindingDesc::new(
        0,
        DescKind::UniformBuffer,
        stages(&[vk::ShaderStageFlags::COMPUTE, vk::ShaderStageFlags::VERTEX, vk::ShaderStageFlags::FRAGMENT]),
    )],
);

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

/// 无灯帧的方向光占位（xyz 单位化合法方向，防归一化 NaN；直射/环境/灯色全零）。
/// 建表时的 identity 初值与 frame.rs 的无灯分支共用此常量。
pub const PLACEHOLDER_DIR_TO_LIGHT: [f32; 4] = [0.5, 1.0, 0.3, 0.0];

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

/// identity 初值的帧数据（view_proj = identity、灯光占位、Lambert 模式）：
/// 建好到首帧写入之间若被采样（理论不发生，帧循环首帧即重写），画面是
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

/// set1 每帧 UBO 与描述符集，按帧槽各一份。
#[derive(bevy::prelude::Resource)]
pub struct FrameUniforms {
    device: Device,
    layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    ubos: Vec<GpuBuffer>,
}

impl FrameUniforms {
    /// 每帧槽建一份 UBO（写入 identity 初值）和一个 set，并把 UBO 写进 set。
    /// 失败时已建的部分由 Drop 回收。
    ///
    /// # Errors
    /// Vulkan 创建/分配失败、UBO 写入失败。
    pub fn new(device: &Device, contract: &MemoryContract) -> Result<Self> {
        let mut this = Self {
            device: device.clone(),
            layout: vk::DescriptorSetLayout::null(),
            pool: vk::DescriptorPool::null(),
            sets: Vec::new(),
            ubos: Vec::with_capacity(MAX_FRAMES_IN_FLIGHT),
        };
        let initial = pack_frame_uniforms(&identity_frame_uniforms());
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let mut ubo = GpuBuffer::create(device, contract, FRAME_UBO_SIZE, BufferRole::FrameUniform)?;
            ubo.write(0, &initial)?;
            this.ubos.push(ubo);
        }
        this.layout = FRAME_SET.create(device)?;
        let slots = MAX_FRAMES_IN_FLIGHT as u32;
        let pool_sizes = FRAME_SET.pool_sizes_vec(slots);
        this.pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default().max_sets(slots).pool_sizes(&pool_sizes),
                None,
            )
        }?;
        let layouts = vec![this.layout; MAX_FRAMES_IN_FLIGHT];
        this.sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default().descriptor_pool(this.pool).set_layouts(&layouts),
            )
        }
        .map_err(VulkanError::from)?;
        // 写入过声明校验（FRAME_SET b0 = UBO，写满才提交）
        for (set, ubo) in this.sets.iter().zip(&this.ubos) {
            let mut writer = DescriptorWriter::new(&FRAME_SET, *set);
            writer.buffer(0, ubo.buffer(), 0, FRAME_UBO_SIZE);
            writer.submit(device)?;
        }
        Ok(this)
    }

    /// set1 布局（登记给管线组装；集与布局同源，不可另建）。
    #[must_use]
    pub fn set_layout(&self) -> SetLayout {
        SetLayout::new(&FRAME_SET, self.layout)
    }

    /// 第 `frame` 帧槽的 set1（按帧槽绑定）。
    #[must_use]
    pub fn frame_set(&self, frame: usize) -> DescSet {
        DescSet::new(&FRAME_SET, self.sets[frame])
    }

    /// 第 `frame` 帧槽的 UBO 本体（写相机/灯光数据的入口，帧槽 fence 纪律内）。
    pub fn ubo(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.ubos[frame]
    }
}

impl Drop for FrameUniforms {
    fn drop(&mut self) {
        // 不等 GPU：退出排空由 teardown_vulkan 的 device_wait_idle 先于一切资源 Drop。
        // set 随 pool 一起释放；半建状态下的空句柄跳过。
        unsafe {
            if self.pool != vk::DescriptorPool::null() {
                self.device.destroy_descriptor_pool(self.pool, None);
            }
            if self.layout != vk::DescriptorSetLayout::null() {
                self.device.destroy_descriptor_set_layout(self.layout, None);
            }
        }
    }
}
