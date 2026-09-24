//! 3.2.2.1 内存契约探针:验证层执法下的契约正路观察 + 绑定负例诊断。
//!
//! 与 timeline_probe v2 同纪律:两组是"观察 + 诊断",不是规范裁决——
//! - 组 A(合同正路观察组):契约三层各验一遍——
//!   ① 类型选择:按角色的必需/优先属性选型,打印设备内存类型/堆表与选中结果
//!   (staging 期望 HOST_VISIBLE、pool 期望 DEVICE_LOCAL,contract.find_type 担保);
//!   ② 绑定:恰按 requirements.size 分配、offset 0 绑定(四条绑定条款由流程满足);
//!   ③ 读写与 coherent 分支:两段非对齐写 → host 读回校验;再用一次最小 copyBuffer
//!   闭环(staging→device→readback,fence 等待后读回)——flush/invalidate 的可见性
//!   语义只有在设备真抄写一遍后才可观察。全部范围断言逐字节,不只对总字节。
//! - 组 B(负例诊断组):memoryOffset 不是 requirements.alignment 的整数倍
//!   直接绑定,只收验证层的预期诊断(对齐条款:01036;SDK 1.4 层按新修订报
//!   VUID-vkBindBufferMemory-None-10739,QCOM tile 例外措辞,同一条款),
//!   不期待驱动必返回固定错误码,绑定即拆,不当正常路径继续用。
//!
//! 本探针不碰窗口/surface,自含 Entry → Instance → PhysicalDevice → Device 最小链,
//! 用 [`ash_renderer::vulkan`] 的契约类型本体(测的是交付代码,不是探针复制品)。
//! 与主工程 device 的差异:本探针不链 12/13 feature(内存路径不涉及这些位),
//! 用旧 SubmitInfo + 旧 pipeline barrier(不启用 synchronization2 就不用 submit2,
//! 接口定案归 3.2.4);同步验证照主工程常开。
//!
//! 销毁纪律与 timeline_probe 同款可证明:fence wait 返回 = 拷贝提交执行完、
//! 无在途引用,此刻拆 buffer/command pool 合法。

use std::slice;
use std::sync::Mutex;

use ash::{ext::debug_utils, vk, Device, Entry};
use ash_renderer::vulkan::{align_up, BufferRole, GpuBuffer, MemoryContract};

const VALIDATION_LAYER: &std::ffi::CStr = c"VK_LAYER_KHRONOS_validation";

/// 区域 A:[0, 65536)——整段 64KiB,atom 对齐零头;
/// 区域 B:[70000, 70016)——起点非 atom 对齐(70000 % 64 = 48),专考 flush 舍入。
const REGION_A: (u64, usize) = (0, 65536);
const REGION_B: (u64, usize) = (70_000, 16);

/// 验证层消息收账:WARNING/ERROR 全记,组间分账、探针结束统一打印。
static VU_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 区域内字节模式:与位置相关的确定性序列(内容校验用,不只对总字节数)。
fn pattern(idx: u64) -> u8 {
    (idx as u8).wrapping_mul(31).wrapping_add(7)
}

