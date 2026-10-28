//! 进程级 Vulkan 上下文：Entry → Instance（+验证层）→ Win32 Surface → PhysicalDevice →
//! Device → 图形队列 + transfer 队列。
//!
//! 只管进程级对象和窗口 Surface；Swapchain 在 `swapchain` 模块，帧级命令缓冲、fence、
//! 信号量在 `frame::frame_pool`。各字段释义与生命周期分层见
//! `.agents/docs/2-宿主壳/材料/VulkanContext字段释义：从Entry到Swapchain.md` §12。

use std::ffi::{c_char, c_void};
use std::num::NonZeroIsize;

use ash::{
    ext::debug_utils,
    khr::{surface, win32_surface},
    vk, Device, Entry, Instance,
};
use bevy::log::{error, info};
use raw_window_handle::RawWindowHandle;

use crate::common::error::{Result, VulkanError};

const VALIDATION_LAYER: &std::ffi::CStr = c"VK_LAYER_KHRONOS_validation";

#[cfg(windows)]
unsafe extern "system" {
    fn GetModuleHandleW(lp_module_name: *const u16) -> isize;
}

/// 验证层回调：WARNING/ERROR 打到 stderr。返回 FALSE 表示不中断触发报告的那次 Vulkan 调用。
///
/// # Safety
/// 只由 Vulkan 按回调约定调用，`p_callback_data` 为合法指针或空。
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

/// 一个内存堆的显存预算读数。
#[derive(Clone, Copy, Debug)]
pub struct HeapBudgetReading {
    /// 是否显存堆（DEVICE_LOCAL）；`false` 是系统内存堆。
    pub device_local: bool,
    /// 堆可用预算（字节）；驱动会留余量，小于堆的真实容量。
    pub budget: u64,
    /// 堆当前用量（字节）。坑：这是整堆的系统级用量，含其他进程，不是本进程占用。
    pub usage: u64,
}

