//! 3.3.3/3.3.4 描述符链路探针：**生产表全链实证**。与前置闸门 bindless_probe 的
//! 分工——闸门验证的是"接口能不能编译、能不能过 val、能不能在临时布局上采样"，
//! 本探针验证的是 **`BindlessTables`（生产 CPU 侧真身）**：
//!
//! - 组 A（设备与限额对账）：生产五位特性集建设备；UAB 轨五项限额实测值落账；
//!   `BindlessTables::new` 以 `TABLE_CAPACITY`(1024) 建表——限额对账在表内执行
//!   并落日志（3.3.2 判定线：对账不过 = 显式报错，不静默截断）。
//! - 组 B（发布与去重 + 耗尽负例）：生产表上发布三张测试贴图（红/绿/蓝，两张
//!   线性一张 NEAREST 采样器）——纹理槽 1/2/3 顺位、采样器按功能键去重（两张
//!   线性共享槽 1）；小容量表（容量 4）第 4 次发布 → 槽耗尽报错（不越界不截断）。
//! - 组 C（采样读回 + update-after-bind）：compute dispatch 经**生产 set0/set1**
//!   采样（push 一致路径 + storage 非一致路径），读回比对纹理选择与采样器槽位；
//!   首次提交完成**之后**改写纹理槽 3（UAB 的存在意义），二次 dispatch 读回新
//!   内容——"set 绑定后可更新"在生产表上实证。全程 VUID 收账，零告警为过。
//!
//! 自含 Entry→Instance→Device 最小链（不碰窗口），验证层 + 同步验证常开；
//! SPIR-V 走 build.rs 正式编译路径（`OUT_DIR/descriptor_probe.spv`，含 capability
//! 补丁器）——探针消费的就是 3.4 将消费的同一产物。
//!
//! set2 是探针专用 I/O（storage 进出），**不属于生产表**；采样路径 set0/set1
//! 完全是生产形状。

use std::collections::HashMap;
use std::sync::Mutex;

use ash::ext::debug_utils;
use ash::vk;
use ash::{Device, Entry};
use ash_renderer::vulkan::{
    sampler_key, BindlessTables, BufferRole, GpuBuffer, GpuImage, ImageSpec, MemoryContract,
    SamplerKey, StagingImageCopy, UploadBatch, Uploader, TABLE_CAPACITY,
};
use bevy::image::{ImageFilterMode, ImageSamplerDescriptor};
use bevy::math::Mat4;

const VALIDATION_LAYER: &std::ffi::CStr = c"VK_LAYER_KHRONOS_validation";
const PROBE_SPV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/descriptor_probe.spv"));

/// 验证层消息收账：WARNING/ERROR 全记，全程总账。
static VU_TOTAL: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 2×2 RGBA8 均色贴图像素（16B）。
fn uniform_pixels(rgba: [u8; 4]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for i in 0..4 {
        out[i * 4..i * 4 + 4].copy_from_slice(&rgba);
    }
    out
}

/// 测试贴图集：名字 → (GPU 本体, 像素)。
type TexMap = HashMap<&'static str, (GpuImage, [u8; 4])>;

/// 均色读回期望（UNORM 无转换）。
const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const GREEN: [f32; 4] = [0.0, 1.0, 0.0, 1.0];
const BLUE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

