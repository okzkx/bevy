//! 3.3.1 贴图链路探针:格式映射 → 上传闭环(布局链) → 跨族所有权 → 负例诊断,四组全链证据。
//!
//! 与 upload_probe 同纪律:各组是"观察 + 诊断",验证层 + 同步验证常开,VUID 收账:
//! - 组 A(映射观察,纯 CPU):bevy `Image` → [`ImageSpec`] 的角色核对——Rgba8UnormSrgb
//!   → `R8G8B8A8_SRGB`(sRGB 解码角色)、Rgba8Unorm → `R8G8B8A8_UNORM`(数据贴图线性);
//!   `ImageSampler::Default` 按官方 ImagePlugin 全局默认解析为 linear;四种拒绝
//!   (维度/mip 链/格式/无数据)按 3.3.1 口径各验一遍。
//! - 组 B(上传闭环观察):两张 4×4 贴图走真实交付路径(GpuImage + Uploader 本体,
//!   非探针复制品)——staging → `vkCmdCopyBufferToImage`(2D mip0 单层紧 packing),
//!   前后置布局屏障(UNDEFINED → TRANSFER_DST → SHADER_READ_ONLY)在 uploader 图像段;
//!   timeline 票据 CPU 等待后,图像 → readback 拷贝(SHADER_READ_ONLY → TRANSFER_SRC
//!   迁入 + 拷贝 + 迁回)逐字节比对。同族读回形状也是"无专用 transfer 族"设备的
//!   回退形状。
//! - 组 C(跨族所有权观察,3.3.3 接线预演):上传批带 image_releases(transfer →
//!   graphics,release 与迁出布局合成一条屏障),graphics 提交侧 GPU 等票据 + acquire
//!   + 真实读(图形队列上的 copyImageToBuffer);EXCLUSIVE 成对语义全链跑通。
//! - 组 D(负例诊断,record-only):绕过前置屏障直接对 UNDEFINED 布局的图发拷贝,
//!   同步验证必须执法——证明验证层在盯图像布局,目标路径的"零 VUID"不是静默。
//!
//! 自含 Entry → Instance → PhysicalDevice → Device 最小链(不碰窗口);接口定案沿
//! 3.2:旧 SubmitInfo + TimelineSemaphoreSubmitInfo + 旧 pipeline barrier,不启用
//! synchronization2。销毁纪律:各组先等票到 + 宿主读完,再拆——无在途引用可证明。

use std::sync::Mutex;

use ash::{ext::debug_utils, vk, Device, Entry};
use ash_renderer::vulkan::{
    image_spec, BufferRole, GpuBuffer, GpuImage, MemoryContract, StagingImageCopy, UploadBatch,
    Uploader,
};
use bevy::asset::RenderAssetUsages;
use bevy::image::{Image, ImageSampler, ImageSamplerDescriptor};
use wgpu_types::{Extent3d, TextureDimension, TextureFormat};

const VALIDATION_LAYER: &std::ffi::CStr = c"VK_LAYER_KHRONOS_validation";

/// 验证层消息收账:WARNING/ERROR 全记,组间分账、探针结束统一打印。
static VU_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 取走当前积累的消息(组间分账)。
fn take_vu() -> Vec<String> {
    std::mem::take(&mut VU_LOG.lock().unwrap_or_else(|p| p.into_inner()))
}

