//! set1 每帧 UBO：相机矩阵、灯光、着色模式、相机位置。
//!
//! 按帧槽各一份 UBO + 一个 set，每帧整块重写；写之前帧槽 fence 已确认上一轮
//! 提交执行完，所以同槽重写不会撞上在飞的 draw。
//!
//! 字节布局由 [`FrameUniformTableLayout`] 的 `offset_of!` 静态断言钉住，WGSL、描述符
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
pub(crate) static FRAME_UNIFORM_SET_DESC: SetLayoutDesc = SetLayoutDesc::new(
    "frame",
    &[BindingDesc::new(
        0,
        DescKind::UniformBuffer,
        stages(&[vk::ShaderStageFlags::COMPUTE, vk::ShaderStageFlags::VERTEX, vk::ShaderStageFlags::FRAGMENT]),
    )],
);

/// 每帧 UBO 字节数（set1 b0；偏移见 `FrameUniformTableLayout` 静态断言）。range 在写描述符时
/// 给出，改尺寸不影响 binding 形状与 pipeline layout。
pub const FRAME_UBO_SIZE: u64 = 208;

/// UBO `mode` 字段取值（语义见 `gbuffer_composite.wgsl`）；4 起只有合成 pass 使用，用来直出
/// G-buffer 单通道调试。
pub const FRAME_MODE_LAMBERT: u32 = 0;
pub const FRAME_MODE_UNLIT: u32 = 1;
pub const FRAME_MODE_NORMAL: u32 = 2;
/// 世界位置可视化：用 G-buffer 深度反投影重建世界坐标后直出，用来检查深度重建。
pub const FRAME_MODE_POSITION: u32 = 3;
/// matparam 单通道灰度直出，用来检查材质热改（metallic/roughness 等）是否写进 G-buffer。
pub const FRAME_MODE_METALLIC: u32 = 4;
pub const FRAME_MODE_ROUGHNESS: u32 = 5;
pub const FRAME_MODE_AO: u32 = 6;
pub const FRAME_MODE_REFLECTANCE: u32 = 7;
/// reverse-Z 深度线性化灰度（clip.w / far；透视下 w 即视空间深度）。
pub const FRAME_MODE_DEPTH: u32 = 8;

/// 无方向光时的占位方向（xyz 已是合法方向，避免归一化出 NaN；灯色全零）。
/// 建表初值与 draw_frame 的无灯分支共用。
pub const PLACEHOLDER_DIR_TO_LIGHT: [f32; 4] = [0.5, 1.0, 0.3, 0.0];

/// 每帧 UBO 的宿主侧数据（`draw_frame` 组装，[`pack_frame_uniforms`] 打包）。
pub struct FrameUniformsData {
    pub view_proj: bevy::math::Mat4,
    /// clip → world 反投影，G-buffer 用它从深度重建世界位置；无相机帧为 identity。
    pub inv_view_proj: bevy::math::Mat4,
    /// 表面指向光源的方向（xyz 单位向量，w=0），与 bevy `dir_to_light = transform.back()`
    /// 同义，Lambert 直接 `dot(n, dir_to_light)`，不再取负。
    pub dir_to_light: [f32; 4],
    /// 环境光（rgb = LinearRgba × brightness，同 bevy；w 未用）。
    pub ambient_color: [f32; 4],
    /// 方向光（rgb = LinearRgba × illuminance，同 bevy；w 未用）。
    pub light_color: [f32; 4],
    /// 着色模式（`FRAME_MODE_*`）。
    pub mode: u32,
    /// 相机世界位置（xyz，光照用 V = normalize(camera_pos − world)；w 未用）。无相机帧为 0。
    pub camera_pos: [f32; 4],
}

/// [`FrameUniformsData`] 的 WGSL uniform 布局镜像：不参与运行时，只用 `offset_of!` 静态断言
/// 核对 [`pack_frame_uniforms`] 的偏移。uniform 对齐 16，mode 后垫到 128。
#[repr(C)]
struct FrameUniformTableLayout {
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
    assert!(std::mem::offset_of!(FrameUniformTableLayout, view_proj) == 0);
    assert!(std::mem::offset_of!(FrameUniformTableLayout, dir_to_light) == 64);
    assert!(std::mem::offset_of!(FrameUniformTableLayout, ambient_color) == 80);
    assert!(std::mem::offset_of!(FrameUniformTableLayout, light_color) == 96);
    assert!(std::mem::offset_of!(FrameUniformTableLayout, mode) == 112);
    assert!(std::mem::offset_of!(FrameUniformTableLayout, inv_view_proj) == 128);
    assert!(std::mem::offset_of!(FrameUniformTableLayout, camera_pos) == 192);
    assert!(std::mem::size_of::<FrameUniformTableLayout>() == 208);
    assert!(FRAME_UBO_SIZE == 208);
};

/// [`FrameUniformsData`] → 208B uniform 字节（偏移见 `FrameUniformTableLayout`）。
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

/// 建表初值（identity 矩阵、占位灯光、Lambert）：首帧写入前若被读到，也不是未定义内容。
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
pub struct FrameUniformTable {
    device: Device,
    layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    ubos: Vec<GpuBuffer>,
}

impl FrameUniformTable {
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
        this.layout = FRAME_UNIFORM_SET_DESC.create(device)?;
        let slots = MAX_FRAMES_IN_FLIGHT as u32;
        let pool_sizes = FRAME_UNIFORM_SET_DESC.pool_sizes_vec(slots);
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
        for (set, ubo) in this.sets.iter().zip(&this.ubos) {
            let mut writer = DescriptorWriter::new(&FRAME_UNIFORM_SET_DESC, *set);
            writer.buffer(0, ubo.buffer(), 0, FRAME_UBO_SIZE);
            writer.submit(device)?;
        }
        Ok(this)
    }

    /// set1 布局（登记给管线组装；集与布局同源，不可另建）。
    #[must_use]
    pub fn set_layout(&self) -> SetLayout {
        SetLayout::new(&FRAME_UNIFORM_SET_DESC, self.layout)
    }

    /// 第 `frame` 帧槽的 set1（按帧槽绑定）。
    #[must_use]
    pub fn frame_set(&self, frame: usize) -> DescSet {
        DescSet::new(&FRAME_UNIFORM_SET_DESC, self.sets[frame])
    }

    /// 第 `frame` 帧槽的 UBO（写相机/灯光数据，须在该帧槽 fence 之后）。
    pub fn ubo(&mut self, frame: usize) -> &mut GpuBuffer {
        &mut self.ubos[frame]
    }
}

impl Drop for FrameUniformTable {
    fn drop(&mut self) {
        // 不等 GPU：退出时 teardown_vulkan 已先 device_wait_idle。set 随 pool 释放；
        // 建表中途失败时句柄可能为空，跳过。
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
