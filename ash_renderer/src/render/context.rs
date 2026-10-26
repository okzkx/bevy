//! ash Vulkan 进程级上下文：Entry → Instance(+验证层) →
//! Win32 Surface → PhysicalDevice → Device(+dynamicRendering+timelineSemaphore) →
//! Queue（图形 + transfer，同族则合一）。
//!
//! 生命周期四层（详见 .agents/docs/2-宿主壳/材料/VulkanContext字段释义：从Entry到Swapchain.md §12）：
//! - 本结构 = 进程级（随进程活）+ Surface（窗口级，单窗宿主壳中并入）；
//! - resize 级的 Swapchain 与帧级的命令缓冲/fence/信号量在 `swapchain` 模块与
//!   `pipeline::frames`。
//!
//! 其余约束：
//! - 句柄取自 `RawHandleWrapper::get_window_handle()`（bevy 的安全方法），Win32 路径
//!   手写 `vkCreateWin32SurfaceKHR`；hinstance 可能为 None，`GetModuleHandleW(None)` 兜底；
//! - ash 0.38.0 对 Instance/Device/loader 均无 Drop 实现——销毁全部手动，Drop 序
//!   Surface → Messenger → Device → Instance（Surface/Messenger 是 instance 级对象、
//!   与 Device 无依赖，先拆合法；Instance 必须最后）；
//! - `Entry` 内持 libloading 句柄，提前丢它会卸载 vulkan-1.dll，必须与 Instance 同寿命；
//! - 正常退出走 `driver::host` 的 `teardown_vulkan`（OnAppExitSystems）：先
//!   `device_wait_idle`，再按声明序移除资源，本结构最后移除触发 Drop。runner
//!   `exiting` 清场序对 Resource 是任意的，Drop 内的 `device_wait_idle` 兜这类无人
//!   排空的路径。

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

/// 一个内存堆的预算读数（VK_EXT_memory_budget 查询展开）。
#[derive(Clone, Copy, Debug)]
pub struct HeapBudgetReading {
    /// 是否设备局部堆（显存）；宿主堆为 `false`（PCIe 上传常见）。
    pub device_local: bool,
    /// 堆可用预算（字节，驱动会留余量，小于堆真实容量）。
    pub budget: u64,
    /// 堆当前用量（字节）——**系统级**语义（含其他进程），非本进程显存。
    pub usage: u64,
}

/// 进程级 Vulkan 上下文。字段顺序即声明层级；`Drop` 手动反序（ash 无自动 Drop）。
#[derive(bevy::prelude::Resource)]
pub struct Context {
    /// 持 libloading 句柄防 vulkan-1.dll 提前卸载（所有 fn 指针已在 new 时拷走，字段本身不被读）
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
    /// transfer 队列族与队列：拷贝提交走这里，与图形提交互不排队。
    /// 没有"TRANSFER 且无 GRAPHICS"的专用族时与 graphics 同族同队列（合批退回，
    /// 下游比对两个族号即可分辨）。
    pub transfer_queue_family_index: u32,
    pub transfer_queue: vk::Queue,
    /// 内存契约（设备侧事实一次查询冻结：类型/堆表 + nonCoherentAtomSize），
    /// 选型唯一裁判。池/上传按引用取用，不各自重查。
    memory_contract: crate::render::MemoryContract,
    /// VK_EXT_memory_budget 是否已加挂（设备创建时按支持情况决定）。
    budget_enabled: bool,
}

