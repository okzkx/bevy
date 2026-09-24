//! ash Vulkan 进程级上下文（step2 施工③④拆分）：Entry → Instance(+验证层) →
//! Win32 Surface → PhysicalDevice → Device(+dynamicRendering+timelineSemaphore) →
//! Queue（图形 + transfer，同族则合一）。
//!
//! 生命周期四层（详见 .agent/docs/2-宿主壳/材料/VulkanContext字段释义：从Entry到Swapchain.md §12）：
//! - 本结构 = 进程级（随进程活）+ Surface（窗口级，单窗宿主壳中并入）；
//! - resize 级的 Swapchain 与帧级的命令缓冲/fence/信号量在同层 `swapchain` / `frames` 子模块。
//!
//! 其余约束来源（step2《窗口链路侦察》）：
//! - 句柄取自 `RawHandleWrapper::get_window_handle()`（bevy 的安全方法），Win32 路径
//!   手写 `vkCreateWin32SurfaceKHR`；hinstance 可能为 None，`GetModuleHandleW(None)` 兜底；
//! - ash 0.38.0 对 Instance/Device/loader 均无 Drop 实现（extensions_generated.rs 零
//!   `impl Drop`）——销毁全部手动、按 Messenger ← Surface ← Device ← Instance 反序；
//! - `Entry` 内持 libloading 句柄，提前丢它会卸载 vulkan-1.dll，必须与 Instance 同寿命；
//! - bevy 退出时 runner `exiting` 回调会清场（清场序对 Resource 是任意的），正常退出
//!   走 main.rs 的 `teardown_vulkan`（OnAppExitSystems）反序拆除，本 Drop 只是兜底。

use std::ffi::{c_char, c_void};
use std::num::NonZeroIsize;

use ash::{
    ext::debug_utils,
    khr::{surface, win32_surface},
    vk, Device, Entry, Instance,
};
use bevy::log::{error, info};
use raw_window_handle::RawWindowHandle;

use crate::error::VulkanError;

const VALIDATION_LAYER: &std::ffi::CStr = c"VK_LAYER_KHRONOS_validation";

#[cfg(windows)]
unsafe extern "system" {
    fn GetModuleHandleW(lp_module_name: *const u16) -> isize;
}

/// Vulkan 验证层回调：WARNING/ERROR 直打 stderr（返回 FALSE = 不被截获）。
///
/// # Safety
/// 本函数不被本项目调用——由 Vulkan 实现按回调契约调用，`p_callback_data`
/// 依约定为合法指针或空；`_user_data` 未使用不触碰。
unsafe extern "system" fn debug_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _message_types: vk::DebugUtilsMessageTypeFlagsEXT,
    p_callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT,
    _user_data: *mut c_void,
) -> vk::Bool32 {
    let msg = if p_callback_data.is_null() {
        "(no data)".into()
    } else {
        unsafe { std::ffi::CStr::from_ptr((*p_callback_data).p_message) }.to_string_lossy()
    };
    let tag = if severity.contains(vk::DebugUtilsMessageSeverityFlagsEXT::ERROR) {
        "VK-ERROR"
    } else {
        "VK-warn "
    };
    eprintln!("[{tag}] {msg}");
    vk::FALSE
}