/// `Context::new` 中途失败时的回滚护栏：析构时反序销毁已建成的对象；建成后置 `committed` 跳过。
struct ContextInitGuard {
    instance: Instance,
    debug: Option<(debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
    surface_fns: Option<surface::Instance>,
    surface: vk::SurfaceKHR,
    device: Option<Device>,
    committed: bool,
}

impl Drop for ContextInitGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        unsafe {
            if let Some(device) = self.device.take() {
                let _ = device.device_wait_idle();
                device.destroy_device(None);
            }
            if self.surface != vk::SurfaceKHR::null()
                && let Some(surface_fns) = &self.surface_fns
            {
                surface_fns.destroy_surface(self.surface, None);
            }
            if let Some((loader, messenger)) = self.debug.take() {
                loader.destroy_debug_utils_messenger(messenger, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

/// CPU 领先 GPU 的最大帧数：第 N 帧要等第 N-2 帧的资源空出来才能重录。所有按帧槽
/// 轮转的资源（命令缓冲、UBO、各种环）都按它分配份数。
pub const MAX_FRAMES_IN_FLIGHT: usize = 2;

/// 进程级 Vulkan 上下文。ash 的 Instance/Device 没有自动 Drop，由本类型的 `Drop` 手动销毁。
#[derive(bevy::prelude::Resource)]
pub struct Context {
    /// 坑：持有 vulkan-1.dll 的加载句柄，提前丢掉会卸载 DLL。字段本身不被读，只为保活。
    #[expect(dead_code, reason = "entry 仅作生命周期锚：Library 在即 Vulkan 可用")]
    entry: Entry,
    pub instance: Instance,
    pub surface_fns: surface::Instance,
    pub surface: vk::SurfaceKHR,
    debug: Option<(debug_utils::Instance, vk::DebugUtilsMessengerEXT)>,
    pub physical_device: vk::PhysicalDevice,
    pub device: Device,
    /// 图形队列族：同时支持 GRAPHICS、COMPUTE 和 present。
    pub queue_family_index: u32,
    pub queue: vk::Queue,
    /// 拷贝提交走的队列，不和图形提交排队。没有专用 transfer 族时就是图形队列本身，
    /// 比较两个族号即可分辨。
    pub transfer_queue_family_index: u32,
    pub transfer_queue: vk::Queue,
    /// 内存类型/堆表与 nonCoherentAtomSize，建设备时查一次；内存选型都从这里取，不各自重查。
    memory_contract: crate::render::MemoryContract,
    /// VK_EXT_debug_utils 的设备侧 loader（对象命名用；扩展常开）。
    debug_device: debug_utils::Device,
    /// 是否启用了 VK_EXT_memory_budget（设备支持才启用）。
    budget_enabled: bool,
}

impl Context {
    /// 给 Vulkan 对象挂调试名（best-effort：失败只影响报错可读性，不致命）。
    /// 对象类型从句柄类型推导（ash 的 Handle trait）。
    pub(crate) fn name_object<H: vk::Handle>(&self, handle: H, name: &str) {
        let c = match std::ffi::CString::new(name) {
            Ok(c) => c,
            Err(_) => return,
        };
        let info = vk::DebugUtilsObjectNameInfoEXT::default().object_handle(handle).object_name(c.as_c_str());
        let _ = unsafe { self.debug_device.set_debug_utils_object_name(&info) };
    }
}

impl Context {
    /// 依次建好全部对象；任一步失败返回错误，已建成的由 `ContextInitGuard` 回滚。
    pub fn new(wrapper: &bevy::window::RawHandleWrapper) -> Result<Self> {
        // ---- 窗口句柄（只支持 Win32；hinstance 为空时取本进程模块句柄）----
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

        // unsafe：动态加载 vulkan-1.dll（Windows 驱动自带），加载到的符号无法校验
        let entry = unsafe { Entry::load() }
            .map_err(|e| VulkanError::Init(format!("加载 Vulkan loader 失败: {e}")))?;

        // 有验证层就开，Vulkan 误用当场报出；找不到就记错误日志后照常运行
        let validation = unsafe { entry.enumerate_instance_layer_properties() }
            .unwrap_or_default()
            .iter()
            .any(|props| unsafe { std::ffi::CStr::from_ptr(props.layer_name.as_ptr()) } == VALIDATION_LAYER);
        if !validation {
            error!("未找到 {VALIDATION_LAYER:?}，验证层未启用");
        }

        // ---- Instance ----
        // debug_utils 常开（被动扩展）：对象命名供验证层报错与 RenderDoc 按名字显示
        let ext_names: Vec<*const c_char> =
            vec![surface::NAME.as_ptr(), win32_surface::NAME.as_ptr(), debug_utils::NAME.as_ptr()];
        let layer_names: Vec<*const c_char> = if validation {
            vec![VALIDATION_LAYER.as_ptr()]
        } else {
            Vec::new()
        };

        let app_name = c"ash_renderer";
        let app_info = vk::ApplicationInfo::default()
            .application_name(app_name)
            .api_version(vk::API_VERSION_1_3);
        // 同步验证：额外检查信号量配对、present 复用、在飞销毁等跨对象时序，核心检查管不到这些。
        // 有帧时间开销；要关掉就把 enables 换成空数组。
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
            // 坑：enables 和 sync_validation 必须活到 create_instance 返回（pNext 只存指针）
            sync_validation = sync_validation.enabled_validation_features(enables);
            instance_info = instance_info.push_next(&mut sync_validation);
        }
        let instance = unsafe { entry.create_instance(&instance_info, None) }
            .map_err(|e| VulkanError::Init(format!("vkCreateInstance 失败: {e}")))?;
        let mut guard = ContextInitGuard {
            instance: instance.clone(),
            debug: None,
            surface_fns: None,
            surface: vk::SurfaceKHR::null(),
            device: None,
            committed: false,
        };

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
            let debug = (loader, messenger);
            guard.debug = Some((debug.0.clone(), debug.1));
            Some(debug)
        } else {
            None
        };

        // ---- Surface（Win32）----
        let surface_fns = surface::Instance::new(&entry, &instance);
        guard.surface_fns = Some(surface_fns.clone());
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
        guard.surface = surface;

        // ---- PhysicalDevice：要求 Vulkan 1.3 + 一个同时支持 graphics/compute/present 的队列族；离散显卡优先 ----
        let physical_devices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| VulkanError::Init(format!("枚举物理设备失败: {e}")))?;
        let mut best: Option<(u32, u32, vk::PhysicalDevice)> = None; // (score, family_index, pd)
        for pd in physical_devices {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            // 低于 1.3 的设备跳过并记日志：后面的 1.3 feature 结构不能交给 1.2 设备。全部跳过时报错退出
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
                    // 剔除等 compute pass 和图形提交共用这个队列（省掉跨队列信号量），所以 COMPUTE 是硬条件
                    f.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                        && f.queue_flags.contains(vk::QueueFlags::COMPUTE)
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
                "没有报支持 Vulkan 1.3 且 graphics+present+compute 同支持的物理设备".into(),
            ));
        };