impl Context {
    pub fn new(wrapper: &bevy::window::RawHandleWrapper) -> Result<Self> {
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
        // fence/信号量的 signal-wait 配对、present 复用、在飞销毁，这正是
        // 同步验证的执法面。同步验证有帧时间开销，
        // 若受限，把 enables 换成空数组即降回核心检查。
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

        // ---- Surface（手写 Win32 路径）----
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
                    // COMPUTE 是选族硬条件（剔除等 compute dispatch 跑图形族，
                    // 同队列最简——无跨队列信号量）：compute 是 1.0 core 无 feature
                    // 位，但"支持"必须在此钉成硬条件（1.3 基线同纪律：不达标的
                    // 设备出局，比创建后 dispatch 炸了便宜）
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

        // ---- transfer 专用队列族：拷贝提交与图形提交分流（合批上传的载体）----
        // 条件：有 TRANSFER 且无 GRAPHICS 的独立族（纯 DMA 引擎，不与图形
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
        // 支持与启用是两件事：features2 查询回答"驱动支持吗"，
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
        // base features 支持位在此快照（features2 借用 vk12/vk13 查询结构，
        // 借用链活到哪，字段就能读到哪——日志段用快照值）
        let multi_draw_indirect_supported = features2.features.multi_draw_indirect != 0;
        // 与 1.3 基线同款的硬校验：要用而驱动不支持 = 出局，不创建半残设备
        if vk12_query.timeline_semaphore == 0 || vk13_query.dynamic_rendering == 0 {
            return Err(VulkanError::Init(format!(
                "物理设备 feature 不足：timelineSemaphore 支持={} dynamicRendering 支持={}（1.3 实现必须为 1）",
                vk12_query.timeline_semaphore, vk13_query.dynamic_rendering
            )));
        }
        // 硬校验：bindless 路线的五个必需位（总开关不解锁任何行为，具体位
        // 缺哪个都走不通）。缺少必要能力时明确报错，不建传统
        // bound 回退渲染器——缺一即出局，与 1.3 基线同款待遇。
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
        // 硬校验：indirect 绘制能力。multi_draw_indirect 挂在
        // 1.0 起的 base feature 位（VkPhysicalDeviceFeatures）——"1.1/1.2 core
        // 晋升"只收编命令不取消启用位（timeline 同理），"查询为 true ≠ 已启用"
        // 纪律照旧：未启用时调 cmd_draw_indexed_indirect(drawCount>1) 直接违
        // VUID-02718。缺位 = 出局（1.3 基线同款待遇）。
        if !multi_draw_indirect_supported {
            return Err(VulkanError::Init(
                "物理设备 feature 不足：multi_draw_indirect 不支持（5.3 indirect 闭环无逐 draw 回退路径，该设备出局）".into(),
            ));
        }
        // 20 个分位的支持值全量落日志——"不启用≠不查"，要用哪位
        //（SSBO 表/variable count…）的证据都在这行里
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
        // 显式启用本工程用到的每个 feature 位，不依赖"查询为 true"的惯性
        let mut vulkan12 = vk::PhysicalDeviceVulkan12Features::default()
            .timeline_semaphore(true)
            .descriptor_indexing(true)
            .runtime_descriptor_array(true)
            .shader_sampled_image_array_non_uniform_indexing(true)
            .descriptor_binding_sampled_image_update_after_bind(true)
            .descriptor_binding_partially_bound(true);
        let mut vulkan13 = vk::PhysicalDeviceVulkan13Features::default().dynamic_rendering(true);
        // base features 启用位（DeviceCreateInfo 的 features 字段）：
        // multi_draw_indirect 是 indirect 闭环的启用位
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
        // 输出编码定案：swapchain 保持
        // B8G8R8A8_UNORM 底板（DWM 母语），渲染附件用同图 B8G8R8A8_SRGB view 别名
        // （wgpu/bevy_render 同构：非 SRGB surface 套 sRGB view）。view 换格式需要
        // image 带 MUTABLE_FORMAT——swapchain image 上它由本扩展 + 格式清单兑现
        //（swapchain.rs 传 VkImageFormatListCreateInfo）。blend 仍在编码前的线性域
        //（SRGB 的 ROP 编码在 blend 之后）。不支持 = 出局：本机验证支持与启用分开。
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

        // 显存预算读数：设备扩展按支持情况加挂，缺席降级为隐藏读数
        //（监控面板显存行不可用，不报错不阻塞帧循环）。
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
            budget_enabled,
        })
    }

    /// 冻结的内存契约(类型选择/atom 舍入的唯一裁判)。
    #[must_use]
    pub fn memory_contract(&self) -> &crate::render::MemoryContract {
        &self.memory_contract
    }

    /// 各内存堆的 budget/usage 一次读（VK_EXT_memory_budget；监控面板数据源）。
    /// 扩展未加挂时返回 `None`。每次调用现查（properties2 纯查询，0.5s 采样节奏
    /// 下开销可忽略）；`heap_usage` 是堆的系统级用量（含其他进程），不是本进程
    /// 显存——进程自身占用看渲染器内部账（MeshPool/ImageCache）并列呈现。
    pub fn memory_budget(&self) -> Option<Vec<HeapBudgetReading>> {
        if !self.budget_enabled {
            return None;
        }
        let mut budget = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
        let mut props = vk::PhysicalDeviceMemoryProperties2::default().push_next(&mut budget);
        // # Safety:physical_device 来自成功枚举、instance 未销毁(同 memory_contract)
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

    /// 物理设备 limits(进程级事实,设备存活期恒定;indirect 上界等留痕口)。
    pub fn limits(&self) -> vk::PhysicalDeviceLimits {
        // # Safety:physical_device 来自成功枚举、instance 未销毁(同 memory_contract)
        unsafe { self.instance.get_physical_device_properties(self.physical_device) }.limits
    }

    /// transfer 写 + graphics 读的资源(池 buffer/贴图)共享族集合:有专用 transfer 族
    /// (≠ graphics)时返回两族(资源按 CONCURRENT 创建,免所有权乒乓),同族设备返回
    /// 空(EXCLUSIVE)。
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
        // 手动反序拆除（ash 0.38 无自动 Drop）。坑：device_wait_idle 在正常退出里
        // 只是兜底——teardown_vulkan 已在销毁任何 GPU 资源前排空过队列（等待必须
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