fn main() {
    let entry = unsafe { Entry::load() }.expect("加载 Vulkan loader");
    let validation_installed = unsafe { entry.enumerate_instance_layer_properties() }
        .unwrap_or_default()
        .iter()
        .any(|props| unsafe { std::ffi::CStr::from_ptr(props.layer_name.as_ptr()) } == VALIDATION_LAYER);
    println!(
        "验证层 {VALIDATION_LAYER:?}: {}",
        if validation_installed {
            "已安装，启用 + 同步验证（VUID 收账见结尾）"
        } else {
            "未安装——本轮裸奔，结果不构成验证证据"
        }
    );
    let instance = make_instance(&entry, validation_installed);
    let messenger = validation_installed.then(|| make_messenger(&entry, &instance));

    let pd = unsafe { instance.enumerate_physical_devices() }
        .expect("枚举物理设备")
        .into_iter()
        .find(|pd| unsafe { instance.get_physical_device_properties(*pd) }.api_version >= vk::API_VERSION_1_3)
        .expect("没有报支持 Vulkan 1.3 的物理设备");

    // ============ 组 A：设备特性与限额对账 ============
    println!("\n== 组 A(设备特性与限额对账)==");
    let mut queried = vk::PhysicalDeviceVulkan12Features::default();
    let mut features2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut queried);
    unsafe { instance.get_physical_device_features2(pd, &mut features2) };
    let need: [(&str, u32); 5] = [
        ("descriptor_indexing", queried.descriptor_indexing),
        ("runtime_descriptor_array", queried.runtime_descriptor_array),
        (
            "shader_sampled_image_array_non_uniform_indexing",
            queried.shader_sampled_image_array_non_uniform_indexing,
        ),
        (
            "descriptor_binding_sampled_image_update_after_bind",
            queried.descriptor_binding_sampled_image_update_after_bind,
        ),
        (
            "descriptor_binding_partially_bound",
            queried.descriptor_binding_partially_bound,
        ),
    ];
    for (name, v) in &need {
        println!("[支持] {name} = {}", *v != 0);
    }
    assert!(
        need.iter().all(|(_, v)| *v != 0),
        "生产五位特性不齐——设备出局（与 context.rs 硬校验同判据）"
    );
    let mut di_props = vk::PhysicalDeviceDescriptorIndexingProperties::default();
    let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut di_props);
    unsafe { instance.get_physical_device_properties2(pd, &mut props2) };
    println!(
        "[UAB 限额] 每阶段 sampledImages={} / 每 set {} | 每阶段 samplers={} / 每 set {} | 全池总额={}（对账在 BindlessTables::new 内执行并落账）",
        di_props.max_per_stage_descriptor_update_after_bind_sampled_images,
        di_props.max_descriptor_set_update_after_bind_sampled_images,
        di_props.max_per_stage_descriptor_update_after_bind_samplers,
        di_props.max_descriptor_set_update_after_bind_samplers,
        di_props.max_update_after_bind_descriptors_in_all_pools,
    );

    let device = make_device(&instance, pd);
    let queue = unsafe { device.get_device_queue(0, 0) }; // families[0] = graphics
    let contract = unsafe { MemoryContract::new(&instance, pd) };
    let mut uploader = Uploader::new(&device, &contract, 0, queue, 1024, 2).expect("探针 Uploader");
    // 生产表：容量 TABLE_CAPACITY，限额对账在 new 内落日志
    let mut tables = BindlessTables::new(&device, &instance, pd, &contract, &mut uploader, 0, TABLE_CAPACITY)
        .expect("生产常驻描述符表(容量 1024)");
    println!(
        "[建表] 容量 {} 双表就绪；fallback 白图占槽 0；已用纹理槽 {} / 采样器槽 {}",
        tables.capacity(),
        tables.used_texture_slots(),
        tables.used_sampler_slots(),
    );

    // 测试贴图：红/绿/蓝 2×2 均色（全部 linear 规格；采样器差异在发布键上制造）
    let mut textures: TexMap = HashMap::new();
    for (name, rgba) in [
        ("red", [255u8, 0, 0, 255]),
        ("green", [0, 255, 0, 255]),
        ("blue", [0, 0, 255, 255]),
    ] {
        let spec = ImageSpec {
            width: 2,
            height: 2,
            format: vk::Format::R8G8B8A8_UNORM,
            sampler: ImageSamplerDescriptor::linear(),
        };
        let img = GpuImage::create(&device, &instance, pd, &contract, &spec).expect("测试贴图");
        textures.insert(name, (img, rgba));
    }

    // ============ 组 B：发布与去重（生产表）============
    println!("\n== 组 B(发布与去重)==");
    let lin = sampler_key(&ImageSamplerDescriptor::linear());
    // NEAREST 变体采样器键（仅在滤波两轴上与 linear 不同 → 去重账本分槽）
    let mut nearest_desc = ImageSamplerDescriptor::linear();
    nearest_desc.mag_filter = ImageFilterMode::Nearest;
    nearest_desc.min_filter = ImageFilterMode::Nearest;
    let near = sampler_key(&nearest_desc);
    upload_and_publish(&mut uploader, &mut tables, &textures, &[
        ("red", &lin), ("green", &lin), ("blue", &near),
    ]);
    assert_eq!(
        tables.used_texture_slots(),
        4,
        "fallback(0) + 三张测试贴图 = 4 个纹理槽"
    );
    assert_eq!(
        tables.used_sampler_slots(),
        2,
        "采样器按功能键去重：fallback(0) + linear(1) + nearest(2)，两张线性贴图共享槽 1"
    );

    // 耗尽负例：容量 4 的小表——fallback(0) + 三张贴图(1,2,3) 满载，第 4 张必须报错
    let mut small_tables =
        BindlessTables::new(&device, &instance, pd, &contract, &mut uploader, 0, 4)
            .expect("小容量表(容量 4)");
    upload_and_publish(&mut uploader, &mut small_tables, &textures, &[
        ("red", &lin), ("green", &lin), ("blue", &near),
    ]);
    let white_spec = ImageSpec {
        width: 2,
        height: 2,
        format: vk::Format::R8G8B8A8_UNORM,
        sampler: ImageSamplerDescriptor::linear(),
    };
    let white = GpuImage::create(&device, &instance, pd, &contract, &white_spec).expect("白色贴图");
    match small_tables.publish(white.view(), white.sampler(), &lin) {
        Ok(_) => panic!("容量 4 的小表第 4 次发布竟然成功——容量纪律失效"),
        Err(e) => {
            assert!(e.to_string().contains("容量耗尽"), "耗尽报错文案异常: {e}");
            println!("[耗尽负例] 第 4 张发布被拒(正确): {e}");
        }
    }
    drop(white);

    // ============ 组 C：生产表采样读回 + update-after-bind ============
    run_sampling(&device, &contract, &queue, &mut tables, &textures);

    // 收场：排空后按"表先于贴图本体"反序拆（槽内 view/sampler 是贴图句柄的借用）
    unsafe { device.device_wait_idle() }.expect("探针收场 wait_idle");
    drop(tables);
    drop(small_tables);
    for (_, (img, _)) in textures {
        drop(img);
    }
    drop(uploader);
    unsafe { device.destroy_device(None) };
    if let Some((loader, handle)) = messenger {
        unsafe { loader.destroy_debug_utils_messenger(handle, None) };
    }
    unsafe { instance.destroy_instance(None) };

    println!("\n== 验证层消息收账(全程)==");
    let vu = VU_TOTAL.lock().unwrap_or_else(|p| p.into_inner());
    if vu.is_empty() {
        println!("  (零条——生产表全链在验证层 + 同步验证下清净)");
    } else {
        for m in vu.iter() {
            println!("  {m}");
        }
        panic!("收到验证层消息 {} 条——\"零告警\"验收点未过", vu.len());
    }
}

