//! 求交器（9.4）：硬件 inline ray query 主后端。宿主侧只暴露 [`RayScene`]——BLAS 注册表
//! 与每帧 TLAS 的构建/更新/查询入口都在它后面，`vk::AccelerationStructureKHR` 这类硬件
//! 类型不出本模块（描述符集与录制层只拿句柄束）。WGSL 侧求交面收在
//! `shaders/common/raytracing.wgsl`，换后端 = 换那份片段 + 换本目录的后端模块。
//!
//! 生命周期：BLAS 挂 mesh 资产身份（commit 排建 / replace 重建 / retire 排销毁，驻留事件
//! 驱动，见 `blas.rs`）；TLAS 每帧从在场环全量重建（`tlas.rs`）。设备不支持 RT 时
//! [`RayScene::new`] 返回 None，整个模块不实例化。

mod blas;
mod tlas;

use ash::{vk, Device};
use bevy::asset::AssetId;
use bevy::mesh::Mesh;

use crate::common::error::Result;

use crate::render::core::device::context::Context;
use crate::render::core::device::rt::RtSupport;
use crate::render::core::record::barrier::MemoryBarrierDesc;
use crate::render::resident::assets::pool::MeshSlot;

pub use blas::BlasRegistry;
pub use tlas::TlasFrame;

/// RT 总开关（BRP `rt_set`）：feature 门之上再一层，供 A/B 与降级验证。`false` 时
/// 一切 RT 命令不录、调试图不写——画面与 9.3 收官态逐位一致。
#[derive(bevy::prelude::Resource, Clone, Copy, Debug, Default)]
pub struct RtSettings {
    /// 总开关（init 按能力门给初值；BRP rt_set 覆写）。
    pub enabled: bool,
    /// 调试图子视图：0 = 命中距离 t，1 = 世界法线，2 = 实例槽 hash（`rt_dump` 按它解码）。
    pub view: u32,
}

/// 求交器宿主入口：BLAS 注册表 + 每帧 TLAS + 设备能力/属性快照。
#[derive(bevy::prelude::Resource)]
pub struct RayScene {
    device: Device,
    accel: ash::khr::acceleration_structure::Device,
    properties: vk::PhysicalDeviceAccelerationStructurePropertiesKHR<'static>,
    support: RtSupport,
    blas: BlasRegistry,
    tlas: TlasFrame,
}

/// BRP `rt_stats` 与施工记录同口径的读数。
#[derive(Clone, Copy, Debug, Default)]
pub struct RaySceneStats {
    pub blas_count: usize,
    pub blas_pending: usize,
    pub blas_bytes: u64,
    /// 地址表/实例数组用的帧槽数（每帧槽一份 TLAS）。
    pub tlas_capacity: u32,
    pub tlas_bytes: u64,
    pub blas_generation: u64,
}

impl RayScene {
    /// RT 支持则建设备侧容器（初始容量 TLAS + 空 BLAS 表），否则 None。
    ///
    /// # Errors
    /// 属性缺席（能力门通过但属性为空，状态矛盾）或 TLAS 初始创建失败。
    pub fn new(ctx: &Context) -> Result<Option<Self>> {
        let support = ctx.rt_support();
        if !support.enabled() {
            return Ok(None);
        }
        let Some(properties) = ctx.rt_properties() else {
            return Err(crate::common::error::VulkanError::Contract(
                "RT 能力门已开但加速结构属性缺席——建设备时查询链断裂".into(),
            ));
        };
        let accel = ash::khr::acceleration_structure::Device::new(&ctx.instance, &ctx.device);
        let tlas = TlasFrame::new(ctx, &accel, properties)?;
        let blas = BlasRegistry::new(&ctx.device, &accel);
        Ok(Some(Self {
            device: ctx.device.clone(),
            accel,
            properties,
            support,
            blas,
            tlas,
        }))
    }

    #[must_use]
    pub fn support(&self) -> RtSupport {
        self.support
    }

    /// 排建一份 BLAS（幂等）。构建输入是 `MeshPool` 的区间，地址每次现取（扩容后会变）。
    ///
    /// # Errors
    /// 见 [`BlasRegistry::ensure_build`]。
    pub fn ensure_blas(
        &mut self,
        ctx: &Context,
        id: AssetId<Mesh>,
        slot: &MeshSlot,
        vertex_buffer: vk::Buffer,
        index_buffer: vk::Buffer,
    ) -> Result<bool> {
        self.blas
            .ensure_build(ctx, self.properties, id, slot, vertex_buffer, index_buffer)
    }