fn main() {
    let entry = unsafe { Entry::load() }.expect("加载 Vulkan loader");

    let validation_installed = unsafe { entry.enumerate_instance_layer_properties() }
        .unwrap_or_default()
        .iter()
        .any(|props| unsafe {
            std::ffi::CStr::from_ptr(props.layer_name.as_ptr()) == VALIDATION_LAYER
        });
    println!(
        "验证层 {VALIDATION_LAYER:?}: {}",
        if validation_installed {
            "已安装,本探针请求启用 + 同步验证(VUID 收账见结尾)"
        } else {
            "未安装(本机缺 Vulkan SDK),本轮裸奔——结果不构成验证证据"
        }
    );

    let app_info = vk::ApplicationInfo::default()
        .application_name(c"image_probe")
        .api_version(vk::API_VERSION_1_3);
    let ext_names: Vec<*const std::ffi::c_char> = if validation_installed {
        vec![debug_utils::NAME.as_ptr()]
    } else {
        Vec::new()
    };
    let layer_names: Vec<*const std::ffi::c_char> = if validation_installed {
        vec![VALIDATION_LAYER.as_ptr()]
    } else {
        Vec::new()
    };
    let sync_enables = if validation_installed {
        Some([vk::ValidationFeatureEnableEXT::SYNCHRONIZATION_VALIDATION])
    } else {
        None
    };
    let mut sync_validation = vk::ValidationFeaturesEXT::default();
    let mut instance_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_extension_names(&ext_names)
        .enabled_layer_names(&layer_names);
    if let Some(enables) = &sync_enables {
        sync_validation = sync_validation.enabled_validation_features(enables);
        instance_info = instance_info.push_next(&mut sync_validation);
    }
    let instance =
        unsafe { entry.create_instance(&instance_info, None) }.expect("vkCreateInstance");

    let _debug = validation_installed.then(|| {
        let loader = debug_utils::Instance::new(&entry, &instance);
        let messenger_info = vk::DebugUtilsMessengerCreateInfoEXT::default()
            .message_severity(
                vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                    | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR,
            )
            .message_type(
                vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                    | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                    | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
            )
            .pfn_user_callback(Some(probe_callback));
        unsafe { loader.create_debug_utils_messenger(&messenger_info, None) }
            .expect("创建验证 messenger")
    });

    let pds = unsafe { instance.enumerate_physical_devices() }.expect("枚举物理设备");
    let pd = *pds
        .iter()
        .find(|pd| unsafe { instance.get_physical_device_properties(**pd) }.api_version
            >= vk::API_VERSION_1_3)
        .expect("没有报支持 Vulkan 1.3 的物理设备");
    let props = unsafe { instance.get_physical_device_properties(pd) };
    let dev_name =
        unsafe { std::ffi::CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy();
    println!(
        "设备 {dev_name}: API v{}.{}.{}(过 1.3 基线)",
        vk::api_version_major(props.api_version),
        vk::api_version_minor(props.api_version),
        vk::api_version_patch(props.api_version),
    );

    // 设备支持证据:两个目标格式的 optimal tiling features(三用途逐一核对)
    for format in [vk::Format::R8G8B8A8_SRGB, vk::Format::R8G8B8A8_UNORM] {
        let features =
            unsafe { instance.get_physical_device_format_properties(pd, format) }
                .optimal_tiling_features;
        let need = vk::FormatFeatureFlags::TRANSFER_DST
            | vk::FormatFeatureFlags::TRANSFER_SRC
            | vk::FormatFeatureFlags::SAMPLED_IMAGE;
        println!(
            "格式支持 {format:?}: optimal {features:?} → 三用途全含 = {}",
            features.contains(need)
        );
    }

    group_a_mapping();

    let (device, gfx_family, gfx_queue, transfer_family, transfer_queue) =
        create_device(&instance, pd);
    let contract = unsafe { MemoryContract::new(&instance, pd) };
    // 设备侧四件的参数束:三个组的公共前缀,收掉逐参数传递
    let ctx = ProbeCtx {
        instance: &instance,
        pd,
        device: &device,
        contract: &contract,
    };
    let cross_family = transfer_family != gfx_family;
    println!(
        "贴图链参数:graphics 族 {gfx_family},transfer 族 {transfer_family}({})",
        if cross_family {
            "专用 transfer 族,跨族分支可实测"
        } else {
            "同族回退(无专用 transfer 族),跨族组按回退口径注明"
        }
    );

    group_b_upload_closure(&ctx, transfer_family, transfer_queue);
    if cross_family {
        group_c_cross_family(&ctx, gfx_family, gfx_queue, transfer_family, transfer_queue);
    } else {
        println!("\n== 组 C(跨族所有权观察):本机无专用 transfer 族,EXCLUSIVE release/acquire 路径无硬件载体——");
        println!("   回退路径未实测(诚实注明);同族形状已由组 B 在验证层下零 VUID 覆盖。");
    }
    group_d_negative(&ctx, transfer_family, transfer_queue);

    unsafe { device.destroy_device(None) };

    if let Some(messenger) = _debug {
        let loader = debug_utils::Instance::new(&entry, &instance);
        unsafe { loader.destroy_debug_utils_messenger(messenger, None) };
    }
    unsafe { instance.destroy_instance(None) };

    println!("\n== 验证层消息收账(全程)==");
    let vu = take_vu();
    if vu.is_empty() {
        println!("  (零条——各组已在自身收账行分账:组 A/B/C 目标路径清净,组 D 负例一条已入档)");
    } else {
        for m in vu.iter() {
            println!("  {m}");
        }
    }
    println!("\n结论口径:组 A=映射逐项;组 B=上传闭环 + 布局链 + 读回逐字节;");
    println!("组 C=跨族 release/acquire 全链(本机具备专用 transfer 族时);组 D=负例执法实证(预期消息)。");
}

// ============ 合成贴图(4×4,字节已知)============

/// sRGB 角色样张:每 texel (r=i%4*85, g=(i/4)*85, b=200, a=255)——64B,可读梯度。
fn srgb_bytes() -> Vec<u8> {
    (0..16u32)
        .flat_map(|i| [(i % 4) as u8 * 85, (i / 4) as u8 * 85, 200, 255])
        .collect()
}

/// 线性角色样张:每 texel (10, 20, 30, 255) 恒值——64B,读回比对锚点。
fn unorm_bytes() -> Vec<u8> {
    (0..16u32).flat_map(|_| [10u8, 20, 30, 255]).collect()
}

fn bevy_image(format: TextureFormat, bytes: Vec<u8>) -> Image {
    Image::new(
        Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        bytes,
        format,
        RenderAssetUsages::MAIN_WORLD,
    )
}

// ============ 组 A:映射观察(纯 CPU)============

fn group_a_mapping() {
    println!("\n== 组 A(映射观察:格式角色 + 采样解析 + 拒绝策略)==");

    // sRGB / 线性双角色映射
    let spec_srgb =
        image_spec(&bevy_image(TextureFormat::Rgba8UnormSrgb, srgb_bytes())).expect("sRGB 映射");
    assert_eq!(spec_srgb.format, vk::Format::R8G8B8A8_SRGB);
    assert!(spec_srgb.srgb_role(), "Rgba8UnormSrgb 应承担 sRGB 解码角色");
    assert_eq!((spec_srgb.width, spec_srgb.height), (4, 4));
    let spec_linear =
        image_spec(&bevy_image(TextureFormat::Rgba8Unorm, unorm_bytes())).expect("线性映射");
    assert_eq!(spec_linear.format, vk::Format::R8G8B8A8_UNORM);
    assert!(!spec_linear.srgb_role(), "Rgba8Unorm 应保持线性角色");
    println!("[映射] Rgba8UnormSrgb → R8G8B8A8_SRGB(sRGB 角色)✓  Rgba8Unorm → R8G8B8A8_UNORM(线性)✓");

    // 采样解析:Default → 官方 ImagePlugin 全局默认 linear();显式描述符透传
    let mut default_img = bevy_image(TextureFormat::Rgba8Unorm, unorm_bytes());
    default_img.sampler = ImageSampler::Default;
    let spec = image_spec(&default_img).expect("Default 采样映射");
    assert_eq!(spec.sampler.mag_filter, bevy::image::ImageFilterMode::Linear);
    assert_eq!(spec.sampler.min_filter, bevy::image::ImageFilterMode::Linear);
    assert_eq!(
        spec.sampler.address_mode_u,
        bevy::image::ImageAddressMode::ClampToEdge
    );
    let mut nearest_img = bevy_image(TextureFormat::Rgba8Unorm, unorm_bytes());
    nearest_img.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor::nearest());
    let spec = image_spec(&nearest_img).expect("Nearest 描述符映射");
    assert_eq!(
        spec.sampler.mag_filter,
        bevy::image::ImageFilterMode::Nearest
    );
    println!("[采样] Default → linear() + ClampToEdge(官方默认出处)✓  Descriptor(nearest) 透传 ✓");

    // 拒绝四态:无数据 / 维度 / mip 链 / 格式
    let uninit = Image::new_uninit(
        Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::MAIN_WORLD,
    );
    let e = image_spec(&uninit).unwrap_err();
    assert!(matches!(
        e,
        ash_renderer::vulkan::ImageConvertError::MissingData
    ));
    let d1 = Image::new(
        Extent3d {
            width: 4,
            height: 1,
            depth_or_array_layers: 1,
        },
        TextureDimension::D1,
        vec![0u8; 16],
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::MAIN_WORLD,
    );
    let e = image_spec(&d1).unwrap_err();
    assert!(matches!(
        e,
        ash_renderer::vulkan::ImageConvertError::UnsupportedDimension(_)
    ));
    let mut mipped = bevy_image(TextureFormat::Rgba8Unorm, unorm_bytes());
    mipped.texture_descriptor.mip_level_count = 3;
    let e = image_spec(&mipped).unwrap_err();
    assert!(matches!(
        e,
        ash_renderer::vulkan::ImageConvertError::UnsupportedMipLevels(3)
    ));
    let hdr = Image::new(
        Extent3d {
            width: 4,
            height: 4,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        vec![0u8; 128],
        TextureFormat::Rgba16Float,
        RenderAssetUsages::MAIN_WORLD,
    );
    let e = image_spec(&hdr).unwrap_err();
    assert!(matches!(
        e,
        ash_renderer::vulkan::ImageConvertError::UnsupportedFormat(_)
    ));
    println!("[拒绝] 无数据 ✓  D1 ✓  mip 链 3 ✓  Rgba16Float ✓(Tier①:warn 一次跳过,帧循环照常)");

    assert!(take_vu().is_empty(), "组 A 纯 CPU 组不应有任何验证层消息");
    println!("[收账] 组 A 零 VUID");
}

// ============ 设备侧参数束 ============

/// 设备侧四件的公共前缀(instance/pd 建图查特性、device 建资源、contract 选内存)。
struct ProbeCtx<'a> {
    instance: &'a ash::Instance,
    pd: vk::PhysicalDevice,
    device: &'a Device,
    contract: &'a MemoryContract,
}

