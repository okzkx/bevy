//! 3.2.1 timeline 探针 v2（缺陷 D5 重写）：验证层执法下的驱动观察，不再是"规范裁决"。
//!
//! v1（68fdafa88）已撤回的教训（见《已实现缺陷与修复验收》§5）：
//! - A/B 两组都调 queue_submit2 却不启用 synchronization2，双双违反
//!   VUID-vkQueueSubmit2-synchronization2-03866，B 不是干净对照；
//! - 不请求验证层，"PASS"只是没炸，不是"验证层通过"；
//! - "违规必得 ERROR_FEATURE_NOT_PRESENT""PASS 即合法""1.3 结构没有字段即无需
//!   启用"都是错误判据：应用违反 valid usage 时驱动不承担按固定返回码拒绝的义务，
//!   成功返回不是规范许可。
//!
//! v1 的运行记录仅作历史观察保留（3.2.1 记录 §复核），其结论不再引用。
//!
//! v2 的两组是"观察 + 诊断"，不是正误裁决：
//! - 组 A（合法用法观察组）：显式启用 timelineSemaphore + synchronization2，
//!   走创建 → 查初值 → submit2 signal → host wait → 复查全链；
//! - 组 B（负例诊断组）：不启用 timelineSemaphore 直接创建 TIMELINE 信号量，
//!   只收验证层的预期诊断（VUID-VkSemaphoreTypeCreateInfo-timelineSemaphore-03252），
//!   不期待驱动必返回固定错误码，创建即拆，不当正常路径继续用。
//!
//! 信号量销毁的完成条件可证明：host wait 返回时计数已到 SIGNAL_VALUE，
//! 即 signal 那笔提交已执行完、无在途引用——此刻销毁合法。
//!
//! 本探针不碰窗口/surface，自含 Entry → Instance → PhysicalDevice → Device
//! 最小链，与主工程互不影响（cargo run --example timeline_probe）。

use std::slice;
use std::sync::Mutex;

use ash::{ext::debug_utils, vk, Device, Entry};

const SIGNAL_VALUE: u64 = 5;
const VALIDATION_LAYER: &std::ffi::CStr = c"VK_LAYER_KHRONOS_validation";

/// 验证层消息收账：WARNING/ERROR 全记，探针结束统一打印。
static VU_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Vulkan 验证层回调：消息原文入 VU_LOG（返回 FALSE = 不被截获）。
///
/// # Safety
/// 本函数不被本项目调用——由 Vulkan 实现按回调契约调用，`p_callback_data`
/// 依约定为合法指针或空。
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
        .push(format!("[{:?}] {msg}", severity));
    vk::FALSE
}