        // ---- transfer 专用队列族：有 TRANSFER、无 GRAPHICS 的独立拷贝引擎 ----
        // 很多桌面 GPU 没有，此时退回图形族；不为它改变上面选定的设备。
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

        // ---- Device：先查支持，再显式启用要用的 feature ----
        // 坑：驱动支持不等于已启用，用到的 feature 必须在建设备时显式开启，否则验证层报错
        //（如 VUID-VkSemaphoreTypeCreateInfo-timelineSemaphore-03252）。1.3 设备的 timeline 启用位仍在 Vulkan12Features 里。
        let mut vk12_query = vk::PhysicalDeviceVulkan12Features::default();
        let mut vk13_query = vk::PhysicalDeviceVulkan13Features::default();
        let mut features2 = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut vk12_query)
            .push_next(&mut vk13_query);
        unsafe { instance.get_physical_device_features2(physical_device, &mut features2) };
        // features2 还借着 vk12/vk13_query，先把要用的 base feature 读出来，之后才能直接读那两个结构
        let multi_draw_indirect_supported = features2.features.multi_draw_indirect != 0;
        // 要用的 feature 不支持就报错退出，不建缺能力的设备
        if vk12_query.timeline_semaphore == 0 || vk13_query.dynamic_rendering == 0 {
            return Err(VulkanError::Init(format!(
                "物理设备 feature 不足：timelineSemaphore 支持={} dynamicRendering 支持={}（1.3 实现必须为 1）",
                vk12_query.timeline_semaphore, vk13_query.dynamic_rendering
            )));
        }
        // bindless 必需的五个位，缺一个就报错退出；不提供传统绑定的回退路径
        let required_di: [(&str, u32); 5] = [
            ("descriptor_indexing", vk12_query.descriptor_indexing),
            (
                "runtime_descriptor_array",
                vk12_query.runtime_descriptor_array,
            ),
            (
                "shader_sampled_image_array_non_uniform_indexing",
                vk12_query.shader_sampled_image_array_non_uniform_indexing,
            ),
            (
                "descriptor_binding_sampled_image_update_after_bind",
                vk12_query.descriptor_binding_sampled_image_update_after_bind,
            ),
            (
                "descriptor_binding_partially_bound",
                vk12_query.descriptor_binding_partially_bound,
            ),
        ];
        if let Some((name, _)) = required_di.iter().find(|(_, v)| *v == 0) {
            return Err(VulkanError::Init(format!(
                "物理设备 descriptor indexing 特性不足：{name} 不支持（bindless 路线无传统回退，该设备出局）"
            )));
        }
        // indirect 一次多 draw 必需，没有逐 draw 回退路径。
        // 坑：未启用时 drawCount>1 的 cmd_draw_indexed_indirect 违反 VUID-vkCmdDrawIndexedIndirect-drawCount-02718
        if !multi_draw_indirect_supported {
            return Err(VulkanError::Init(
                "物理设备 feature 不足：multi_draw_indirect 不支持（5.3 indirect 闭环无逐 draw 回退路径，该设备出局）".into(),
            ));
        }
        // 20 个分位的支持情况全部记日志，以后要多启用哪一位先查这行
        let di_all: [(&str, u32); 20] = [
            ("inAttArrDynIdx", vk12_query.shader_input_attachment_array_dynamic_indexing),
            ("uniTexelArrDynIdx", vk12_query.shader_uniform_texel_buffer_array_dynamic_indexing),
            ("stoTexelArrDynIdx", vk12_query.shader_storage_texel_buffer_array_dynamic_indexing),
            ("uniBufArrNonUni", vk12_query.shader_uniform_buffer_array_non_uniform_indexing),
            ("smpImgArrNonUni", vk12_query.shader_sampled_image_array_non_uniform_indexing),
            ("stoBufArrNonUni", vk12_query.shader_storage_buffer_array_non_uniform_indexing),
            ("stoImgArrNonUni", vk12_query.shader_storage_image_array_non_uniform_indexing),
            ("inAttArrNonUni", vk12_query.shader_input_attachment_array_non_uniform_indexing),
            ("uniTexelArrNonUni", vk12_query.shader_uniform_texel_buffer_array_non_uniform_indexing),
            ("stoTexelArrNonUni", vk12_query.shader_storage_texel_buffer_array_non_uniform_indexing),
            ("uniBufUAB", vk12_query.descriptor_binding_uniform_buffer_update_after_bind),
            ("smpImgUAB", vk12_query.descriptor_binding_sampled_image_update_after_bind),
            ("stoImgUAB", vk12_query.descriptor_binding_storage_image_update_after_bind),
            ("stoBufUAB", vk12_query.descriptor_binding_storage_buffer_update_after_bind),
            ("uniTexelUAB", vk12_query.descriptor_binding_uniform_texel_buffer_update_after_bind),
            ("stoTexelUAB", vk12_query.descriptor_binding_storage_texel_buffer_update_after_bind),
            ("updUnusedPending", vk12_query.descriptor_binding_update_unused_while_pending),
            ("partiallyBound", vk12_query.descriptor_binding_partially_bound),
            ("varDescCount", vk12_query.descriptor_binding_variable_descriptor_count),
            ("runtimeDescArray", vk12_query.runtime_descriptor_array),
        ];
        info!(
            "descriptor indexing 分位支持(20/20): {}",
            di_all.iter().map(|(n, v)| format!("{n}={}", *v != 0)).collect::<Vec<_>>().join(" ")
        );
        // 逐位显式启用用到的 feature
        let mut vulkan12 = vk::PhysicalDeviceVulkan12Features::default()
            .timeline_semaphore(true)
            .descriptor_indexing(true)
            .runtime_descriptor_array(true)
            .shader_sampled_image_array_non_uniform_indexing(true)
            .descriptor_binding_sampled_image_update_after_bind(true)
            .descriptor_binding_partially_bound(true);
        let mut vulkan13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true);
        // multi_draw_indirect 在 base features 里，不在 Vulkan12/13 结构中
        let base_features = vk::PhysicalDeviceFeatures::default().multi_draw_indirect(true);
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
        // swapchain 用 B8G8R8A8_UNORM 图像，渲染时套同一张图的 B8G8R8A8_SRGB view 写入。
        // 坑：view 换格式要求图像带 MUTABLE_FORMAT，swapchain 图像靠本扩展加格式清单（见 swapchain.rs）实现；不支持就报错退出
        let device_ext_props = unsafe { instance.enumerate_device_extension_properties(physical_device) }
            .map_err(|e| VulkanError::Init(format!("枚举设备扩展失败: {e}")))?;
        let has_mutable_format = device_ext_props.iter().any(|p| unsafe {
            std::ffi::CStr::from_ptr(p.extension_name.as_ptr()) == ash::khr::swapchain_mutable_format::NAME
        });
        if !has_mutable_format {
            return Err(VulkanError::Init(
                "设备不支持 VK_KHR_swapchain_mutable_format——输出编码定案（UNORM 底板 + SRGB view 别名）无法落地，出局".into(),
            ));
        }

        // 显存预算扩展可选：不支持就不开，只是监控面板隐藏显存行，不影响渲染
        let budget_enabled = unsafe { instance.enumerate_device_extension_properties(physical_device) }
            .unwrap_or_default()
            .iter()
            .any(|props| unsafe { std::ffi::CStr::from_ptr(props.extension_name.as_ptr()) }
                == ash::ext::memory_budget::NAME);
        let mut dev_exts = vec![
            ash::khr::swapchain::NAME.as_ptr(),
            ash::khr::swapchain_mutable_format::NAME.as_ptr(),
        ];
        if budget_enabled {
            dev_exts.push(ash::ext::memory_budget::NAME.as_ptr());
        } else {
            info!("VK_EXT_memory_budget 设备不支持：显存 budget/usage 读数不可用（监控面板隐藏该行）");
        }
        let device = unsafe {
            instance.create_device(
                physical_device,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_infos)
                    .enabled_extension_names(&dev_exts)
                    .enabled_features(&base_features)
                    .push_next(&mut vulkan12)
                    .push_next(&mut vulkan13),
                None,
            )
        }
        .map_err(|e| VulkanError::Init(format!("vkCreateDevice 失败: {e}")))?;
        guard.device = Some(device.clone());
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
        // Safety：physical_device 来自枚举结果，instance 仍存活
        let memory_contract =
            unsafe { crate::render::MemoryContract::new(&instance, physical_device) };
        info!(
            "Vulkan 进程级上下文就绪: API v{}.{}.{}  设备 {dev_name} ({:?})  图形队列族 {queue_family_index}（GRAPHICS+COMPUTE+PRESENT，5.2 前置闸门①实测钉明）  transfer: 族 {transfer_queue_family_index}（{transfer_note}）  features[支持→已启用]: timelineSemaphore {}→on  dynamicRendering {}→on  descriptorIndexing+4 位 {}→on  multiDrawIndirect {}→on（5.3 闸门①）  maxDrawIndirectCount {}（5.3 闸门③留痕）  swapchain_mutable_format on  synchronization2 off（屏障走传统形状）  验证层 {}",
            vk::api_version_major(dev_props.api_version),
            vk::api_version_minor(dev_props.api_version),
            vk::api_version_patch(dev_props.api_version),
            dev_props.device_type,
            vk12_query.timeline_semaphore != 0,
            vk13_query.dynamic_rendering != 0,
            vk12_query.descriptor_indexing != 0,
            multi_draw_indirect_supported,
            dev_props.limits.max_draw_indirect_count,
            if validation { "on" } else { "未找到" },
        );

        let debug_device = debug_utils::Device::new(&instance, &device);
        let context = Self {
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
            budget_enabled,
            debug_device,
        };
        guard.committed = true;
        Ok(context)
    }

    /// 内存契约：内存类型选择和 atom 对齐都以它为准。
    #[must_use]
    pub fn memory_contract(&self) -> &crate::render::MemoryContract {
        &self.memory_contract
    }

    /// 读各内存堆的 budget/usage；未启用 VK_EXT_memory_budget 时返回 `None`。每次调用现查，开销可忽略。
    pub fn memory_budget(&self) -> Option<Vec<HeapBudgetReading>> {
        if !self.budget_enabled {
            return None;
        }
        let mut budget = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
        let mut props = vk::PhysicalDeviceMemoryProperties2::default().push_next(&mut budget);
        // Safety：同 new 里建 memory_contract 处
        unsafe { self.instance.get_physical_device_memory_properties2(self.physical_device, &mut props) };
        let heaps = &props.memory_properties;
        Some(
            heaps.memory_heaps[..heaps.memory_heap_count as usize]
                .iter()
                .zip(&budget.heap_budget)
                .zip(&budget.heap_usage)
                .map(|((heap, &budget), &usage)| HeapBudgetReading {
                    device_local: heap.flags.contains(vk::MemoryHeapFlags::DEVICE_LOCAL),
                    budget,
                    usage,
                })
                .collect(),
        )
    }

    /// 物理设备 limits，设备存活期内不变。
    pub fn limits(&self) -> vk::PhysicalDeviceLimits {
        // Safety：同 new 里建 memory_contract 处
        unsafe { self.instance.get_physical_device_properties(self.physical_device) }.limits
    }

    /// transfer 写、graphics 读的资源（池 buffer、贴图）创建时用的共享族列表。
    /// 有专用 transfer 族时返回两个族号，资源按 CONCURRENT 创建，免去所有权转移；同族时返回空，按 EXCLUSIVE 创建。
    #[must_use]
    pub fn sharing_families(&self) -> Vec<u32> {
        if self.transfer_queue_family_index != self.queue_family_index {
            vec![self.transfer_queue_family_index, self.queue_family_index]
        } else {
            Vec::new()
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // 按依赖反序销毁：Surface 和 Messenger 不依赖 Device，先拆也合法；Instance 必须最后。
        // 坑：这里等 GPU 已经太晚，保护不了先拆掉的其他资源。正常退出靠 teardown_vulkan 先等
        // GPU 空闲；这里只兜 panic 清场这类没人等 GPU 的路径。
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