// ============ 组 B:上传闭环 + 同族读回 ============

fn group_b_upload_closure(
    ctx: &ProbeCtx,
    transfer_family: u32,
    transfer_queue: vk::Queue,
) {
    let ProbeCtx {
        instance,
        pd,
        device,
        contract,
    } = *ctx;
    println!("\n== 组 B(上传闭环观察:staging → copyBufferToImage → 布局链 → 读回逐字节)==");
    let mut uploader =
        Uploader::new(device, contract, transfer_family, transfer_queue, 64 * 1024, 2)
            .expect("组 B 创建上传器");

    let expect_srgb = srgb_bytes();
    let expect_unorm = unorm_bytes();
    let spec_srgb =
        image_spec(&bevy_image(TextureFormat::Rgba8UnormSrgb, expect_srgb.clone()))
            .expect("sRGB 映射");
    let spec_unorm = image_spec(&bevy_image(TextureFormat::Rgba8Unorm, expect_unorm.clone()))
        .expect("线性映射");
    let gpu_srgb =
        GpuImage::create(device, instance, pd, contract, &spec_srgb, &[]).expect("sRGB 建图");
    let gpu_unorm =
        GpuImage::create(device, instance, pd, contract, &spec_unorm, &[]).expect("线性建图");

    // 一批两图:staging 连续排布(64B + 64B),uploader 图像段完成迁移与拷贝
    let mut staging = expect_srgb.clone();
    staging.extend_from_slice(&expect_unorm);
    let ticket = uploader
        .submit_batch(UploadBatch {
            staging,
            image_uploads: vec![
                StagingImageCopy {
                    image: gpu_srgb.image(),
                    src_offset: 0,
                    width: 4,
                    height: 4,
                },
                StagingImageCopy {
                    image: gpu_unorm.image(),
                    src_offset: 64,
                    width: 4,
                    height: 4,
                },
            ],
            ..Default::default()
        })
        .expect("组 B transfer 提交")
        .expect("组 B 批必有票据");
    println!("[上传] 批 #{ticket}:两图进显存(UNDEFINED→TRANSFER_DST→SHADER_READ_ONLY 在 transfer 提交内)");
    uploader.wait_until(ticket).expect("等票据");

    // 读回:image → buffer(SHADER_READ_ONLY → TRANSFER_SRC 迁入,拷贝,迁回)。
    // 这也是"无专用 transfer 族"设备的同族回退形状:消费在同族队列,无所有权转移。
    let readback =
        GpuBuffer::create(device, contract, 128, BufferRole::Readback).expect("readback");
    let (pool, cb) = alloc_commands(device, transfer_family);
    unsafe {
        device
            .begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .expect("B begin");
        let to_src = |img: vk::Image| {
            vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(mip0_range())
        };
        device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_src(gpu_srgb.image()), to_src(gpu_unorm.image())],
        );
        device.cmd_copy_image_to_buffer(
            cb,
            gpu_srgb.image(),
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            readback.buffer(),
            &[copy_region(0)],
        );
        device.cmd_copy_image_to_buffer(
            cb,
            gpu_unorm.image(),
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            readback.buffer(),
            &[copy_region(64)],
        );
        let back = |img: vk::Image| {
            vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                .dst_access_mask(vk::AccessFlags::empty())
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(mip0_range())
        };
        device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[back(gpu_srgb.image()), back(gpu_unorm.image())],
        );
        device.end_command_buffer(cb).expect("B end");
        let fence = device
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .expect("B fence");
        device
            .queue_submit(
                transfer_queue,
                &[vk::SubmitInfo::default().command_buffers(&[cb])],
                fence,
            )
            .expect("B 读回提交");
        device
            .wait_for_fences(&[fence], true, 5_000_000_000)
            .expect("等读回完成");
        device.destroy_fence(fence, None);
    }
    let mut got = vec![0u8; 128];
    let mut readback = readback;
    readback.read(0, &mut got).expect("B 宿主读");
    assert_eq!(&got[..64], &expect_srgb[..], "组 B sRGB 读回不符");
    assert_eq!(&got[64..], &expect_unorm[..], "组 B 线性读回不符");
    println!("[内容] 两图读回逐字节一致——copy 引擎完成 swizzle 重排且字节无损(显存机制篇判定线 1 的双向证据)");

    uploader.wait_all_uploads().expect("B 等全部票据");
    drop(readback);
    unsafe { device.destroy_command_pool(pool, None) };
    drop(uploader);
    drop(gpu_srgb);
    drop(gpu_unorm);

    let vu = take_vu();
    assert!(vu.is_empty(), "组 B 目标路径应零 VUID,实际: {vu:?}");
    println!("[收账] 组 B 零 VUID");
}