fn make_instance(entry: &Entry, validation: bool) -> ash::Instance {
    let app_info = vk::ApplicationInfo::default()
        .application_name(c"descriptor_probe")
        .api_version(vk::API_VERSION_1_3);
    let ext_names: Vec<*const std::ffi::c_char> = if validation {
        vec![debug_utils::NAME.as_ptr()]
    } else {
        Vec::new()
    };
    let layer_names: Vec<*const std::ffi::c_char> = if validation {
        vec![VALIDATION_LAYER.as_ptr()]
    } else {
        Vec::new()
    };
    // 同步验证随主工程同款(VkValidationFeaturesEXT)
    let mut sync_validation = vk::ValidationFeaturesEXT::default();
    let mut instance_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_extension_names(&ext_names)
        .enabled_layer_names(&layer_names);
    let sync_enables;
    if validation {
        sync_enables = [vk::ValidationFeatureEnableEXT::SYNCHRONIZATION_VALIDATION];
        sync_validation = sync_validation.enabled_validation_features(&sync_enables);
        instance_info = instance_info.push_next(&mut sync_validation);
    }
    unsafe { entry.create_instance(&instance_info, None) }.expect("vkCreateInstance")
}

fn make_messenger(entry: &Entry, instance: &ash::Instance) -> (debug_utils::Instance, vk::DebugUtilsMessengerEXT) {
    let loader = debug_utils::Instance::new(entry, instance);
    let messenger_info = vk::DebugUtilsMessengerCreateInfoEXT::default()
        .message_severity(
            vk::DebugUtilsMessageSeverityFlagsEXT::WARNING | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR,
        )
        .message_type(
            vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
        )
        .pfn_user_callback(Some(probe_callback));
    let handle = unsafe { loader.create_debug_utils_messenger(&messenger_info, None) }
        .expect("创建验证 messenger");
    (loader, handle)
}

