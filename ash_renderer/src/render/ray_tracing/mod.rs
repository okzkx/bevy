//! 求交器（9.4）：硬件 inline ray query 主后端。宿主侧只暴露 [`RayScene`]——BLAS 注册表
//! 与每帧 TLAS 的构建/更新/查询入口都在它后面，`vk::AccelerationStructureKHR` 这类硬件
//! 类型不出本模块（描述符集与录制层只拿句柄束）。WGSL 侧求交面收在
//! `shaders/common/raytracing.wgsl`，换后端 = 换那份片段 + 换本目录的后端模块。
//!
//! 生命周期：BLAS 挂 mesh 资产身份（commit 排建 / replace 重建 / retire 排销毁，驻留事件
//! 驱动，见 `blas.rs`）；TLAS 每帧从在场环全量重建（`tlas.rs`）。设备不支持 光线求交时
//! [`RayScene::new`] 返回 None，整个模块不实例化。

mod blas;
mod tlas;

use ash::{vk, Device};
use bevy::asset::AssetId;
use bevy::mesh::Mesh;

use crate::common::error::Result;

use crate::render::core::device::context::Context;
use crate::render::core::device::ray_tracing::RayTracingSupport;
use crate::render::core::record::barrier::MemoryBarrierDesc;
use crate::render::resident::assets::pool::MeshSlot;

pub use blas::BlasRegistry;
pub use tlas::TlasFrame;

/// 光线求交总开关（BRP `ray_tracing_set`）：feature 门之上再一层，供 A/B 与降级验证。`false` 时
/// 一切 光线求交命令不录、调试图不写——画面与 9.3 收官态逐位一致。
#[derive(bevy::prelude::Resource, Clone, Copy, Debug, Default)]
pub struct RayTracingSettings {
    /// 总开关（init 按能力门给初值；BRP ray_tracing_set 覆写）。
    pub enabled: bool,
    /// 调试图子视图：0 = 命中距离 t，1 = 世界法线，2 = 实例槽 hash（`ray_tracing_dump` 按它解码）。
    pub view: u32,
    /// 调试图开关（默认 false）：只有它为 true 时才录全屏调试图；BRP `ray_tracing_set` 显式打开，
    /// 取证后关回。TLAS 仍随 `enabled` 运行，`debug` 只门控调试图 dispatch。
    pub debug: bool,
}

/// 求交器宿主入口：BLAS 注册表 + 每帧 TLAS + 设备能力/属性快照。
#[derive(bevy::prelude::Resource)]
pub struct RayScene {
    device: Device,
    accel: ash::khr::acceleration_structure::Device,
    properties: vk::PhysicalDeviceAccelerationStructurePropertiesKHR<'static>,
    support: RayTracingSupport,
    blas: BlasRegistry,
    tlas: TlasFrame,
}

