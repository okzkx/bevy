//! ash Vulkan 进程级上下文（step2 施工③④拆分）：Entry → Instance(+验证层) →
//! Win32 Surface → PhysicalDevice → Device(+dynamicRendering) → Queue。
//!
//! 生命周期四层（详见 .agent/docs/step2-宿主壳/VulkanContext字段释义：从Entry到Swapchain.md §12）：
//! - 本结构 = 进程级（随进程活）+ Surface（窗口级，单窗宿主壳中并入）；
//! - resize 级的 Swapchain 已拆去 [`crate::swapchain`]；
//! - 帧级的命令缓冲/fence/信号量在 [`crate::frames`]。
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
}

impl Context {
    pub fn new(wrapper: &bevy::window::RawHandleWrapper) -> Result<Self, VulkanError> {
        // ---- 窗口句柄（Win32；hinstance 缺失时进程句柄兜底）----
        let (hwnd, hinstance) = match wrapper.get_window_handle() {
            RawWindowHandle::Win32(w) => (
                w.hwnd.get() as isize,
                w.hinstance.map_or_else(
                    || unsafe { GetModuleHandleW(std::ptr::null()) },
                    |v| v.get() as isize,
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
        let layer_names: Vec<*const c_char> =
            validation.then(|| vec![VALIDATION_LAYER.as_ptr()]).unwrap_or_default();

        let app_name = c"ash_renderer";
        let app_info = vk::ApplicationInfo::default()
            .application_name(app_name)
            .api_version(vk::API_VERSION_1_3);
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app_info)
                    .enabled_extension_names(&ext_names)
                    .enabled_layer_names(&layer_names),
                None,
            )
        }
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
            let messenger = unsafe { loader.create_debug_utils_messenger(&messenger_info, None) }
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
                &vk::Win32SurfaceCreateInfoKHR::default().hinstance(hinstance).hwnd(hwnd),
                None,
            )
        }
        .map_err(|e| VulkanError::Init(format!("vkCreateWin32SurfaceKHR 失败: {e}")))?;

        // ---- PhysicalDevice：graphics+present 同族，离散卡优先 ----
        let physical_devices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| VulkanError::Init(format!("枚举物理设备失败: {e}")))?;
        let mut best: Option<(u32, u32, vk::PhysicalDevice)> = None; // (score, family_index, pd)
        for pd in physical_devices {
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
            let score = match unsafe { instance.get_physical_device_properties(pd) }.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 1000,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 100,
                _ => 1,
            };
            if best.is_none_or(|(s, _, _)| score > s) {
                best = Some((score, family, pd));
            }
        }
        let Some((_score, queue_family_index, physical_device)) = best else {
            return Err(VulkanError::Init("没有 graphics+present 同支持的物理设备".into()));
        };

        // ---- Device：单个通用队列族 + swapchain 扩展 + 1.3 dynamicRendering ----
        // 动态渲染是帧循环清屏的载体（swapchain.rs 录制部分）：不用建 RenderPass/Framebuffer，
        // 但它是 1.3 feature，必须在 vkCreateDevice 里显式开启
        let mut vulkan13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true);
        let mut queue_priority = [1.0f32];
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&mut queue_priority);
        let device_exts = [ash::khr::swapchain::NAME.as_ptr()];
        let device = unsafe {
            instance.create_device(
                physical_device,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(std::slice::from_ref(&queue_info))
                    .enabled_extension_names(&device_exts)
                    .push_next(&mut vulkan13),
                None,
            )
        }
        .map_err(|e| VulkanError::Init(format!("vkCreateDevice 失败: {e}")))?;
        let queue = unsafe { device.get_device_queue(queue_family_index, 0) };

        let dev_props = unsafe { instance.get_physical_device_properties(physical_device) };
        let dev_name = unsafe { std::ffi::CStr::from_ptr(dev_props.device_name.as_ptr()) }.to_string_lossy();
        info!(
            "Vulkan 进程级上下文就绪: API v{}.{}.{}  设备 {dev_name} ({:?})  队列族 {queue_family_index}",
            vk::api_version_major(dev_props.api_version),
            vk::api_version_minor(dev_props.api_version),
            vk::api_version_patch(dev_props.api_version),
            dev_props.device_type,
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
        })
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // 手动反序拆除（ash 0.38 无自动 Drop）：先等队列安静，再按依赖逆序拆。
        // 注意 swapchain/image view 不在这里——它们在 Swapchain 的 Drop 里，且正常退出
        // 时 teardown_vulkan 已保证 FramePool → Swapchain → 本结构 的反序。
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