/// 生产特性集（与 context.rs 启用清单同款：timeline + descriptor indexing 五位
/// ——支持 ≠ 启用，每一位显式声明；timeline 是 Uploader 票据信号量的前提，
/// 缺它 = VUID-03252，本探针首跑实抓）。
fn production_vk12() -> vk::PhysicalDeviceVulkan12Features<'static> {
    vk::PhysicalDeviceVulkan12Features::default()
        .timeline_semaphore(true)
        .descriptor_indexing(true)
        .runtime_descriptor_array(true)
        .shader_sampled_image_array_non_uniform_indexing(true)
        .descriptor_binding_sampled_image_update_after_bind(true)
        .descriptor_binding_partially_bound(true)
}

fn make_device(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Device {
    let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    let gfx = families
        .iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .expect("无 graphics 族") as u32;
    let priority = [1.0f32];
    let queue_infos = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(gfx)
        .queue_priorities(&priority)];
    let mut vk12 = production_vk12();
    unsafe {
        instance
            .create_device(
                pd,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_infos)
                    .push_next(&mut vk12),
                None,
            )
            .expect("探针 vkCreateDevice")
    }
}

/// 合批上传（与 flush_uploads 同形状：staging 连续排布 + 一次 transfer 提交）
/// → 等票据（探针合法的 CPU 等待；生产路径由 3.4 的 GPU 侧票据等待承接）
/// → 逐张发布并打印槽位。
fn upload_and_publish(
    uploader: &mut Uploader,
    tables: &mut BindlessTables,
    textures: &TexMap,
    batch: &[(&str, &SamplerKey)],
) {
    let mut staging: Vec<u8> = Vec::new();
    let mut image_uploads = Vec::new();
    for (name, _) in batch {
        let (img, rgba) = &textures[name];
        let src_offset = staging.len() as u64;
        staging.extend_from_slice(&uniform_pixels(*rgba));
        image_uploads.push(StagingImageCopy {
            image: img.image(),
            src_offset,
            width: 2,
            height: 2,
        });
    }
    let ticket = uploader
        .submit_batch(UploadBatch {
            staging,
            image_uploads,
            ..Default::default()
        })
        .expect("测试批次提交")
        .expect("非空批次必有票据");
    uploader.wait_until(ticket).expect("等上传票据");
    for (name, key) in batch {
        let (img, _) = &textures[name];
        let slots = tables.publish(img.view(), img.sampler(), key).expect("发布");
        println!("[发布] {name} → 纹理槽 {} / 采样器槽 {}", slots.texture, slots.sampler);
    }
}

/// push 常量 96B：identity model + tex/sampler 索引 + 零 base_color
/// （偏移 0/64/68/80，与闸门 Rust 镜像 offset_of! 互证）。
fn push_bytes(tex_index: u32, sampler_index: u32) -> [u8; 96] {
    let mut b = [0u8; 96];
    for (i, v) in Mat4::IDENTITY.to_cols_array().iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    b[64..68].copy_from_slice(&tex_index.to_le_bytes());
    b[68..72].copy_from_slice(&sampler_index.to_le_bytes());
    b
}

fn assert_close(actual: [f32; 4], expect: [f32; 4], label: &str) {
    for (i, (&a, &e)) in actual.iter().zip(expect.iter()).enumerate() {
        assert!(
            (a - e).abs() < 0.01,
            "{label} 通道 {i} 读回 {a} 期望 {e}(全值 {actual:?})"
        );
    }
}