// ============ 组 C:跨族所有权(release → graphics 等票据 + acquire + 真实读)============

fn group_c_cross_family(
    ctx: &ProbeCtx,
    gfx_family: u32,
    gfx_queue: vk::Queue,
    transfer_family: u32,
    transfer_queue: vk::Queue,
) {
    let ProbeCtx {
        instance,
        pd,
        device,
        contract,
    } = *ctx;
    println!("\n== 组 C(跨族所有权观察:release(合成迁出布局)→ graphics 等票据 + acquire + 真实读)==");
    let mut uploader =
        Uploader::new(device, contract, transfer_family, transfer_queue, 64 * 1024, 2)
            .expect("组 C 创建上传器");

    let expect = srgb_bytes();
    let spec =
        image_spec(&bevy_image(TextureFormat::Rgba8UnormSrgb, expect.clone())).expect("C 映射");
    let gpu = GpuImage::create(device, instance, pd, contract, &spec, &[]).expect("C 建图");

    // 上传批带 release:所有权 transfer → graphics,与迁出布局合成一条屏障
    let ticket = uploader
        .submit_batch(UploadBatch {
            staging: expect.clone(),
            image_uploads: vec![StagingImageCopy {
                image: gpu.image(),
                src_offset: 0,
                width: 4,
                height: 4,
            }],
            image_releases: vec![ash_renderer::vulkan::ImageRelease {
                image: gpu.image(),
                to_family: gfx_family,
            }],
            ..Default::default()
        })
        .expect("C transfer 提交")
        .expect("C 批必有票据");
    println!("[release] 批 #{ticket} 末尾把贴图所有权让渡给 graphics 族(与迁出 SHADER_READ_ONLY 合成一条屏障)");

    // graphics 侧提交:GPU 等票据 + acquire + 图形队列上的真实读
    let readback =
        GpuBuffer::create(device, contract, 64, BufferRole::Readback).expect("C readback");
    let (pool, cb) = alloc_commands(device, gfx_family);
    unsafe {
        device
            .begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .expect("C begin");
        // acquire:族号与 release 成对,old/new 与 release 声明同一对(TRANSFER_DST →
        // SHADER_READ_ONLY)——所有权转移屏障按"对"生效,转换由这一对屏障共同表达,
        // 接收侧以转换前布局为旧态重新声明(实测:声明成 new 布局会被跟踪器判为
        // "当前仍是 TRANSFER_DST"而拒)。src 阶段 ALL_COMMANDS 等 release 完成
        // (acquire 的 srcAccessMask 被规范忽略,置空)。
        let acquire = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::TRANSFER_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(transfer_family)
            .dst_queue_family_index(gfx_family)
            .image(gpu.image())
            .subresource_range(mip0_range());
        device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[acquire],
        );
        // 读回须在传输布局(copyImageToBuffer 的 srcImageLayout 不收 SHADER_READ_ONLY,
        // VUID-01397):acquire 落定的转换写之后,自族迁入 TRANSFER_SRC
        let into_src = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(gpu.image())
            .subresource_range(mip0_range());
        device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[into_src],
        );
        device.cmd_copy_image_to_buffer(
            cb,
            gpu.image(),
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            readback.buffer(),
            &[copy_region(0)],
        );
        // 读毕迁回可采样布局(读侧写已收口,src 罩住 TRANSFER_READ;dst 本提交无后续)
        let back = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::empty())
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(gpu.image())
            .subresource_range(mip0_range());
        device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[back],
        );
        let to_host = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(readback.buffer())
            .offset(0)
            .size(vk::WHOLE_SIZE);
        device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            &[to_host],
            &[],
        );
        device.end_command_buffer(cb).expect("C end");

        let wait_values = [ticket];
        let mut timeline_submit =
            vk::TimelineSemaphoreSubmitInfo::default().wait_semaphore_values(&wait_values);
        let wait_sem = [uploader.ticket_semaphore()];
        let wait_stages = [vk::PipelineStageFlags::ALL_COMMANDS];
        let fence = device
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .expect("C fence");
        device
            .queue_submit(
                gfx_queue,
                &[vk::SubmitInfo::default()
                    .wait_semaphores(&wait_sem)
                    .wait_dst_stage_mask(&wait_stages)
                    .command_buffers(&[cb])
                    .push_next(&mut timeline_submit)],
                fence,
            )
            .expect("C graphics 提交");
        device
            .wait_for_fences(&[fence], true, 5_000_000_000)
            .expect("等 graphics 消费完成");
        device.destroy_fence(fence, None);
    }
    println!("[acquire] graphics 提交:等票据 #{ticket}(GPU 侧)+ acquire + image→readback 拷贝(图形队列上的真实读)");

    let mut got = vec![0u8; 64];
    let mut readback = readback;
    readback.read(0, &mut got).expect("C 宿主读");
    assert_eq!(&got[..], &expect[..], "组 C 跨族读回不符");
    println!("[内容] 跨族读回逐字节一致——EXCLUSIVE release/acquire 成对语义全链跑通(3.3.3 接线预演)");

    uploader.wait_all_uploads().expect("C 等全部票据");
    drop(readback);
    unsafe { device.destroy_command_pool(pool, None) };
    drop(uploader);
    drop(gpu);

    let vu = take_vu();
    assert!(vu.is_empty(), "组 C 目标路径应零 VUID,实际: {vu:?}");
    println!("[收账] 组 C 零 VUID");
}