/// 进程级 Vulkan 上下文。字段顺序即声明层级；`Drop` 手动反序（ash 无自动 Drop）。
#[derive(bevy::prelude::Resource)]
pub struct Context {
    /// 持 libloading 句柄防 vulkan-1.dll 提前卸载（所有 fn 指针已在 new 时拷走，字段本身不再读）
    #[expect(dead_code, reason = "entry 仅作生命周期锚：Library 在即 Vulkan 可用")]
    entry: Entry,
    pub instance: Instance,
    pub surface_fns: surface::Instance,
    pub surface: vk::SurfaceKHR,
    debug: Option<(debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
    pub physical_device: vk::PhysicalDevice,
    pub device: Device,
    /// graphics 与 present 同族（桌面 GPU 普适；异族需求出现时再拆）
    pub queue_family_index: u32,
    pub queue: vk::Queue,
    /// transfer 队列族与队列：3.2 起拷贝提交走这里，与图形提交互不排队。
    /// 没有"TRANSFER 且无 GRAPHICS"的专用族时与 graphics 同族同队列（合批退回，
    /// 取舍记录见 3.2.1 施工记录）；下游比对两个族号即可分辨。
    pub transfer_queue_family_index: u32,
    pub transfer_queue: vk::Queue,
    /// 内存契约（设备侧事实一次查询冻结：类型/堆表 + nonCoherentAtomSize），
    /// 3.2.2.1 定案的选型唯一裁判。池/上传按引用取用，不各自重查。
    memory_contract: crate::vulkan::MemoryContract,
}

impl Context {
    pub fn new(wrapper: &bevy::window::RawHandleWrapper) -> Result<Self, VulkanError> {
        // ---- 窗口句柄（Win32；hinstance 缺失时进程句柄兜底）----
        let (hwnd, hinstance) = match wrapper.get_window_handle() {
            RawWindowHandle::Win32(w) => (
                w.hwnd.get(),
                w.hinstance.map_or_else(
                    || unsafe { GetModuleHandleW(std::ptr::null()) },
                    NonZeroIsize::get,
                ),
            ),
            other => {
                return Err(VulkanError::Init(format!(
                    "宿主壳仅支持 Win32 窗口句柄，实际 {other:?}"
                )))
            }
        };

        // Entry::load 是 unsafe：dlopen 到的符号无任何来源担保（ash 文档约定），
        // 本项目只在 main 起点调一次，Windows 驱动自带 vulkan-1.dll
        let entry = unsafe { Entry::load() }
            .map_err(|e| VulkanError::Init(format!("加载 Vulkan loader 失败: {e}")))?;

        // 验证层：学习项目第一优先级，所有 Vulkan 误用要当场可见；不可用则明说后裸奔
        let validation = unsafe { entry.enumerate_instance_layer_properties() }
            .unwrap_or_default()
            .iter()
            .any(|props| unsafe { std::ffi::CStr::from_ptr(props.layer_name.as_ptr()) } == VALIDATION_LAYER);
        if !validation {
            error!("未找到 {VALIDATION_LAYER:?}，验证层未启用");
        }

        // ---- Instance ----
        let mut ext_names: Vec<*const c_char> =
            vec![surface::NAME.as_ptr(), win32_surface::NAME.as_ptr()];
        if validation {
            ext_names.push(debug_utils::NAME.as_ptr());
        }
        let layer_names: Vec<*const c_char> = if validation {
            vec![VALIDATION_LAYER.as_ptr()]
        } else {
            Vec::new()
        };

        let app_name = c"ash_renderer";
        let app_info = vk::ApplicationInfo::default()
            .application_name(app_name)
            .api_version(vk::API_VERSION_1_3);
        // 同步验证（VkValidationFeaturesEXT）：核心检查不覆盖跨对象同步时序——
        // fence/信号量的 signal-wait 配对、present 复用、在飞销毁，这正是缺陷
        // D2/D3/D4 的执法面（V1 前置闸门的裁判本体）。同步验证有帧时间开销，
        // 3.4 真实绘制后若受限，把 enables 换成空数组即降回核心检查。
        let sync_enables = if validation {
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
            // enables/sync_validation 都须活到 create_instance 返回（pNext 存指针）
            sync_validation = sync_validation.enabled_validation_features(enables);
            instance_info = instance_info.push_next(&mut sync_validation);
        }
        let instance = unsafe { entry.create_instance(&instance_info, None) }
            .map_err(|e| VulkanError::Init(format!("vkCreateInstance 失败: {e}")))?;

        // ---- 验证层 messenger（WARNING/ERROR 全接）----
        let debug = if validation {
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
                .pfn_user_callback(Some(debug_callback));
            let messenger =
                unsafe { loader.create_debug_utils_messenger(&messenger_info, None) }
                    .map_err(|e| VulkanError::Init(format!("创建验证 messenger 失败: {e}")))?;
            Some((loader, messenger))
        } else {
            None
        };

        // ---- Surface（手写 Win32 路径，方案见搭建记录 §3）----
        let surface_fns = surface::Instance::new(&entry, &instance);
        let win32_fns = win32_surface::Instance::new(&entry, &instance);
        let surface = unsafe {
            win32_fns.create_win32_surface(
                &vk::Win32SurfaceCreateInfoKHR::default()
                    .hinstance(hinstance)
                    .hwnd(hwnd),
                None,
            )
        }
        .map_err(|e| VulkanError::Init(format!("vkCreateWin32SurfaceKHR 失败: {e}")))?;

        // ---- PhysicalDevice：1.3 基线硬校验（学习工程，不做老设备兼容路径）+ graphics+present 同族，离散卡优先 ----
        let physical_devices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| VulkanError::Init(format!("枚举物理设备失败: {e}")))?;
        let mut best: Option<(u32, u32, vk::PhysicalDevice)> = None; // (score, family_index, pd)
        for pd in physical_devices {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            // 1.3 闸门：timeline 信号量在 1.3 是无条件核心（features 无位可开），
            // dynamicRendering 等 1.3 feature 结构也不能喂给 1.2 设备——不达标的设备
            // 在这里出局并记日志，比创建后莫名崩溃便宜；全部出局 = Init 报错冒泡退出
            if props.api_version < vk::API_VERSION_1_3 {
                let name = unsafe { std::ffi::CStr::from_ptr(props.device_name.as_ptr()) }
                    .to_string_lossy();
                info!(
                    "跳过物理设备 {name}：报 API v{}.{}.{}，低于本项目 1.3 基线",
                    vk::api_version_major(props.api_version),
                    vk::api_version_minor(props.api_version),
                    vk::api_version_patch(props.api_version),
                );
                continue;
            }
            let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
            let Some(family) = families
                .iter()
                .enumerate()
                .find(|(i, f)| {
                    f.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                        && unsafe {
                            surface_fns.get_physical_device_surface_support(pd, *i as u32, surface)
                        }
                        .unwrap_or(false)
                })
                .map(|(i, _)| i as u32)
            else {
                continue;
            };
            let score = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 1000,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 100,
                _ => 1,
            };
            if best.is_none_or(|(s, _, _)| score > s) {
                best = Some((score, family, pd));
            }
        }
        let Some((_score, queue_family_index, physical_device)) = best else {
            return Err(VulkanError::Init(
                "没有报支持 Vulkan 1.3 且 graphics+present 同支持的物理设备".into(),
            ));
        };

