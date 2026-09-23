//! 3.2.1 校验探针：1.3 设备上 timeline 信号量不启用 features 能不能用？
//!
//! 背景：施工 3.2.1 定案撤掉了 `Vulkan12Features.timeline_semaphore` 显式启用
//! （判断：timeline 在 1.3 是无条件核心，无 feature bit）。校验意见质疑"1.3 基线
//! 不等于自动启用，仍需声明"。本探针用 A/B 实测裁决：
//! - 组 A：设备创建**完全不链任何 features**（被质疑的路径，即主工程现状）；
//! - 组 B：链 `Vulkan12Features.timeline_semaphore(true)`（显式启用对照）。
//!
//! 两组各走全链：创建 timeline 信号量 → 查初值 0 → queue_submit2 signal 到 5
//! （空命令提交，无命令缓冲）→ vkWaitSemaphores 等到 5 → 复查值。
//! 若"必须启用"成立，组 A 会在创建或提交处拿到 ERROR_FEATURE_NOT_PRESENT。
//!
//! 另附结构性证据：`PhysicalDeviceVulkan13Features`（1.3 feature 的家）的全部
//! 字段里没有 timelineSemaphore——1.3 里它没有启用位，"启用"无从谈起。
//!
//! 本探针不碰窗口/surface，自含 Entry → Instance → PhysicalDevice → Device
//! 最小链，与主工程互不影响（cargo run --example timeline_probe）。

use std::slice;

use ash::{vk, Device, Entry};

const SIGNAL_VALUE: u64 = 5;

fn main() {
    let entry = unsafe { Entry::load() }.expect("加载 Vulkan loader");
    let app_info = vk::ApplicationInfo::default()
        .application_name(c"timeline_probe")
        .api_version(vk::API_VERSION_1_3);
    let instance = unsafe {
        entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app_info),
            None,
        )
    }
    .expect("vkCreateInstance");

    // 与主工程同门槛：只选报 1.3 的设备
    let pds = unsafe { instance.enumerate_physical_devices() }.expect("枚举物理设备");
    let pd = *pds
        .iter()
        .find(|pd| unsafe { instance.get_physical_device_properties(**pd) }.api_version >= vk::API_VERSION_1_3)
        .expect("没有报支持 Vulkan 1.3 的物理设备");
    let props = unsafe { instance.get_physical_device_properties(pd) };
    let dev_name =
        unsafe { std::ffi::CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy();
    println!(
        "设备 {dev_name}: API v{}.{}.{}（过 1.3 基线）",
        vk::api_version_major(props.api_version),
        vk::api_version_minor(props.api_version),
        vk::api_version_patch(props.api_version),
    );

    // 1.3 设备上 Vulkan12/13Features 的支持查询。ash 0.38 的 extends 矩阵漏标了
    // 12/13 features → Properties2（手接 pNext 链实测驱动未填充，未深究），查询支持
    // 走 Features2 链——两条结构都 impl 了 ExtendsPhysicalDeviceFeatures2，类型安全。
    let mut vk12_query = vk::PhysicalDeviceVulkan12Features::default();
    let mut vk13_query = vk::PhysicalDeviceVulkan13Features::default();
    let mut feats2 = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut vk12_query)
        .push_next(&mut vk13_query);
    unsafe { instance.get_physical_device_features2(pd, &mut feats2) };
    println!(
        "查询 Vulkan12Features: drawIndirectCount={} descriptorIndexing={} timelineSemaphore={}",
        vk12_query.draw_indirect_count,
        vk12_query.descriptor_indexing,
        vk12_query.timeline_semaphore
    );
    println!(
        "查询 Vulkan13Features: dynamicRendering={} synchronization2={}",
        vk13_query.dynamic_rendering, vk13_query.synchronization2
    );

    for (label, enable) in [
        ("A 不链任何 features（主工程现状）", false),
        ("B 链 Vulkan12Features.timeline_semaphore（显式启用）", true),
    ] {
        let (device, queue) = create_device(&instance, pd, enable);
        run_timeline_round(&device, queue);
        println!("[PASS] 组 {label}：创建/查值/signal/wait 全链通过");
        unsafe { device.destroy_device(None) };
    }
    unsafe { instance.destroy_instance(None) };
    println!("结论：组 A 成立 = 1.3 设备上 timeline 信号量无需 features 启用");
}

/// graphics 族 +（可选）Vulkan12Features timeline 位，返回设备与对应队列。
fn create_device(instance: &ash::Instance, pd: vk::PhysicalDevice, enable_12_bit: bool) -> (Device, vk::Queue) {
    let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    let gfx = families
        .iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .expect("无 graphics 族") as u32;
    let priority = [1.0f32];
    let queue_info = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(gfx)
        .queue_priorities(&priority);
    let mut info = vk::DeviceCreateInfo::default().queue_create_infos(slice::from_ref(&queue_info));
    let mut vk12 = vk::PhysicalDeviceVulkan12Features::default();
    if enable_12_bit {
        vk12 = vk12.timeline_semaphore(true);
        info = info.push_next(&mut vk12);
    }
    let device = unsafe { instance.create_device(pd, &info, None) }.expect("vkCreateDevice");
    let queue = unsafe { device.get_device_queue(gfx, 0) };
    (device, queue)
}

/// 全链：创建 → 查初值 → submit signal 到 5（空命令）→ wait 到 5 → 复查。
/// 任何一步若"能力未启用"，驱动按规范应返回 ERROR_FEATURE_NOT_PRESENT，expect 即炸。
fn run_timeline_round(device: &Device, queue: vk::Queue) {
    unsafe {
        let mut ty = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let sem = device
            .create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut ty), None)
            .expect("创建 timeline 信号量");
        assert_eq!(
            device.get_semaphore_counter_value(sem).expect("查初值"),
            0,
            "初值应为 0"
        );

        let signal = vk::SemaphoreSubmitInfo::default()
            .semaphore(sem)
            .value(SIGNAL_VALUE)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS);
        let submit = vk::SubmitInfo2::default().signal_semaphore_infos(slice::from_ref(&signal));
        device
            .queue_submit2(queue, &[submit], vk::Fence::null())
            .expect("signal 提交（空命令缓冲）");
        device
            .wait_semaphores(
                &vk::SemaphoreWaitInfo::default()
                    .semaphores(&[sem])
                    .values(&[SIGNAL_VALUE]),
                1_000_000_000,
            )
            .expect("host 等到 5");
        assert_eq!(
            device.get_semaphore_counter_value(sem).expect("复查"),
            SIGNAL_VALUE,
            "信号量值应为 5"
        );
        device.destroy_semaphore(sem, None);
    }
}