// ============ 组 D:负例诊断(record-only)============

fn group_d_negative(ctx: &ProbeCtx, transfer_family: u32, transfer_queue: vk::Queue) {
    let ProbeCtx {
        instance,
        pd,
        device,
        contract,
    } = *ctx;
    println!("\n== 组 D(负例诊断:绕过前置屏障直接拷贝 UNDEFINED 布局的图——record-only)==");
    let mut staging = GpuBuffer::create(device, contract, 64, BufferRole::Staging).expect("D staging");
    let bytes = unorm_bytes();
    staging.write(0, &bytes).expect("D staging 写");
    let spec = image_spec(&bevy_image(TextureFormat::Rgba8Unorm, bytes)).expect("D 映射");
    // 建图即用后即弃:负例图的 layout 恒 UNDEFINED,拷贝后不修复、不留用
    let gpu = GpuImage::create(device, instance, pd, contract, &spec, &[]).expect("D 建图");
    let (pool, cb) = alloc_commands(device, transfer_family);
    unsafe {
        device
            .begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .expect("D begin");
        // 无 UNDEFINED→TRANSFER_DST 前置屏障,直接对"当前布局 UNDEFINED"的图发拷贝
        device.cmd_copy_buffer_to_image(
            cb,
            staging.buffer(),
            gpu.image(),
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[copy_region(0)],
        );
        device.end_command_buffer(cb).expect("D end");
        let fence = device
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .expect("D fence");
        device
            .queue_submit(
                transfer_queue,
                &[vk::SubmitInfo::default().command_buffers(&[cb])],
                fence,
            )
            .expect("D 提交");
        device
            .wait_for_fences(&[fence], true, 5_000_000_000)
            .expect("D 等负例提交完成(执法不拦提交,只记账)");
        device.destroy_fence(fence, None);
    }
    let vu = take_vu();
    assert!(
        !vu.is_empty(),
        "组 D 预期验证层执法(布局违例),实际零消息——验证层没盯图像布局?"
    );
    println!("[执法] 验证层收账 {} 条,首条:", vu.len());
    println!("  {}", vu[0]);
    println!("[结论] 图像布局被验证层 + 同步验证盯防——目标路径的零 VUID 不是静默(组 D 图已弃)。");

    unsafe { device.destroy_command_pool(pool, None) };
    drop(gpu);
}