/// BRP `ray_tracing_stats` 与施工记录同口径的读数。
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
    /// 光线求交支持则建设备侧容器（初始容量 TLAS + 空 BLAS 表），否则 None。
    ///
    /// # Errors
    /// 属性缺席（能力门通过但属性为空，状态矛盾）或 TLAS 初始创建失败。
    pub fn new(ctx: &Context) -> Result<Option<Self>> {
        let support = ctx.ray_tracing_support();
        if !support.enabled() {
            return Ok(None);
        }
        let Some(properties) = ctx.ray_tracing_properties() else {
            return Err(crate::common::error::VulkanError::Contract(
                "光线求交能力门已开但加速结构属性缺席——建设备时查询链断裂".into(),
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
    pub fn support(&self) -> RayTracingSupport {
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

    /// 第 `slot` 帧槽 TLAS 句柄（光线求交描述符集 b0 用）。
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

    /// ACCELERATION_STRUCTURE 描述符写入要走 ash 的 loader，这里借出（passes/ray_tracing 建集用）。
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

#[cfg(test)]
mod tests {
    use bevy::math::{Vec2, Vec3};

    /// Pin the shader contract as well as asymmetric examples: centroid-only checks cannot
    /// detect a cyclic permutation of the Vulkan vertex weights.
    #[test]
    fn ray_hit_attributes_use_vulkan_vertex_weights() {
        let source = include_str!("../../../shaders/common/raytracing.wgsl");
        for expression in [
            "g.position = p0 * w + p1 * bary.x + p2 * bary.y;",
            "g.normal = n0 * w + n1 * bary.x + n2 * bary.y;",
            "g.uv = uv0 * w + uv1 * bary.x + uv2 * bary.y;",
        ] {
            assert!(
                source.contains(expression),
                "shader contract changed: {expression}"
            );
        }
        let vertices = [
            Vec3::new(1.0, 2.0, 0.0),
            Vec3::new(9.0, 2.0, 0.0),
            Vec3::new(1.0, 8.0, 0.0),
        ];
        for (bary, expected) in [
            (Vec2::ZERO, vertices[0]),
            (Vec2::X, vertices[1]),
            (Vec2::Y, vertices[2]),
            (Vec2::new(0.2, 0.3), Vec3::new(2.6, 3.8, 0.0)),
        ] {
            let reconstructed =
                vertices[0] * (1.0 - bary.x - bary.y) + vertices[1] * bary.x + vertices[2] * bary.y;
            assert!((reconstructed - expected).length() < 1e-5);
        }
    }

    #[test]
    fn skipped_estimation_preserves_both_ping_pong_versions() {
        let source = include_str!("../../../shaders/scene/gi/estimation.wgsl");
        let carry = source
            .find("patch_irradiance1[patch_idx] = patch_irradiance0[patch_idx];")
            .expect("active patches must carry the previous value");
        let skip = source
            .find("let stride = max(gi.estimation_l.z, 1u);")
            .expect("budget stride branch");
        assert!(carry < skip, "carry-forward must precede the early return");
        for stride in [2, 4] {
            let mut values = [1.0f32; 2];
            let mut latest = 1.0;
            for frame in 0..32usize {
                let write = frame & 1;
                values[write] = values[1 - write];
                if frame % stride == 0 {
                    values[write] += 1.0;
                    latest += 1.0;
                }
                assert_eq!(values[write], latest, "stride {stride}, frame {frame}");
            }
        }
    }

    #[test]
    fn history_frame_identity_uses_exact_finite_components() {
        let source = include_str!("../../../shaders/scene/gi/temporal_filter.wgsl");
        assert!(source.contains("previous.l1x.w == f32(expected_frame >> 16u) + 1.0"));
        let stamp = |frame: u32| [(frame & 0xFFFF) as f32 + 1.0, (frame >> 16) as f32 + 1.0];
        for frame in [0, 1, 65535, 65536, (1 << 23) - 1, 1 << 23, u32::MAX] {
            let [low, high] = stamp(frame);
            assert!(low.is_finite() && high.is_finite() && low > 0.0 && high > 0.0);
            assert_eq!((low - 1.0) as u32 | (((high - 1.0) as u32) << 16), frame);
        }
        assert_ne!(stamp(0), stamp(1 << 23));
    }

    #[test]
    fn gi_restart_cannot_reuse_frozen_counter_history() {
        let source = include_str!("../../driver/host/frame/gi.rs");
        let capture = source
            .find("let history_contiguous = local.was_enabled;")
            .unwrap();
        let enable = source.find("local.was_enabled = true;").unwrap();
        assert!(capture < enable);
        assert!(source.contains("settings.temporal_filter && history_contiguous"));
        for (requested, previous_gi, consecutive_stamp, expected) in [
            (true, false, true, false),
            (true, true, true, true),
            (true, true, false, false),
            (false, true, true, false),
        ] {
            assert_eq!(requested && previous_gi && consecutive_stamp, expected);
        }
    }

    #[test]
    fn viewport_border_normals_use_valid_inward_differences() {
        let source = include_str!("../../../shaders/common/gi_cell.wgsl");
        for guard in [
            "uv.x >= texel.x && uv.x <= 1.0 - texel.x",
            "uv.x < texel.x",
            "uv.x > 1.0 - texel.x",
            "uv.y < texel.y",
            "uv.y > 1.0 - texel.y",
            "dot(cross_normal, cross_normal) < 1e-24",
        ] {
            assert!(
                source.contains(guard),
                "missing border/degeneracy guard: {guard}"
            );
        }
        for (t, b) in [
            (Vec3::X, -Vec3::Y),
            (Vec3::X * 2.0, -Vec3::Y),
            (Vec3::X, -Vec3::Y * 2.0),
        ] {
            assert_eq!(t.cross(b).normalize(), -Vec3::Z);
        }
        assert_eq!(Vec3::X.cross(Vec3::X), Vec3::ZERO);
    }

    #[test]
    fn recent_visible_patches_survive_all_seed_phases() {
        let source = include_str!("../../../shaders/scene/gi/eviction.wgsl");
        let age_gate = source.find("if (age <= gi.frame.y)").expect("age gate");
        let demotion = source
            .find("if (gi.frame.z != 0u)")
            .expect("bounce demotion");
        assert!(age_gate < demotion, "rank changes must follow expiration");
        let evict = |frame: u32, heartbeat: u32, rank: u8, bounce: bool| {
            let distance = frame.wrapping_sub(heartbeat) & 0xFFFF;
            let age = distance.min(65536 - distance);
            if age <= 240 {
                (Some(rank), heartbeat)
            } else if bounce && rank == 0 {
                (Some(1), frame & 0xFFFF)
            } else {
                (None, heartbeat)
            }
        };
        let mut heartbeat = 0;
        for frame in 1..=64 {
            assert_eq!(evict(frame, heartbeat, 0, true).0, Some(0));
            if frame % 16 == 0 {
                heartbeat = frame;
            }
        }
        assert_eq!(evict(241, 0, 0, true), (Some(1), 241));
        assert_eq!(evict(481, 241, 1, true).0, Some(1));
        assert_eq!(evict(482, 241, 1, true).0, None);
        assert_eq!(evict(241, 0, 0, false).0, None);
        assert_eq!(evict(5, 65530, 0, true).0, Some(0));
    }

    #[test]
    fn screen_guide_identity_is_independent_of_sample_phase() {
        let source = include_str!("../../../shaders/scene/gi/screen_gather.wgsl");
        assert!(
            source.contains("var guide_pixel = min(low_to_full(low, vec2u(1u, 1u)), last_pixel);")
        );
        assert!(source.contains("guide_offset = pixel - low * 4u;"));
        assert!(!source.contains("let reference = gather_one(seed_pixel"));
        // 参考几何即覆盖率来源：必须在有效几何之后按固定次序找"有缓存"的子像素。
        assert!(source.contains("if (reference.surface_valid && !reference.usable)"));
        assert!(source.contains("if (candidate.surface_valid && candidate.usable)"));
        // 覆盖率不得再回到抖动样本的加权占比（那会随采样相位在 0.005 与 1.0 间整块翻转）。
        assert!(!source.contains("covered_weight"));
        assert!(!source.contains("attempted_weight"));
        let choose = |valid: [bool; 16]| {
            if valid[5] {
                Some(5)
            } else {
                valid.iter().position(|present| *present)
            }
        };
        for frame in 0..64 {
            let mut valid = [false; 16];
            valid[5] = true;
            valid[(frame * 7) % 16] = true;
            assert_eq!(choose(valid), Some(5));
            valid[5] = false;
            valid[9] = true;
            valid[2] = true;
            assert!(choose(valid).is_some_and(|phase| phase <= 2));
        }
        assert_eq!(choose([false; 16]), None);
        for full in 1u32..=12 {
            let low = (full - 1) / 4;
            let pixel = (low * 4 + 1).min(full - 1);
            assert!(pixel < full && pixel - low * 4 < 4);
        }
    }

    #[test]
    fn screen_gather_color_does_not_depend_on_sample_phase() {
        let source = include_str!("../../../shaders/scene/gi/screen_gather.wgsl");
        // 累加必须以确定性参考样本打底，抖动样本只作细化：否则样本全落在未估计的格子上时
        // weight_sum = 0，块被写成纯黑而覆盖率仍是 1，合成端在正常 E 与黑洞之间闪。
        assert!(source.contains("var acc_l0 = reference.l0;"));
        assert!(source.contains("var weight_sum = 1.0;"));
        assert!(!source.contains("var weight_sum = 0.0;"));
        assert!(source.contains("let coverage = select(0.0, 1.0, reference.usable);"));
        // 未估计过的参考不能注入（退回 IBL），否则会用播种值冒充辐照度。
        assert!(!source.contains("let coverage = 1.0;"));
    }

    #[test]
    fn patch_allocation_epoch_is_unique_per_frame() {
        let seed = include_str!("../../../shaders/scene/gi/patch_alloc.wgsl");
        let bounce = include_str!("../../../shaders/scene/gi/bounce_alloc.wgsl");
        // 帧号 | 1 会让相邻的偶数帧与奇数帧算出同一个纪元标签：奇数帧的分配全被判成
        // "同帧已有人抢到"而跳过，新 cell 只能隔帧建立，被回收的 cell 隔帧缺映射。
        assert!(!seed.contains("push.frame_index | 1u"));
        assert!(!bounce.contains("gi.frame.x | 1u"));
        assert!(seed.contains("let epoch = push.frame_index * 2u + 1u;"));
        assert!(bounce.contains("let epoch = gi.frame.x * 2u + 1u;"));
        // 标签必须每帧不同，且永远不为 0（0 是"无标签"）。
        let mut previous = 0u32;
        for frame in 0u32..4096 {
            let epoch = frame.wrapping_mul(2).wrapping_add(1);
            assert_ne!(epoch, 0);
            assert_ne!(epoch, previous);
            assert_eq!(epoch & 1, 1);
            previous = epoch;
        }
    }

    #[test]
    fn truncated_sh_negative_lobes_do_not_transport_negative_energy() {
        let compression = include_str!("../../../shaders/common/gi_compression.wgsl");
        let estimation = include_str!("../../../shaders/scene/gi/estimation.wgsl");
        assert!(
            compression.contains("return max(l0 + l1x * n.x + l1y * n.y + l1z * n.z, vec3f(0.0));")
        );
        assert!(estimation.contains("return max(lookup_eval(previous, n), vec3f(0.0));"));
        let l0 = Vec3::new(1.0, 2.0, 3.0);
        let l1 = l0 * 2.0;
        assert_eq!((l0 - l1).max(Vec3::ZERO), Vec3::ZERO);
        assert_eq!((l0 + l1).max(Vec3::ZERO), l0 * 3.0);
    }
}