    /// 排销毁一份 BLAS（资产退场/换内容）。
    pub fn retire_blas(&mut self, id: AssetId<Mesh>) -> bool {
        self.blas.retire(id)
    }

    /// 录本帧全部待建 BLAS + 其后屏障（构建写 → 后续 TLAS 构建/ray query 读）。
    /// `vertex_base`/`index_base` 是本帧网格池的设备基址（池可能扩容迁移，必须现取）。
    /// 返回本次录制的条数（0 = 没录命令）。
    pub fn record_blas_builds(
        &mut self,
        cb: vk::CommandBuffer,
        vertex_base: vk::DeviceAddress,
        index_base: vk::DeviceAddress,
    ) -> usize {
        let count = self.blas.record_builds(cb, vertex_base, index_base);
        if count > 0 {
            // 构建写 → 后续 TLAS 构建/ray query 读
            unsafe {
                MemoryBarrierDesc {
                    src_stage: vk::PipelineStageFlags::ACCELERATION_STRUCTURE_BUILD_KHR,
                    src_access: vk::AccessFlags::ACCELERATION_STRUCTURE_WRITE_KHR,
                    dst_stage: vk::PipelineStageFlags::ACCELERATION_STRUCTURE_BUILD_KHR
                        | vk::PipelineStageFlags::COMPUTE_SHADER,
                    dst_access: vk::AccessFlags::ACCELERATION_STRUCTURE_READ_KHR
                        | vk::AccessFlags::SHADER_READ,
                }
                .record(&self.device, cb);
            }
        }
        count
    }

    /// 停尸房排空（等帧侧 + 拆 AS/scratch）。与上传链同一次等待点。
    ///
    /// # Errors
    /// `wait` 失败。
    pub fn drain(&mut self, wait: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        self.blas.drain(wait)
    }

    /// 已建成 BLAS 地址（None = 未建/待建，地址表写 0）。
    #[must_use]
    pub fn blas_address(&self, id: AssetId<Mesh>) -> Option<vk::DeviceAddress> {
        self.blas.address(id)
    }

    /// 建成/销毁计数（地址表整表重写判据）。
    #[must_use]
    pub fn blas_generation(&self) -> u64 {
        self.blas.generation()
    }

    /// 本帧 TLAS 容量保证（不够时等全部在飞帧后重建）。
    ///
    /// # Errors
    /// 重建失败。
    pub fn ensure_tlas_capacity(
        &mut self,
        ctx: &Context,
        needed: u32,
        wait: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        self.tlas
            .ensure_capacity(ctx, self.properties, needed, wait)
    }

    /// 第 `slot` 帧槽 TLAS 句柄（RT 描述符集 b0 用）。
    #[must_use]
    pub fn tlas_handle(&self, slot: usize) -> vk::AccelerationStructureKHR {
        self.tlas.handle(slot)
    }

    /// 录 TLAS 构建命令（含前后屏障）。实例数组地址由调用方从 `InstanceTables` 现取。
    pub fn record_tlas_build(
        &self,
        cb: vk::CommandBuffer,
        slot: usize,
        instance_buffer_address: vk::DeviceAddress,
        instance_count: u32,
    ) {
        self.tlas
            .record_build(cb, slot, instance_buffer_address, instance_count);
    }

    /// ACCELERATION_STRUCTURE 描述符写入要走 ash 的 loader，这里借出（passes/rt 建集用）。
    #[must_use]
    pub fn accel_loader(&self) -> &ash::khr::acceleration_structure::Device {
        &self.accel
    }

    /// 录制层按职责拆借：BLAS 注册表要 `&mut`（待建条目转正），TLAS 只读（录构建命令）。
    /// 两个字段不相交，借期覆盖整帧录制。
    pub fn split(&mut self) -> (&mut BlasRegistry, &TlasFrame) {
        (&mut self.blas, &self.tlas)
    }

    #[must_use]
    pub fn blas(&self) -> &BlasRegistry {
        &self.blas
    }

    #[must_use]
    pub fn stats(&self) -> RaySceneStats {
        RaySceneStats {
            blas_count: self.blas.count(),
            blas_pending: self.blas.pending_count(),
            blas_bytes: self.blas.bytes(),
            tlas_capacity: self.tlas.capacity(0),
            tlas_bytes: self.tlas.bytes(),
            blas_generation: self.blas.generation(),
        }
    }
}