fn main() {
    let entry = unsafe { Entry::load() }.expect("加载 Vulkan loader");

    // 验证层三件事分开报:装没装、请求没请求、收没收到 VUID(结尾分账)。
    let validation_installed = unsafe { entry.enumerate_instance_layer_properties() }
        .unwrap_or_default()
        .iter()
        .any(|props| unsafe { std::ffi::CStr::from_ptr(props.layer_name.as_ptr()) } == VALIDATION_LAYER);
    println!(
        "验证层 {VALIDATION_LAYER:?}: {}",
        if validation_installed {
            "已安装,本探针请求启用 + 同步验证(VUID 收账见结尾)"
        } else {
            "未安装(本机缺 Vulkan SDK),本轮裸奔——结果不构成验证证据"
        }
    );

    let app_info = vk::ApplicationInfo::default()
        .application_name(c"memory_probe")
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
    // 同步验证随主工程同款(VkValidationFeaturesEXT);enables 须活到 create_instance 返回
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

    // 与主工程同门槛:只选报 1.3 的设备
    let pds = unsafe { instance.enumerate_physical_devices() }.expect("枚举物理设备");
    let pd = *pds
        .iter()
        .find(|pd| unsafe { instance.get_physical_device_properties(**pd) }.api_version >= vk::API_VERSION_1_3)
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

    // align_up 零成本自检(bump 偏移与 atom 舍入共用的算术地基)
    assert_eq!(align_up(0, 64), 0);
    assert_eq!(align_up(1, 64), 64);
    assert_eq!(align_up(64, 64), 64);
    assert_eq!(align_up(70_001, 256), 70_144);
    println!("align_up 自检通过(0/边界/非对齐样例)");

    println!("\n== 组 A(合同正路观察组)==");
    {
        let (device, queue) = create_device(&instance, pd);
        // 契约事实冻结:内存类型/堆表 + atom 粒度。表本身是选择决策的证据输入。
        let contract = unsafe { MemoryContract::new(&instance, pd) };
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };
        println!(
            "契约冻结:nonCoherentAtomSize = {}B,内存类型 {} 个 / 堆 {} 个:",
            contract.non_coherent_atom_size(),
            mem_props.memory_type_count,
            mem_props.memory_heap_count
        );
        for ty in 0..mem_props.memory_type_count {
            let t = mem_props.memory_types[ty as usize];
            let h = mem_props.memory_heaps[t.heap_index as usize];
            println!(
                "  类型 {ty}: {:?} → 堆 {}({}B, {:?})",
                t.property_flags, t.heap_index, h.size, h.flags
            );
        }

        let mut staging = GpuBuffer::create(&device, &contract, 1 << 20, BufferRole::Staging)
            .expect("staging 按契约创建");
        println!(
            "[契约①] staging: 请求 {}B → 分配 {}B,内存类型 {}({:?}) host_visible={} host_coherent={} → flush 策略走 {}",
            1 << 20,
            staging.allocation_size(),
            staging.memory_type(),
            staging.memory_properties(),
            staging.host_visible(),
            staging.host_coherent(),
            if staging.host_coherent() { "免 flush 分支" } else { "atom 对齐 flush 分支" }
        );
        assert!(staging.host_visible(), "staging 契约:HOST_VISIBLE 必须成立");
        assert!(staging.allocation_size() >= 1 << 20, "分配不小于请求");

        // 读写:两段区域,一段起点非 atom 对齐;写后 host 读回逐字节校验
        for (off, len) in [REGION_A, REGION_B] {
            let bytes: Vec<u8> = (0..len).map(|i| pattern(off + i as u64)).collect();
            staging.write(off, &bytes).expect("staging 写");
        }
        for (off, len) in [REGION_A, REGION_B] {
            let mut back = vec![0u8; len];
            staging.read(off, &mut back).expect("staging 读");
            for (i, b) in back.iter().enumerate() {
                assert_eq!(
                    *b,
                    pattern(off + i as u64),
                    "staging 读回不匹配 @{:#x}",
                    off + i as u64
                );
            }
        }
        println!(
            "[读写] staging 两段({:#x}..{:#x}、{:#x}..{:#x})host 写→读回逐字节一致",
            REGION_A.0,
            REGION_A.0 + REGION_A.1 as u64,
            REGION_B.0,
            REGION_B.0 + REGION_B.1 as u64,
        );

        let pool = GpuBuffer::create(&device, &contract, 1 << 20, BufferRole::DevicePool)
            .expect("device pool 按契约创建");
        println!(
            "[类型②] device pool: 请求 {}B → 分配 {}B,内存类型 {}({:?}) host_visible={}",
            1 << 20,
            pool.allocation_size(),
            pool.memory_type(),
            pool.memory_properties(),
            pool.host_visible(),
        );
        assert!(
            pool.memory_properties()
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL),
            "pool 契约:DEVICE_LOCAL 必须命中"
        );
        assert!(
            !pool.host_visible(),
            "离散卡上 pool 不应有宿主映射(集成卡合法例外,见打印)"
        );

        let mut readback = GpuBuffer::create(&device, &contract, 1 << 20, BufferRole::Readback)
            .expect("readback 按契约创建");
        println!(
            "[类型③] readback: 内存类型 {}({:?}) host_coherent={}",
            readback.memory_type(),
            readback.memory_properties(),
            readback.host_coherent(),
        );

        // 最小 copyBuffer 闭环:staging → pool → readback,fence 等完成再读。
        // 同族单队列,无 ownership 转移;两条内存屏障只补执行/内存依赖
        // (transfer 写 → transfer 读;transfer 写 → HOST 读)。
        record_copy_round(&device, queue, &staging, &pool, &readback);
        for (off, len) in [REGION_A, REGION_B] {
            let mut back = vec![0u8; len];
            readback
                .read(off, &mut back)
                .expect("readback 读(设备写后)");
            for (i, b) in back.iter().enumerate() {
                assert_eq!(
                    *b,
                    pattern(off + i as u64),
                    "池数据 @{:#x} 不符",
                    off + i as u64
                );
            }
        }
        println!(
            "[闭环] staging → DEVICE_LOCAL 池 → readback 两段逐字节一致(host 写→设备拷→宿主读全链)"
        );

        // fence 已等过 + host 已读完 = 无在途引用,此刻销毁合法(完成条件可证明)
        drop(readback);
        drop(pool);
        drop(staging);
        let clean_so_far = VU_LOG
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty();
        println!(
            "[组 A 诊断] 验证层消息 {}",
            if clean_so_far {
                "零条——合同正路在执法下清净"
            } else {
                "非空——见结尾收账,组 A 不清净"
            }
        );
        unsafe { device.destroy_device(None) };
    }

    println!("\n== 组 B(负例诊断组):memoryOffset 违反 alignment 整数倍条款 ==");
    {
        let (device, _queue) = create_device(&instance, pd);
        let contract = unsafe { MemoryContract::new(&instance, pd) };
        let buffer = unsafe {
            device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(1024)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST),
                None,
            )
        }
        .expect("组 B 创建测试 buffer");
        let reqs = unsafe { device.get_buffer_memory_requirements(buffer) };
        if reqs.alignment <= 1 {
            println!(
                "[组 B 观察] requirements.alignment = {},任意 offset 都是对齐的,负例不成立,跳过",
                reqs.alignment
            );
        } else {
            // offset = alignment-1:小于 memory 大小、类型合法、size 充足,唯一违反 01036
            let offset = reqs.alignment - 1;
            let ty = contract
                .find_type(
                    reqs.memory_type_bits,
                    vk::MemoryPropertyFlags::empty(),
                    vk::MemoryPropertyFlags::HOST_VISIBLE,
                )
                .expect("组 B 选型(必需为空 = 任一允许类型)");
            let memory = unsafe {
                device.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(reqs.size + reqs.alignment)
                        .memory_type_index(ty),
                    None,
                )
            }
            .expect("组 B 分配 memory");
            match unsafe { device.bind_buffer_memory(buffer, memory, offset) } {
                Ok(()) => println!(
                    "[组 B 观察] 驱动返回成功——违反 VUID 未必被驱动拒绝(成功返回≠规范许可);立即回收,不当正常路径继续用"
                ),
                Err(e) => println!("[组 B 观察] 驱动拒绝绑定: {e:?}(记录实际返回码,不假设固定错误码)"),
            }
            unsafe { device.destroy_buffer(buffer, None) };
            unsafe { device.free_memory(memory, None) };
        }
        let hit = VU_LOG
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|m| m.contains("VUID-vkBindBufferMemory"));
        println!(
            "[组 B 诊断] 对齐整数倍条款(01036/新修订 10739){}",
            if hit {
                "已被验证层执法收账"
            } else {
                "未见(验证层未运行或未覆盖此检查)"
            }
        );
        unsafe { device.destroy_device(None) };
    }

    // messenger 先于 instance 销毁——ash 0.38 无自动 Drop,漏了会被 VUID-00629 收账
    if let Some(messenger) = _debug {
        let loader = debug_utils::Instance::new(&entry, &instance);
        unsafe { loader.destroy_debug_utils_messenger(messenger, None) };
    }
    unsafe { instance.destroy_instance(None) };

    println!("\n== 验证层消息收账(全程)==");
    let vu = VU_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if vu.is_empty() {
        println!("  (零条——组 B 未触发,收账以组内打印为准)");
    } else {
        for m in vu.iter() {
            println!("  {m}");
        }
    }
    println!("\n结论口径:组 A = 契约三层在验证层 + 同步验证下跑通;组 B 只证明对齐条款能被执法,");
    println!(
        "不作为生产路径证据。规范依据(memory.adoc 绑定/映射/flush 各条款)见《3.2.2.1 施工记录》。"
    );
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
        .push(format!("[{:?}] {msg}", severity));
    vk::FALSE
}