/// 读回缓冲第 `i` 个 vec4 的整槽值。
fn f32_at(back: &[u8], i: usize) -> [f32; 4] {
    let base = i * 16;
    [
        f32::from_le_bytes(back[base..base + 4].try_into().expect("4 字节")),
        f32::from_le_bytes(back[base + 4..base + 8].try_into().expect("4 字节")),
        f32::from_le_bytes(back[base + 8..base + 12].try_into().expect("4 字节")),
        f32::from_le_bytes(back[base + 12..base + 16].try_into().expect("4 字节")),
    ]
}

/// 组 C：生产表采样读回 + update-after-bind。
///
/// 槽位前提（组 B 定下的账）：纹理槽 1=红 2=绿 3=蓝；采样器槽 1=linear 2=nearest。
fn run_sampling(
    device: &Device,
    contract: &MemoryContract,
    queue: &vk::Queue,
    tables: &mut BindlessTables,
    textures: &TexMap,
) {
    println!("\n== 组 C(生产表采样读回 + update-after-bind)==");
    let words: Vec<u32> = PROBE_SPV
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();

    // 探针专用 I/O：set2 = storage 索引(读) + 输出(写)；与生产表无关
    let set2_layout = unsafe {
        device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&[
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
            ]),
            None,
        )
    }
    .expect("set2 布局(探针 I/O)");
    let set2_pool = unsafe {
        device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(1)
                .pool_sizes(&[vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(2)]),
            None,
        )
    }
    .expect("set2 pool");
    let set2 = unsafe {
        device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(set2_pool)
                .set_layouts(std::slice::from_ref(&set2_layout)),
        )
    }
    .expect("set2 分配")[0];

    // 每调用索引。**槽位账本以 publish 返回值为准，不按心算**：去重后 linear 键
    // 与 fallback 共享采样器槽 0、nearest 占槽 1——初版探针误按"linear=1/nearest=2"
    // 心算用了未写入的槽 2，读回竟然"全对"且验证层零告警，恰好实证特性族篇 §5
    // "空槽访问是 UB、验证层不保证能抓"（首跑教训，钉入施工记录）。
    // 现按真实账本：i=0 → (tex2 绿, 采样槽 0 线性)；i=1 → (tex1 红, 采样槽 1 最近)
    let mut indices =
        GpuBuffer::create(device, contract, 16, BufferRole::Storage).expect("indices buffer");
    let mut idx_bytes = [0u8; 16];
    idx_bytes[0..4].copy_from_slice(&2u32.to_le_bytes());
    idx_bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    idx_bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
    idx_bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
    indices.write(0, &idx_bytes).expect("indices 写");
    let mut out =
        GpuBuffer::create(device, contract, 48, BufferRole::Storage).expect("out buffer");
    let indices_info = [vk::DescriptorBufferInfo::default()
        .buffer(indices.buffer())
        .offset(0)
        .range(16)];
    let out_info = [vk::DescriptorBufferInfo::default()
        .buffer(out.buffer())
        .offset(0)
        .range(48)];
    unsafe {
        device.update_descriptor_sets(
            &[
                vk::WriteDescriptorSet::default()
                    .dst_set(set2)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&indices_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(set2)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&out_info),
            ],
            &[],
        );
    }

    // pipeline layout：生产 set0/set1（表内同源布局）+ 探针 set2 + push 96B
    let push_ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::COMPUTE)
        .offset(0)
        .size(96)];
    let pipeline_layout = unsafe {
        device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&[tables.set0_layout(), tables.set1_layout(), set2_layout])
                .push_constant_ranges(&push_ranges),
            None,
        )
    }
    .expect("pipeline layout");
    let shader_module = unsafe {
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
    }
    .expect("shader module");
    let pipeline = unsafe {
        let created = device
            .create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .stage(
                        vk::PipelineShaderStageCreateInfo::default()
                            .stage(vk::ShaderStageFlags::COMPUTE)
                            .module(shader_module)
                            .name(c"main"),
                    )
                    .layout(pipeline_layout)],
                None,
            )
            .map_err(|(_, e)| e)
            .expect("compute pipeline");
        created[0]
    };

    let cmd_pool = unsafe {
        device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                .queue_family_index(0),
            None,
        )
    }
    .expect("命令池");
    let cbs = unsafe {
        device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(cmd_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(2),
        )
    }
    .expect("命令缓冲");

    // 一次 dispatch = 一段录制 + 一次提交 + fence 等待（绑定生产 set0/set1 + 探针 set2）
    let run_dispatch = |tex: u32, samp: u32, cb: vk::CommandBuffer| {
        unsafe {
            device.begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
        }
        .expect("begin");
        unsafe {
            device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, pipeline);
            device.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                pipeline_layout,
                0,
                &[tables.resident_set(), tables.frame_set(0), set2],
                &[],
            );
            device.cmd_push_constants(
                cb,
                pipeline_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                &push_bytes(tex, samp),
            );
            device.cmd_dispatch(cb, 1, 1, 1);
            device.end_command_buffer(cb)
        }
        .expect("end");
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .expect("fence");
        unsafe {
            device.queue_submit(
                *queue,
                &[vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&cb))],
                fence,
            )
        }
        .expect("提交");
        unsafe { device.wait_for_fences(std::slice::from_ref(&fence), true, 5_000_000_000) }
            .expect("等 dispatch 完成");
        unsafe { device.destroy_fence(fence, None) };
    };

    // —— dispatch 1:push 走 tex3(蓝)+采样槽 1(最近)；storage 非一致 → 绿(线性)/红(最近)——
    run_dispatch(3, 1, cbs[0]);
    let mut back = [0u8; 48];
    out.read(0, &mut back).expect("读回");
    let out0 = f32_at(&back, 0);
    let out1 = f32_at(&back, 1);
    let out2 = f32_at(&back, 2);
    assert_close(out0, BLUE, "out[0] push 路径(tex3 蓝+采样槽1 最近)");
    assert_close(out1, GREEN, "out[1] 非一致 i=0(tex2 绿+采样槽0 线性)");
    assert_close(out2, RED, "out[2] 非一致 i=1(tex1 红+采样槽1 最近)");
    println!(
        "[采样] 生产 set0/set1 读回 = 蓝/绿/红 全对(push 一致 + storage 非一致双路径)"
    );

    // —— update-after-bind:set 已在 dispatch 1 被绑定并提交完成,此刻改写纹理槽 3
    // 为绿图 view,二次 dispatch 应读出新内容——UAB 的存在意义在生产表上实证
    let (_, (green_img, _)) = textures.iter().find(|(name, _)| **name == "green").expect("绿图");
    let green_view_info = [vk::DescriptorImageInfo::default()
        .image_view(green_img.view())
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    unsafe {
        device.update_descriptor_sets(
            &[vk::WriteDescriptorSet::default()
                .dst_set(tables.resident_set())
                .dst_binding(0)
                .dst_array_element(3)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(&green_view_info)],
            &[],
        );
    }
    println!("[UAB] set 已绑定并提交过的前提下改写纹理槽 3 → 绿(零告警前提:上一轮 fence 已完成)");
    run_dispatch(3, 1, cbs[1]);
    out.read(0, &mut back).expect("读回");
    let out0_after = f32_at(&back, 0);
    assert_close(out0_after, GREEN, "out[0] UAB 更新后(tex3 应读出绿)");
    println!("[UAB] 二次 dispatch 读回绿——update-after-bind 在生产表上生效");

    // 清场:排空后反序拆。set2 不显式 free(pool 无 FREE 位,VUID-00312——首跑实抓),
    // pool 销毁即整体回收
    unsafe { device.device_wait_idle() }.expect("组 C 收场 wait_idle");
    unsafe { device.destroy_pipeline(pipeline, None) };
    unsafe { device.destroy_shader_module(shader_module, None) };
    unsafe { device.destroy_pipeline_layout(pipeline_layout, None) };
    unsafe { device.destroy_command_pool(cmd_pool, None) };
    drop(out);
    drop(indices);
    unsafe { device.destroy_descriptor_pool(set2_pool, None) };
    unsafe { device.destroy_descriptor_set_layout(set2_layout, None) };
}

/// 验证层回调（与 bindless_probe 同款收账）。
///
/// # Safety
/// 由 Vulkan 实现按回调契约调用；`p_callback_data` 依约定为合法指针或空。
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
    VU_TOTAL
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(format!("[{severity:?}] {msg}"));
    vk::FALSE
}