        // ---- transfer 专用队列族：拷贝提交与图形提交分流（3.2 合批上传的载体）----
        // 条件照施工计划：有 TRANSFER 且无 GRAPHICS 的独立族（纯 DMA 引擎，不与图形
        // 抢占）；不要求 present/compute——拷贝队列只做拷贝。找不到是桌面 GPU 常态
        // （多数驱动的 transfer 能力就长在 graphics 族上），退回 graphics 合批并记日志，
        // 不为凑专用族改变设备选择。
        let families =
            unsafe { instance.get_physical_device_queue_family_properties(physical_device) };
        let transfer_queue_family_index = families
            .iter()
            .enumerate()
            .find(|(_, f)| {
                f.queue_flags.contains(vk::QueueFlags::TRANSFER)
                    && !f.queue_flags.contains(vk::QueueFlags::GRAPHICS)
            })
            .map(|(i, _)| i as u32)
            .unwrap_or(queue_family_index);

        // ---- Device：图形队列族（+ 独立 transfer 族若有）+ swapchain 扩展 + 1.3 features ----
        // 支持与启用是两件事（缺陷 D1 定案）：features2 查询回答"驱动支持吗"，
        // vkCreateDevice 的 feature 结构声明"本逻辑设备要用哪些"——1.2 收编核心起
        // timeline 支持即 mandatory，但"默认启用"从不存在（features.adoc L105-106：
        // 用到的细粒度 feature 必须在设备创建时启用；VUID-VkSemaphoreTypeCreateInfo-
        // timelineSemaphore-03252 执法）。Vulkan12Features 是 1.2 晋升特性的聚合启用位，
        // 1.3 设备照用；Vulkan13Features 不列 timeline 是晋升年代不同，不是取消启用位。
        let mut vk12_query = vk::PhysicalDeviceVulkan12Features::default();
        let mut vk13_query = vk::PhysicalDeviceVulkan13Features::default();
        let mut features2 = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut vk12_query)
            .push_next(&mut vk13_query);
        unsafe { instance.get_physical_device_features2(physical_device, &mut features2) };
        // 与 1.3 基线同款的硬校验：要用而驱动不支持 = 出局，不创建半残设备
        if vk12_query.timeline_semaphore == 0 || vk13_query.dynamic_rendering == 0 {
            return Err(VulkanError::Init(format!(
                "物理设备 feature 不足：timelineSemaphore 支持={} dynamicRendering 支持={}（1.3 实现必须为 1）",
                vk12_query.timeline_semaphore, vk13_query.dynamic_rendering
            )));
        }
        // 显式启用本工程用到的每个 feature 位，不依赖"查询为 true"的惯性
        let mut vulkan12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
        let mut vulkan13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true);
        let queue_priority = [1.0f32];
        let mut queue_infos = vec![vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&queue_priority)];
        if transfer_queue_family_index != queue_family_index {
            queue_infos.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(transfer_queue_family_index)
                    .queue_priorities(&queue_priority),
            );
        }
        let device_exts = [ash::khr::swapchain::NAME.as_ptr()];
        let device = unsafe {
            instance.create_device(
                physical_device,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_infos)
                    .enabled_extension_names(&device_exts)
                    .push_next(&mut vulkan12)
                    .push_next(&mut vulkan13),
                None,
            )
        }
        .map_err(|e| VulkanError::Init(format!("vkCreateDevice 失败: {e}")))?;
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };
        let transfer_queue = if transfer_queue_family_index == queue_family_index {
            queue
        } else {
            unsafe { device.get_device_queue(transfer_queue_family_index, 0) }
        };

        let dev_props = unsafe { instance.get_physical_device_properties(physical_device) };
        let dev_name =
            unsafe { std::ffi::CStr::from_ptr(dev_props.device_name.as_ptr()).to_string_lossy() };
        let transfer_note = if transfer_queue_family_index == queue_family_index {
            "与 graphics 同族（无专用 transfer 族，合批退回）"
        } else {
            "专用 transfer 族（无 GRAPHICS）"
        };
        // # Safety:physical_device 来自成功枚举、instance 未销毁(ash 约定参数
        // 合法性由调用方担保);契约在设备存活期内恒定
        let memory_contract =
            unsafe { crate::vulkan::MemoryContract::new(&instance, physical_device) };
        info!(
            "Vulkan 进程级上下文就绪: API v{}.{}.{}  设备 {dev_name} ({:?})  图形队列族 {queue_family_index}  transfer: 族 {transfer_queue_family_index}（{transfer_note}）  features[支持→已启用]: timelineSemaphore {}→on  dynamicRendering {}→on  验证层 {}",
            vk::api_version_major(dev_props.api_version),
            vk::api_version_minor(dev_props.api_version),
            vk::api_version_patch(dev_props.api_version),
            dev_props.device_type,
            vk12_query.timeline_semaphore != 0,
            vk13_query.dynamic_rendering != 0,
            if validation { "on" } else { "未找到" },
        );

        Ok(Self {
            entry,
            instance,
            surface_fns,
            surface,
            debug,
            physical_device,
            device,
            queue_family_index,
            queue,
            transfer_queue_family_index,
            transfer_queue,
            memory_contract,
        })
    }

    /// 冻结的内存契约(类型选择/atom 舍入的唯一裁判,3.2.2.1 定案)。
    #[must_use]
    pub fn memory_contract(&self) -> &crate::vulkan::MemoryContract {
        &self.memory_contract
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // 手动反序拆除（ash 0.38 无自动 Drop）。device_wait_idle 在正常退出里只是
        // 兜底——teardown_vulkan 已在销毁任何 GPU 资源前排空过队列（D4：等待必须
        // 先于销毁，本 Drop 的等待保护不了早已拆掉的兄弟资源）；它真正服务的是
        // panic 清场这类拆毁顺序不定、无人排空的路径。swapchain/image view 在
        // Swapchain 的 Drop 里，不归本结构管。
        unsafe {
            let _ = self.device.device_wait_idle();
            self.surface_fns.destroy_surface(self.surface, None);
            if let Some((loader, messenger)) = self.debug.take() {
                loader.destroy_debug_utils_messenger(messenger, None);
            }
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