fn main() {
    let entry = unsafe { Entry::load() }.expect("加载 Vulkan loader");

    // 验证层三件事分开报：装没装（系统状态）、请求没请求（本探针）、收没收到 VUID（结果）。
    let validation_installed = unsafe { entry.enumerate_instance_layer_properties() }
        .unwrap_or_default()
        .iter()
        .any(|props| unsafe { std::ffi::CStr::from_ptr(props.layer_name.as_ptr()) } == VALIDATION_LAYER);
    println!(
        "验证层 {VALIDATION_LAYER:?}: {}",
        if validation_installed {
            "已安装，本探针请求启用（VUID 收账见结尾）"
        } else {
            "未安装（本机缺 Vulkan SDK），本轮裸奔——结果不构成验证证据"
        }
    );

    let app_info = vk::ApplicationInfo::default()
        .application_name(c"timeline_probe")
        .api_version(vk::API_VERSION_1_3);
    let ext_names = if validation_installed {
        vec![debug_utils::NAME.as_ptr()]
    } else {
        Vec::new()
    };
    let layer_names = if validation_installed {
        vec![VALIDATION_LAYER.as_ptr()]
    } else {
        Vec::new()
    };
    let instance = unsafe {
        entry.create_instance(
            &vk::InstanceCreateInfo::default()
                .application_info(&app_info)
                .enabled_extension_names(&ext_names)
                .enabled_layer_names(&layer_names),
            None,
        )
    }
    .expect("vkCreateInstance");

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
        unsafe { loader.create_debug_utils_messenger(&messenger_info, None) }.expect("创建验证 messenger")
    });

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

    // 支持查询（features2 链）：报告"驱动支持吗"，与设备创建时的启用集合分开报。
    let mut vk12_query = vk::PhysicalDeviceVulkan12Features::default();
    let mut vk13_query = vk::PhysicalDeviceVulkan13Features::default();
    let mut feats2 = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut vk12_query)
        .push_next(&mut vk13_query);
    unsafe { instance.get_physical_device_features2(pd, &mut feats2) };
    println!(
        "查询支持: timelineSemaphore={} synchronization2={} dynamicRendering={}",
        vk12_query.timeline_semaphore, vk13_query.synchronization2, vk13_query.dynamic_rendering,
    );

    println!("\n== 组 A（合法用法观察组）：启用 timelineSemaphore + synchronization2 ==");
    {
        let (device, queue) = create_device(&instance, pd, true);
        run_timeline_round(&device, queue);
        println!("[组 A] 创建/查初值/submit2 signal/host wait/复查 全链通过");
        unsafe { device.destroy_device(None) };
    }

    println!("\n== 组 B（负例诊断组）：不启用 timelineSemaphore 创建 TIMELINE 信号量 ==");
    {
        let (device, _queue) = create_device(&instance, pd, false);
        match try_create_timeline_without_enable(&device) {
            Ok(sem) => {
                println!(
                    "[组 B 观察] 驱动返回成功——违反 VUID 未必被驱动拒绝（判据修正的活证据：成功返回≠规范许可）；信号量立即销毁，不当正常路径继续用"
                );
                unsafe { device.destroy_semaphore(sem, None) };
            }
            Err(e) => {
                println!("[组 B 观察] 驱动拒绝创建: {e:?}（记录实际返回码，不假设必为 ERROR_FEATURE_NOT_PRESENT）");
            }
        }
        let hit = VU_LOG
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|m| m.contains("VUID-VkSemaphoreTypeCreateInfo-timelineSemaphore-03252"));
        println!(
            "[组 B 诊断] VUID-03252 {}",
            if hit { "已被验证层执法收账" } else { "未见（验证层未运行或未覆盖此检查）" }
        );
        unsafe { device.destroy_device(None) };
    }

    unsafe { instance.destroy_instance(None) };

    println!("\n== 验证层 VUID 收账 ==");
    let vu = VU_LOG.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if vu.is_empty() {
        println!("  （零条——仅当验证层已启用时，这才是\"已验证无告警\"的证据）");
    } else {
        for m in vu.iter() {
            println!("  {m}");
        }
    }
    println!("\n结论口径：本探针只回答\"这套用法在验证层下跑通了没有\"；");
    println!("timeline 必须显式启用的规范依据见《已实现缺陷与修复验收》§1 引用的 features.adoc L105-106 与 VUID-03252，不以本探针的 PASS 裁决。");
}

/// graphics 族设备。`enable_bits=true` 链 Vulkan12Features.timeline_semaphore +
/// Vulkan13Features.synchronization2（组 A 的合法启用集合，与所用 API 一一对应）；
/// false 则什么都不链（组 B 负例）。feature 结构活到 create_device 之后（pNext 存指针）。
fn create_device(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    enable_bits: bool,
) -> (Device, vk::Queue) {
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
    let mut vk13 = vk::PhysicalDeviceVulkan13Features::default();
    if enable_bits {
        vk12 = vk12.timeline_semaphore(true);
        vk13 = vk13.synchronization2(true);
        info = info.push_next(&mut vk12).push_next(&mut vk13);
    }
    let device = unsafe { instance.create_device(pd, &info, None) }.expect("vkCreateDevice");
    let queue = unsafe { device.get_device_queue(gfx, 0) };
    (device, queue)
}

/// 组 A 全链：创建 → 查初值 → submit2 signal 到 5（空命令提交，合法：SubmitInfo2
/// 可以只带信号量）→ host wait 到 5 → 复查。合法用法失败即 expect（探针环境）。
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
        // 完成条件可证明：host wait 已返回且值到达 → signal 提交执行完、无在途引用，销毁合法
        device.destroy_semaphore(sem, None);
    }
}

/// 组 B 负例：不启用 timelineSemaphore 直接创建 TIMELINE 信号量。
/// 任何结局只记录不断言——驱动可以返回成功也可以报错，验证层的 VUID-03252
/// 才是要收的账；"必返 ERROR_FEATURE_NOT_PRESENT"正是 v1 被撤回的判据。
fn try_create_timeline_without_enable(device: &Device) -> Result<vk::Semaphore, vk::Result> {
    unsafe {
        let mut ty = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        device.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut ty), None)
    }
}