// ============ 公共小件 ============

/// 一个一次性命令池 + 单命令缓冲(族必须与提交目标一致)。
fn alloc_commands(device: &Device, family: u32) -> (vk::CommandPool, vk::CommandBuffer) {
    let pool = unsafe {
        device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                .queue_family_index(family),
            None,
        )
    }
    .expect("命令池");
    let cb = unsafe {
        device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )
    }
    .expect("命令缓冲")[0];
    (pool, cb)
}

/// 4×4 mip0 单层的 copyImageToBuffer/image 拷贝区域(紧 packing)。
fn copy_region(buffer_offset: u64) -> vk::BufferImageCopy {
    vk::BufferImageCopy {
        buffer_offset,
        buffer_row_length: 0,
        buffer_image_height: 0,
        image_subresource: vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .mip_level(0)
            .base_array_layer(0)
            .layer_count(1),
        image_offset: vk::Offset3D::default(),
        image_extent: vk::Extent3D {
            width: 4,
            height: 4,
            depth: 1,
        },
    }
}

/// 2D mip0 单层的 COLOR 子资源范围(图像屏障统一口径,与 view 创建一致)。
fn mip0_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

/// graphics + 专用 transfer(如有)双族设备:返回 (device, gfx_family, gfx_queue,
/// transfer_family, transfer_queue)。显式启用 timelineSemaphore(票据真实使用,
/// 支持与启用分开)。
fn create_device(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
) -> (Device, u32, vk::Queue, u32, vk::Queue) {
    let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    let gfx = families
        .iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .expect("无 graphics 族") as u32;
    let transfer = families
        .iter()
        .position(|f| {
            f.queue_flags.contains(vk::QueueFlags::TRANSFER)
                && !f.queue_flags.contains(vk::QueueFlags::GRAPHICS)
        })
        .map(|i| i as u32)
        .unwrap_or(gfx);
    let priority = [1.0f32];
    let mut queue_infos = vec![vk::DeviceQueueCreateInfo::default()
        .queue_family_index(gfx)
        .queue_priorities(&priority)];
    if transfer != gfx {
        queue_infos.push(
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(transfer)
                .queue_priorities(&priority),
        );
    }
    let mut vulkan12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
    let device = unsafe {
        instance.create_device(
            pd,
            &vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_infos)
                .push_next(&mut vulkan12),
            None,
        )
    }
    .expect("vkCreateDevice");
    let gfx_queue = unsafe { device.get_device_queue(gfx, 0) };
    let transfer_queue = if transfer == gfx {
        gfx_queue
    } else {
        unsafe { device.get_device_queue(transfer, 0) }
    };
    (device, gfx, gfx_queue, transfer, transfer_queue)
}

/// 验证层回调:消息原文入 VU_LOG(返回 FALSE = 不被截获)。
///
/// # Safety
/// 本函数不被本项目调用——由 Vulkan 实现按回调契约调用,`p_callback_data`
/// 依约定为合法指针或空;`_user_data` 未使用不触碰。
unsafe extern "system" fn probe_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _types: vk::DebugUtilsMessageTypeFlagsEXT,
    p_callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT,
    _user_data: *mut std::ffi::c_void,
) -> vk::Bool32 {
    let msg = if p_callback_data.is_null() {
        "(no data)".to_string()
    } else {
        unsafe { std::ffi::CStr::from_ptr((*p_callback_data).p_message) }
            .to_string_lossy()
            .into_owned()
    };
    VU_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(format!("[{severity:?}] {msg}"));
    vk::FALSE
}