/// graphics 族设备,零 feature 链(内存路径不涉及 12/13 位;与主工程的差异见文件头)。
fn create_device(instance: &ash::Instance, pd: vk::PhysicalDevice) -> (Device, vk::Queue) {
    let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    let gfx = families
        .iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .expect("无 graphics 族") as u32;
    let priority = [1.0f32];
    let queue_info = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(gfx)
        .queue_priorities(&priority);
    let device = unsafe {
        instance.create_device(
            pd,
            &vk::DeviceCreateInfo::default().queue_create_infos(slice::from_ref(&queue_info)),
            None,
        )
    }
    .expect("vkCreateDevice");
    let queue = unsafe { device.get_device_queue(gfx, 0) };
    (device, queue)
}

/// 组 A 的拷贝闭环:一次录制两张 region(对齐 + 非对齐各一)两段拷贝:
/// staging → pool(TRANSFER_DST),屏障后 pool → readback,出场屏障把
/// transfer 写对 HOST 读收口;fence 等待后 host 才读(读侧 invalidate 在 read() 里)。
fn record_copy_round(
    device: &Device,
    queue: vk::Queue,
    staging: &GpuBuffer,
    pool: &GpuBuffer,
    readback: &GpuBuffer,
) {
    unsafe {
        let command_pool = device
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(0),
                None,
            )
            .expect("探针命令池");
        let command_buffer = device
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(command_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
            .expect("探针命令缓冲")[0];
        device
            .begin_command_buffer(
                command_buffer,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .expect("begin");
        let regions = copy_regions();
        device.cmd_copy_buffer(command_buffer, staging.buffer(), pool.buffer(), &regions);
        // pool 上的写 → 后续拷贝的读:transfer 写 → transfer 读
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[buffer_barrier(
                pool.buffer(),
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::TRANSFER_READ,
            )],
            &[],
        );
        device.cmd_copy_buffer(command_buffer, pool.buffer(), readback.buffer(), &regions);
        // 设备写 → 宿主读:HOST 阶段收口(readback 读侧还有 invalidate 兜底非 coherent)
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            &[buffer_barrier(
                readback.buffer(),
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::HOST_READ,
            )],
            &[],
        );
        device.end_command_buffer(command_buffer).expect("end");
        let fence = device
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .expect("探针 fence");
        device
            .queue_submit(
                queue,
                &[vk::SubmitInfo::default().command_buffers(slice::from_ref(&command_buffer))],
                fence,
            )
            .expect("拷贝提交");
        device
            .wait_for_fences(slice::from_ref(&fence), true, 1_000_000_000)
            .expect("等拷贝完成");
        // 完成条件可证明:fence 已 signal + 宿主已读完 → 无在途引用,销毁合法
        device.destroy_fence(fence, None);
        device.destroy_command_pool(command_pool, None);
    }
}

/// 两张拷贝区域(对齐零头 + 非 atom 对齐起点,专考 flush/invalidate 舍入)。
fn copy_regions() -> [vk::BufferCopy; 2] {
    [REGION_A, REGION_B].map(|(off, len)| vk::BufferCopy {
        src_offset: off,
        dst_offset: off,
        size: len as u64,
    })
}

/// 同族单队列的执行屏障(无 ownership 转移,只补内存依赖)。
fn buffer_barrier(
    buffer: vk::Buffer,
    src: vk::AccessFlags,
    dst: vk::AccessFlags,
) -> vk::BufferMemoryBarrier<'static> {
    vk::BufferMemoryBarrier::default()
        .buffer(buffer)
        .offset(0)
        .size(vk::WHOLE_SIZE)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .src_access_mask(src)
        .dst_access_mask(dst)
}
