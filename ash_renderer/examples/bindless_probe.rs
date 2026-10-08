//! 3.3 前置闸门探针:naga 30.0.1 WGSL → SPIR-V 小样例 + spirv-val + 临时 Vulkan
//! 管线试验,四验收点(设备支持/编译成功/SPIR-V 合法/最终采样正确)逐层取证。
//!
//! 与 3.2 三探针同纪律:自含 Entry → Instance → PhysicalDevice → Device 最小链
//! (不碰窗口),验证层 + 同步验证(VkValidationFeaturesEXT)常开,VUID 收账。
//!
//! - 组 A(编译与结构观察组,纯 CPU):两份同形状 WGSL(runtime 数组无长度 /
//!   定长 4)经 wgsl-in → naga::valid → spv-out;手写 SPIR-V 词流扫描核对
//!   capability/扩展/入口/runtime array/NonUniform 装饰/set-binding 对/
//!   字段偏移(与 Rust `offset_of!` 镜像互证——"按实际布局冻结"的证据面)。
//!   重点记录疑点:naga 30.0.1 spv 后端是否发射 `OpCapability
//!   RuntimeDescriptorArray`(源码 grep 为零,待本组实证)。
//! - 组 B(spirv-val 观察组):SDK `spirv-val --target-env vulkan1.3` 逐模块;
//!   被拒则经最小确定性补丁(capability 段注入缺失 OpCapability/OpExtension)
//!   重验,记录补丁前后差异。缺失 SDK 工具 = 证据链断裂,如实阻塞不冒充通过。
//! - 组 C(管线试验观察组):对每个过 val 的模块建独立设备——Vulkan12Features
//!   先查后开(查询值 = "设备支持"验收点证据;支持 ≠ 启用),两张 2×2 贴图走
//!   `vkCmdCopyBufferToImage` + `UNDEFINED→TRANSFER_DST→SHADER_READ_ONLY`
//!   屏障对(显存机制篇约束:不走 buffer barrier 模板);set0 用
//!   update-after-bind + partially-bound 池/布局(4 槽写 2 槽,3.3.3 生产配方
//!   预演);compute dispatch 两条采样路径:push 索引(一致)与每调用 storage
//!   索引(非一致)——读回比对纹理选择(绿/红)与 sampler 选择(NEAREST 红 /
//!   LINEAR 混色)。
//! - 组 D(负例诊断组):未启用 runtimeDescriptorArray 的设备上建带
//!   OpTypeRuntimeArray 的 compute 管线——只收账不断言驱动行为,断言落在
//!   VU_LOG 是否出现预期 VUID(实测钉号)。
//!
//! 接口形状即冻结草案:set0 = 纹理数组 + sampler 数组(WGSL texture/sampler
//! 分离),set1 = 每帧 UBO,push constant = 每 draw 参数(96B ≤ 128B 最低上限)。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use ash::{ext::debug_utils, vk, Device, Entry};
use ash_renderer::vulkan::{BufferRole, GpuBuffer, MemoryContract};

const VALIDATION_LAYER: &std::ffi::CStr = c"VK_LAYER_KHRONOS_validation";
const RUNTIME_WGSL: &str = include_str!("shaders/bindless_probe_runtime.wgsl");
const FIXED_WGSL: &str = include_str!("shaders/bindless_probe_fixed.wgsl");

// SPIR-V 语法表常量(核心 1.0 + SPV_EXT_descriptor_indexing,值由规范 JSON 钉死):
// 扫描器与补丁器只认词流,不引 spirv crate 的枚举依赖。
const OP_EXTENSION: u16 = 10;
const OP_ENTRY_POINT: u16 = 15;
const OP_CAPABILITY: u16 = 17;
const OP_TYPE_ARRAY: u16 = 28;
const OP_TYPE_RUNTIME_ARRAY: u16 = 29;
const OP_DECORATE: u16 = 71;
const OP_MEMBER_DECORATE: u16 = 72;
const DEC_BINDING: u32 = 33;
const DEC_DESCRIPTOR_SET: u32 = 34;
const DEC_OFFSET: u32 = 35;
const DEC_NON_UNIFORM: u32 = 5300;
const CAP_SHADER: u32 = 1;
const CAP_SHADER_NON_UNIFORM: u32 = 5301;
const CAP_RUNTIME_DESCRIPTOR_ARRAY: u32 = 5302;
const CAP_SAMPLED_IMAGE_ARRAY_NON_UNIFORM: u32 = 5307;
const EXT_DESCRIPTOR_INDEXING: &str = "SPV_EXT_descriptor_indexing";
const EXEC_GL_COMPUTE: u32 = 5;
const SPV_MAGIC: u32 = 0x0723_0203;

/// 验证层消息收账:WARNING/ERROR 全记。VU_LOG 供组分账排水(drain),VU_TOTAL
/// 只追加,探针结束统一打印全程总账。
static VU_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
static VU_TOTAL: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 排空组分账(每组的消息归每组);总账 VU_TOTAL 不动。
fn drain_vu() -> Vec<String> {
    VU_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain(..)
        .collect()
}

/// 组 C 候选:过 spirv-val 的模块词流 + 其特性要求。
struct Candidate {
    label: String,
    words: Vec<u32>,
    needs_runtime: bool,
    val_note: String,
}

fn main() {
    let entry = unsafe { Entry::load() }.expect("加载 Vulkan loader");

    // 验证层三件事分开报:装没装、请求没请求、收没收到 VUID(结尾分账)。
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
        .application_name(c"bindless_probe")
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

    // ============ 组 A:编译与结构观察 ============
    let runtime = compile_module("runtime", RUNTIME_WGSL);
    let fixed = compile_module("fixed", FIXED_WGSL);
    // 负例②用补丁词流(capability 已声明、特性未启用 → 期待 feature 执法);
    // patch 幂等(已存在的不重复注),与组 B 闸门内的补丁同函数同结果
    let patched_runtime = patch_descriptor_indexing(&runtime.words);

    // ============ 组 B:spirv-val + 补丁 ============
    let mut candidates = Vec::new();
    match locate_spirv_val() {
        Some(val_exe) => {
            for module in [&runtime, &fixed] {
                if let Some(candidate) = spirv_val_gate(&val_exe, module) {
                    candidates.push(candidate);
                }
            }
        }
        None => {
            println!(
                "\n[阻塞] 本机未找到 spirv-val(VULKAN_SDK 未设且默认路径不存在)——"
            );
            println!(
                "       闸门④的\"SPIR-V 合法\"验收点无官方工具证据,组 C/D 一并停跑;"
            );
            println!(
                "       不把缺环境的静默当证据(3.3 README 红线)。装 Vulkan SDK 后重跑。"
            );
        }
    }

    // ============ 组 C:Vulkan 管线试验 ============
    if candidates.is_empty() {
        println!("\n[结论] 无模块通过 spirv-val——闸门④\"SPIR-V 合法\"未过,管线试验无载体。");
    } else {
        for cand in &candidates {
            run_trial(&instance, pd, cand);
        }
    }

    // ============ 组 D:负例诊断 ============
    group_d_negative(&instance, pd, &runtime.words, &patched_runtime);

    // messenger 先于 instance 销毁——ash 0.38 无自动 Drop,漏了会被 VUID-00629 收账
    if let Some(messenger) = _debug {
        let loader = debug_utils::Instance::new(&entry, &instance);
        unsafe { loader.destroy_debug_utils_messenger(messenger, None) };
    }
    unsafe { instance.destroy_instance(None) };

    println!("\n== 验证层消息收账(全程)==");
    let vu = VU_TOTAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if vu.is_empty() {
        println!("  (零条——探针目标路径在验证层 + 同步验证下清净;组 D 负例按组内口径报告)");
    } else {
        for m in vu.iter() {
            println!("  {m}");
        }
    }
    drop(vu);
    println!("\n结论口径:组 A=编译 + 结构扫描(四验收点之\"编译成功\");组 B=spirv-val");
    println!("(之\"SPIR-V 合法\");组 C=设备支持 + 管线 + 采样读回(之\"设备支持\"\"最终采样");
    println!("正确\");组 D=负例诊断(只收账)。冻结结论与面板落账见 3.3 施工记录。");
}

// ============ 组 A:编译与结构扫描(纯 CPU)============

/// 编译一份 WGSL:wgsl-in → naga::valid(能力面显式声明)→ spv-out → 词流扫描。
/// 编译成功本身是验收点,失败即 panic(带完整错误链,不静默)。
fn compile_module(label: &'static str, source: &str) -> CompiledModule {
    println!("\n== 组 A({label}:naga 30.0.1 wgsl-in → spv-out + 结构扫描)==");
    let module = match naga::front::wgsl::parse_str(source) {
        Ok(m) => m,
        Err(e) => panic!("[{label}] WGSL 解析失败(验收点\"编译成功\"未过): {e}"),
    };
    // 能力面显式声明:binding 数组、其非一致索引、var<immediate>(push constant)。
    // "支持 ≠ 启用"纪律的编译侧对应物:validator 只在这些能力被授予时放行。
    let caps = naga::valid::Capabilities::IMMEDIATES
        | naga::valid::Capabilities::TEXTURE_AND_SAMPLER_BINDING_ARRAY
        | naga::valid::Capabilities::TEXTURE_AND_SAMPLER_BINDING_ARRAY_NON_UNIFORM_INDEXING;
    let mut validator = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), caps);
    let info = match validator.validate(&module) {
        Ok(i) => i,
        Err(e) => panic!("[{label}] naga IR 校验失败: {e}"),
    };
    let spv_options = naga::back::spv::Options::default();
    let words = match naga::back::spv::write_vec(&module, &info, &spv_options, None) {
        Ok(w) => w,
        Err(e) => panic!("[{label}] SPIR-V 生成失败: {e}"),
    };
    println!("[编译] wgsl-in → IR 校验 → spv-out 全链通过,{} 字({}B)", words.len(), words.len() * 4);

    let scan = scan_spv(&words);
    print_scan(label, &scan);
    assert_scan(label, &scan);
    CompiledModule {
        label,
        words,
        scan,
        needs_runtime: label == "runtime",
    }
}

pub struct CompiledModule {
    pub label: &'static str,
    pub words: Vec<u32>,
    pub scan: SpvScan,
    /// 是否需要 runtimeDescriptorArray 特性:runtime 版纹理/sampler 数组不带长度
    /// → UniformConstant 里的 OpTypeRuntimeArray 需要该特性;fixed 版不需要。
    /// 注意 storage 输出的 `array<vec4f>` 也是 OpTypeRuntimeArray,但那是 SSBO
    /// 核心语义(1.0 起),不算 descriptor-indexing 需求——所以按模块来源判,不按
    /// 全模块 OpTypeRuntimeArray 计数判。
    pub needs_runtime: bool,
}

/// SPIR-V 词流结构摘要:capability/扩展/入口/数组形态/NonUniform/set-binding/偏移。
pub struct SpvScan {
    version: (u8, u8),
    capabilities: Vec<u32>,
    extensions: Vec<String>,
    entry_points: Vec<(u32, String)>,
    runtime_array_count: usize,
    fixed_array_count: usize,
    non_uniform_count: usize,
    /// (set, binding) 对,来自 DescriptorSet/Binding 装饰配对。
    set_bindings: Vec<(u32, u32)>,
    /// 结构体成员偏移表:类型 id → 按成员序的 offset 值。
    struct_member_offsets: Vec<(u32, Vec<u32>)>,
}

fn scan_spv(words: &[u32]) -> SpvScan {
    assert_eq!(words[0], SPV_MAGIC, "SPIR-V magic 不符");
    let v = words[1];
    let version = ((v >> 16) as u8, (v >> 8) as u8);
    let mut capabilities = Vec::new();
    let mut extensions = Vec::new();
    let mut entry_points = Vec::new();
    let mut runtime_array_count = 0usize;
    let mut fixed_array_count = 0usize;
    let mut non_uniform_count = 0usize;
    let mut sets: HashMap<u32, u32> = HashMap::new();
    let mut binds: HashMap<u32, u32> = HashMap::new();
    let mut offsets: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();

    let mut i = 5usize; // 跳过 5 字头
    while i < words.len() {
        let word0 = words[i];
        let wc = (word0 >> 16) as usize;
        let op = (word0 & 0xFFFF) as u16;
        match op {
            OP_CAPABILITY => capabilities.push(words[i + 1]),
            OP_EXTENSION => extensions.push(decode_spv_string(&words[i + 1..i + wc])),
            OP_ENTRY_POINT => {
                let model = words[i + 1];
                // 名字串是第 3 个操作数(exec model、entry id 之后),接口 id 列表在
                // NUL 之后,decode 按首个 NUL 截断不受影响
                let name = decode_spv_string(&words[i + 3..i + wc]);
                entry_points.push((model, name));
            }
            OP_TYPE_ARRAY => fixed_array_count += 1,
            OP_TYPE_RUNTIME_ARRAY => runtime_array_count += 1,
            OP_DECORATE => {
                let target = words[i + 1];
                match words[i + 2] {
                    DEC_NON_UNIFORM => non_uniform_count += 1,
                    DEC_DESCRIPTOR_SET => {
                        sets.insert(target, words[i + 3]);
                    }
                    DEC_BINDING => {
                        binds.insert(target, words[i + 3]);
                    }
                    _ => {}
                }
            }
            OP_MEMBER_DECORATE => {
                let struct_id = words[i + 1];
                let member = words[i + 2];
                if words[i + 3] == DEC_OFFSET {
                    offsets.entry(struct_id).or_default().push((member, words[i + 4]));
                }
            }
            _ => {}
        }
        i += wc.max(1);
    }
    let mut set_bindings: Vec<(u32, u32)> = sets
        .iter()
        .filter_map(|(id, &set)| binds.get(id).map(|&b| (set, b)))
        .collect();
    set_bindings.sort_unstable();
    let mut struct_member_offsets: Vec<(u32, Vec<u32>)> = offsets
        .into_iter()
        .map(|(id, mut ms)| {
            ms.sort_unstable_by_key(|&(member, _)| member);
            (id, ms.into_iter().map(|(_, off)| off).collect())
        })
        .collect();
    struct_member_offsets.sort_unstable_by_key(|&(id, _)| id);
    SpvScan {
        version,
        capabilities,
        extensions,
        entry_points,
        runtime_array_count,
        fixed_array_count,
        non_uniform_count,
        set_bindings,
        struct_member_offsets,
    }
}

/// 装饰与偏移的观察组打印(结构证据,供施工记录摘录)。
fn print_scan(label: &str, scan: &SpvScan) {
    let caps: Vec<String> = scan.capabilities.iter().map(|&c| cap_name(c)).collect();
    println!(
        "[{label}] SPIR-V v{}.{};capabilities [{}];extensions {:?}",
        scan.version.0, scan.version.1,
        caps.join(", "),
        scan.extensions,
    );
    for &(model, ref name) in &scan.entry_points {
        println!(
            "[入口] 执行模型 {} 名 {:?}({})",
            model,
            name,
            if model == EXEC_GL_COMPUTE { "GLCompute,符合" } else { "非 GLCompute!" },
        );
    }
    println!(
        "[数组] OpTypeRuntimeArray ×{} OpTypeArray ×{};NonUniform 装饰 ×{}",
        scan.runtime_array_count, scan.fixed_array_count, scan.non_uniform_count,
    );
    println!("[绑定] set/binding 对 {:?}", scan.set_bindings);
    for (id, offs) in &scan.struct_member_offsets {
        println!("[偏移] 结构体 #{id}: {offs:?}");
    }
}

/// capability 值 → 可读名(已知枚举,未知报值不猜名)。
fn cap_name(value: u32) -> String {
    match value {
        CAP_SHADER => "Shader".into(),
        CAP_SHADER_NON_UNIFORM => "ShaderNonUniform".into(),
        CAP_RUNTIME_DESCRIPTOR_ARRAY => "RuntimeDescriptorArray".into(),
        CAP_SAMPLED_IMAGE_ARRAY_NON_UNIFORM => "SampledImageArrayNonUniformIndexing".into(),
        other => format!("未知#{other}"),
    }
}

/// 结构断言(载荷性检查,失败即闸门证据缺失):
/// - 入口 GLCompute "main";binding 表恰为 0:0/0:1/1:0/2:0/2:1;
/// - NonUniform ≥ 1(非一致路径须被 naga 自动装饰,不写 GLSL nonuniformEXT);
/// - push 结构偏移恰为 0/64/68/80(与下方 Rust 镜像 `offset_of!` 互证);
/// - runtime 版必有 OpTypeRuntimeArray;定长版 texture/sampler 走 OpTypeArray。
fn assert_scan(label: &str, scan: &SpvScan) {
    assert!(
        scan.entry_points
            .iter()
            .any(|&(model, ref name)| model == EXEC_GL_COMPUTE && name == "main"),
        "[{label}] 缺 GLCompute main 入口: {:?}",
        scan.entry_points,
    );
    for expect in [(0u32, 0u32), (0, 1), (1, 0), (2, 0), (2, 1)] {
        assert!(
            scan.set_bindings.contains(&expect),
            "[{label}] 缺 set/binding 对 {expect:?}: {:?}",
            scan.set_bindings,
        );
    }
    assert!(
        scan.non_uniform_count >= 1,
        "[{label}] 无 NonUniform 装饰——非一致索引路径未被标记,须进一步查 naga 行为",
    );
    let push_offsets = [0u32, 64, 68, 80];
    assert!(
        scan.struct_member_offsets.iter().any(|(_, offs)| offs.as_slice() == push_offsets),
        "[{label}] 未找到偏移 0/64/68/80 的 push 结构: {:?}",
        scan.struct_member_offsets,
    );
    assert!(
        scan.struct_member_offsets.iter().any(|(_, offs)| offs == &[0u32]),
        "[{label}] 未找到单成员(64B)的每帧 UBO 结构: {:?}",
        scan.struct_member_offsets,
    );
    assert!(
        scan.capabilities.contains(&CAP_SHADER),
        "[{label}] 缺 Shader capability"
    );
    if scan.runtime_array_count > 0 {
        println!(
            "[疑点] {label} 含 OpTypeRuntimeArray;RuntimeDescriptorArray capability: {}",
            if scan.capabilities.contains(&CAP_RUNTIME_DESCRIPTOR_ARRAY) {
                "已声明"
            } else {
                "未声明(naga 30.0.1 后端源码 grep 零发射——是否合法交组 B spirv-val 裁决)"
            },
        );
    }
}

/// push 参数的 Rust 镜像:与 WGSL PushParams 字段同序,`offset_of!` 在编译期钉住
/// 一侧事实,SPIR-V 扫描钉住另一侧——两边对上才是"按实际布局冻结"。
#[repr(C)]
struct PushParamsMirror {
    // Mat4/Vec4 带 16B 对齐(与 WGSL uniform 布局的 vec4 对齐一致):WGSL 里
    // base_color: vec4f 对齐 16 → 偏移 80;若用裸 [f32; 4](对齐 4)会落在 68+4=72,
    // 与 shader 侧不一致——"repr(C) 不能独立证明 CPU/shader 对齐"正是施工计划
    // §2 的提醒,本镜像把 16B 对齐显式钉在类型上。
    model: bevy::math::Mat4,
    tex_index: u32,
    sampler_index: u32,
    base_color: bevy::math::Vec4,
}
const _: () = {
    assert!(std::mem::offset_of!(PushParamsMirror, model) == 0);
    assert!(std::mem::offset_of!(PushParamsMirror, tex_index) == 64);
    assert!(std::mem::offset_of!(PushParamsMirror, sampler_index) == 68);
    assert!(std::mem::offset_of!(PushParamsMirror, base_color) == 80);
    assert!(std::mem::size_of::<PushParamsMirror>() == 96);
};

/// SPIR-V 字符串字面量解码:NUL 结尾、每字 4 字节小端、零填充。
fn decode_spv_string(ws: &[u32]) -> String {
    let mut bytes = Vec::new();
    for &w in ws {
        for b in w.to_le_bytes() {
            if b == 0 {
                return String::from_utf8_lossy(&bytes).into_owned();
            }
            bytes.push(b);
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

// ============ 组 B:spirv-val 与 capability 补丁 ============

/// 定位 SDK 的 spirv-val:先 VULKAN_SDK 环境变量,回退 1.4.357.0 默认安装路径。
/// 双双落空返回 None(调用方如实阻塞——缺官方工具不得冒充"SPIR-V 合法")。
fn locate_spirv_val() -> Option<PathBuf> {
    if let Ok(sdk) = std::env::var("VULKAN_SDK") {
        let p = PathBuf::from(sdk).join("Bin").join("spirv-val.exe");
        if p.is_file() {
            return Some(p);
        }
    }
    let fallback = PathBuf::from("C:/VulkanSDK/1.4.357.0/Bin/spirv-val.exe");
    fallback.is_file().then_some(fallback)
}

/// 组 B 闸门:逐模块 spirv-val;被拒则补丁重验。产出组 C 候选列表。
fn spirv_val_gate(val_exe: &std::path::Path, module: &CompiledModule) -> Option<Candidate> {
    println!("\n== 组 B({}:spirv-val --target-env vulkan1.3)==", module.label);
    match run_spirv_val(val_exe, &module.words) {
        Ok(()) => {
            println!("[val] 原生输出通过(零告警)");
            return Some(Candidate {
                label: format!("{}-原生", module.label),
                words: module.words.clone(),
                needs_runtime: module.needs_runtime,
                val_note: "原生输出直接过 val".into(),
            });
        }
        Err(out) => {
            println!("[val] 原生输出被拒:");
            for line in out.lines() {
                println!("    {line}");
            }
        }
    }
    // 补丁:capability 段注入缺失的 descriptor-indexing capability/扩展,重验。
    let patched = patch_descriptor_indexing(&module.words);
    match run_spirv_val(val_exe, &patched) {
        Ok(()) => {
            println!("[补丁] 注入缺失 capability/扩展后通过——naga 原生输出确有发射缺口,补丁器为最小确定性修复");
            Some(Candidate {
                label: format!("{}-补丁", module.label),
                words: patched,
                needs_runtime: module.needs_runtime,
                val_note: "原生被 val 拒 → capability 补丁后过".into(),
            })
        }
        Err(out) => {
            println!("[补丁] 补丁后仍被拒:");
            for line in out.lines() {
                println!("    {line}");
            }
            println!("[结论] {label} 模块无合法形态(补丁未挽回)——闸门④该模块未过",
                label = module.label);
            None
        }
    }
}

/// 跑一次 spirv-val:词流写临时 .spv(小端),--target-env vulkan1.3,运行后清理。
/// 通过返回 Ok;被拒返回 Err(stderr/stderr 合并文本)。
fn run_spirv_val(val_exe: &std::path::Path, words: &[u32]) -> Result<(), String> {
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let spv_path = std::env::temp_dir().join("ash_bindless_probe_val.spv");
    std::fs::write(&spv_path, &bytes).map_err(|e| format!("写临时 .spv 失败: {e}"))?;
    let result = std::process::Command::new(val_exe)
        .arg("--target-env")
        .arg("vulkan1.3")
        .arg(&spv_path)
        .output();
    let _ = std::fs::remove_file(&spv_path);
    match result {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stderr).into_owned();
            if text.trim().is_empty() {
                text = String::from_utf8_lossy(&o.stdout).into_owned();
            }
            Err(text)
        }
        Err(e) => Err(format!("spawn spirv-val 失败: {e}")),
    }
}

/// 最小确定性补丁:capability 段注入 descriptor-indexing 相关的缺失项——
/// RuntimeDescriptorArray、SampledImageArrayNonUniformIndexing 两个 capability +
/// SPV_EXT_descriptor_indexing 扩展(已存在的不重复注)。声明多余的 capability
/// 只收紧设备要求,不放松合法性。
///
/// SPIR-V 逻辑布局顺序是 capabilities → extensions → …:capability 必须插在
/// extension 区段之前,extension 插在既有 extension 之后、下一段之前,所以分两个
/// 插入点两段拼接,不合并成一点(否则 capability 会落到 extension 后面,布局违规)。
fn patch_descriptor_indexing(words: &[u32]) -> Vec<u32> {
    let mut caps_present: Vec<u32> = Vec::new();
    let mut exts_present: Vec<String> = Vec::new();
    // 第一遍:吃掉开头连续的 OpCapability,记下 capability 区段结束点
    let mut pos = 5usize; // 跳过 5 字头
    while pos < words.len() {
        let word0 = words[pos];
        let wc = ((word0 >> 16) as usize).max(1);
        if (word0 & 0xFFFF) as u16 == OP_CAPABILITY {
            caps_present.push(words[pos + 1]);
            pos += wc;
        } else {
            break;
        }
    }
    let cap_insert = pos;
    // 第二遍:从 capability 结束点继续吃 OpExtension,记下 extension 区段结束点
    let mut ext_pos = cap_insert;
    let mut ext_insert = cap_insert;
    while ext_pos < words.len() {
        let word0 = words[ext_pos];
        let wc = ((word0 >> 16) as usize).max(1);
        if (word0 & 0xFFFF) as u16 == OP_EXTENSION {
            exts_present.push(decode_spv_string(&words[ext_pos + 1..ext_pos + wc]));
            ext_insert = ext_pos + wc;
            ext_pos += wc;
        } else {
            break;
        }
    }
    let mut extra_caps: Vec<u32> = Vec::new();
    for cap in [CAP_RUNTIME_DESCRIPTOR_ARRAY, CAP_SAMPLED_IMAGE_ARRAY_NON_UNIFORM] {
        if !caps_present.contains(&cap) {
            // OpCapability 恒 2 字:(wordcount<<16)|opcode
            extra_caps.push((2u32 << 16) | u32::from(OP_CAPABILITY));
            extra_caps.push(cap);
        }
    }
    let mut extra_exts: Vec<u32> = Vec::new();
    if !exts_present.iter().any(|e| e == EXT_DESCRIPTOR_INDEXING) {
        let name = EXT_DESCRIPTOR_INDEXING.as_bytes();
        // word0 + 字符串字(NUL 终止、整字补零)
        let word_count = 1 + (name.len() + 1).div_ceil(4);
        extra_exts.push((word_count as u32) << 16 | u32::from(OP_EXTENSION));
        let mut buf = name.to_vec();
        buf.push(0); // NUL 终止
        buf.resize(buf.len().next_multiple_of(4), 0); // 补齐整字
        for chunk in buf.as_chunks::<4>().0 {
            extra_exts.push(u32::from_le_bytes(*chunk));
        }
    }
    let mut out = Vec::with_capacity(words.len() + extra_caps.len() + extra_exts.len());
    out.extend_from_slice(&words[..cap_insert]);
    out.extend_from_slice(&extra_caps);
    out.extend_from_slice(&words[cap_insert..ext_insert]);
    out.extend_from_slice(&extra_exts);
    out.extend_from_slice(&words[ext_insert..]);
    out
}

// ============ 组 C:Vulkan compute 管线试验 ============

/// 纹理像素(2×2 RGBA8,行主序)。tex0 = 红蓝双列(行同),tex1 = 全绿。
const TEX0_PIXELS: [u8; 16] = [
    255, 0, 0, 255, 0, 0, 255, 255, 255, 0, 0, 255, 0, 0, 255, 255,
];
const TEX1_PIXELS: [u8; 16] = [0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255];

/// 采样期望值:uv=(0.3, 0.5) 恒定,texel 空间 u=0.6(中心 0.5/1.5,避开取整边界)。
/// - out[0](push:tex1 + LINEAR)= 绿;
/// - out[1](gid0:tex0 + NEAREST)= 红(0.6 距中心 0.5 最近 → texel 0);
/// - out[2](gid1:tex0 + LINEAR)= t=0.1 → 0.9 红 + 0.1 蓝(行向 t=0.5,两行同色)。
const EXPECT_GREEN: [f32; 4] = [0.0, 1.0, 0.0, 1.0];
const EXPECT_RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const EXPECT_MIX: [f32; 4] = [0.9, 0.0, 0.1, 1.0];

/// push 常量字节:identity 模型阵 + tex/sampler 索引 + 全零 base_color,96B。
/// 字段偏移已由 `PushParamsMirror` 的 `offset_of!` 在编译期钉死。
fn push_bytes(tex_index: u32, sampler_index: u32) -> [u8; 96] {
    let mut b = [0u8; 96];
    let identity: [f32; 16] = [
        1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    ];
    for (i, v) in identity.iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    b[64..68].copy_from_slice(&tex_index.to_le_bytes());
    b[68..72].copy_from_slice(&sampler_index.to_le_bytes());
    b
}

/// 每帧 UBO 字节:view_proj = identity,64B(与 WGSL FrameUniforms span 一致)。
fn ubo_bytes() -> [u8; 64] {
    push_bytes(0, 0)[..64].try_into().expect("取前 64 字节")
}

/// 每调用索引字节:tex=[0,0],samp=[1,0](gid0 取槽 1 NEAREST,gid1 取槽 0 LINEAR)。
fn index_bytes() -> [u8; 16] {
    let mut b = [0u8; 16];
    b[8..12].copy_from_slice(&1u32.to_le_bytes()); // samp[0] = 槽 1(NEAREST)
    b[12..16].copy_from_slice(&0u32.to_le_bytes()); // samp[1] = 槽 0(LINEAR)
    b
}

/// 对一个过 val 的候选跑完整 compute 试验。设备特性先查后开;任何一环失败即
/// panic(验收点未过);全程 VUID 收账。
fn run_trial(instance: &ash::Instance, pd: vk::PhysicalDevice, cand: &Candidate) {
    println!("\n== 组 C({}:临时 Vulkan 管线试验)==", cand.label);
    println!("[val 口径] {}", cand.val_note);

    // 验收点①设备支持:先查后开——查询值打印为证,需要位不支持则如实注明跳过。
    let mut queried = vk::PhysicalDeviceVulkan12Features::default();
    let mut features2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut queried);
    unsafe { instance.get_physical_device_features2(pd, &mut features2) };
    println!(
        "[设备支持] descriptor_indexing={} runtime_descriptor_array={} \
         sampled_image_array_non_uniform={} sampled_image_update_after_bind={} partially_bound={}",
        queried.descriptor_indexing == vk::TRUE,
        queried.runtime_descriptor_array == vk::TRUE,
        queried.shader_sampled_image_array_non_uniform_indexing == vk::TRUE,
        queried.descriptor_binding_sampled_image_update_after_bind == vk::TRUE,
        queried.descriptor_binding_partially_bound == vk::TRUE,
    );
    let props = unsafe { instance.get_physical_device_properties(pd) };
    println!(
        "[限额] maxPushConstantsSize={}B maxPerStageDescriptorSampledImages={} \
         maxDescriptorSetSampledImages={} maxPerStageDescriptorSamplers={}(1024 容量定案素材,归 3.3.2)",
        props.limits.max_push_constants_size,
        props.limits.max_per_stage_descriptor_sampled_images,
        props.limits.max_descriptor_set_sampled_images,
        props.limits.max_per_stage_descriptor_samplers,
    );
    let need = feature_set(cand.needs_runtime);
    let supported = queried.descriptor_indexing == vk::TRUE
        && queried.shader_sampled_image_array_non_uniform_indexing == vk::TRUE
        && queried.descriptor_binding_sampled_image_update_after_bind == vk::TRUE
        && queried.descriptor_binding_partially_bound == vk::TRUE
        && (!cand.needs_runtime || queried.runtime_descriptor_array == vk::TRUE);
    if !supported {
        println!(
            "[跳过] 本机设备缺本候选必需特性位——\"设备支持\"未过,如实注明,不冒充通过"
        );
        return;
    }

    // 设备与队列(compute 走 graphics 队列,不另设 compute 族)
    let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    let gfx_family = families
        .iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .expect("无 graphics 族") as u32;
    let mut vk12 = need;
    let priority = [1.0f32];
    let queue_infos = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(gfx_family)
        .queue_priorities(&priority)];
    let device = unsafe {
        instance
            .create_device(
                pd,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_infos)
                    .push_next(&mut vk12),
                None,
            )
            .expect("组 C vkCreateDevice")
    };
    let queue = unsafe { device.get_device_queue(gfx_family, 0) };
    let contract = unsafe { MemoryContract::new(instance, pd) };

    // 两张贴图(OPTIMAL tiling,独立于任何 buffer 池——显存机制篇约束②)
    let tex0 = unsafe { TestImage::new(&device, &contract, "tex0") };
    let tex1 = unsafe { TestImage::new(&device, &contract, "tex1") };
    // staging 用交付本体 GpuBuffer(契约 flush 纪律),像素 16B/张
    let mut stage0 = GpuBuffer::create(&device, &contract, 16, BufferRole::Staging)
        .expect("staging0 创建");
    let mut stage1 =
        GpuBuffer::create(&device, &contract, 16, BufferRole::Staging).expect("staging1 创建");
    stage0.write(0, &TEX0_PIXELS).expect("stage0 写");
    stage1.write(0, &TEX1_PIXELS).expect("stage1 写");

    // sampler 两枚:槽 0 = LINEAR,槽 1 = NEAREST(同图异样器 → 混色可区分)
    let sampler_linear = unsafe {
        device
            .create_sampler(&sampler_info(vk::Filter::LINEAR), None)
            .expect("LINEAR sampler")
    };
    let sampler_nearest = unsafe {
        device
            .create_sampler(&sampler_info(vk::Filter::NEAREST), None)
            .expect("NEAREST sampler")
    };

    // UBO / 索引 / 输出三块 host-visible(HOST_COHERENT 作必需:免 flush 分支)
    let ubo = unsafe {
        PlainBuffer::new(
            &device,
            &contract,
            64,
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            "ubo",
        )
    };
    let indices = unsafe {
        PlainBuffer::new(
            &device,
            &contract,
            16,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            "indices",
        )
    };
    let out = unsafe {
        PlainBuffer::new(
            &device,
            &contract,
            48,
            vk::BufferUsageFlags::STORAGE_BUFFER,
            "out",
        )
    };
    ubo.write(&ubo_bytes());
    indices.write(&index_bytes());

    // 接口形状:set0 UAB+PARTIALLY_BOUND 双数组 / set1 UBO / set2 storage×2 + push
    let shapes = unsafe { InterfaceShapes::new(&device) };
    let pool = unsafe {
        device
            .create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .flags(vk::DescriptorPoolCreateFlags::UPDATE_AFTER_BIND)
                    .max_sets(3)
                    .pool_sizes(&[
                        vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::SAMPLED_IMAGE)
                            .descriptor_count(4),
                        vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::SAMPLER)
                            .descriptor_count(4),
                        vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::UNIFORM_BUFFER)
                            .descriptor_count(1),
                        vk::DescriptorPoolSize::default()
                            .ty(vk::DescriptorType::STORAGE_BUFFER)
                            .descriptor_count(2),
                    ]),
                None,
            )
            .expect("descriptor pool(UAB)")
    };
    let sets = unsafe {
        device
            .allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&shapes.set_layouts),
            )
            .expect("分配 3 组描述符集")
    };
    // set0:4 槽只写 2 槽(PARTIALLY_BOUND 的空槽不得被访问——shader 只索引 0/1)
    let image_infos = [
        vk::DescriptorImageInfo::default()
            .image_view(tex0.view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
        vk::DescriptorImageInfo::default()
            .image_view(tex1.view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
    ];
    let sampler_infos = [
        vk::DescriptorImageInfo::default().sampler(sampler_linear),
        vk::DescriptorImageInfo::default().sampler(sampler_nearest),
    ];
    let ubo_info = [vk::DescriptorBufferInfo::default()
        .buffer(ubo.buffer)
        .offset(0)
        .range(64)];
    let indices_info = [vk::DescriptorBufferInfo::default()
        .buffer(indices.buffer)
        .offset(0)
        .range(16)];
    let out_info = [vk::DescriptorBufferInfo::default()
        .buffer(out.buffer)
        .offset(0)
        .range(48)];
    unsafe {
        device.update_descriptor_sets(
            &[
                vk::WriteDescriptorSet::default()
                    .dst_set(sets[0])
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&image_infos),
                vk::WriteDescriptorSet::default()
                    .dst_set(sets[0])
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&sampler_infos),
                vk::WriteDescriptorSet::default()
                    .dst_set(sets[1])
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(&ubo_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(sets[2])
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&indices_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(sets[2])
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&out_info),
            ],
            &[],
        );
    }

    let shader_module = unsafe {
        device
            .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&cand.words), None)
            .expect("shader module")
    };
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
                    .layout(shapes.pipeline_layout)],
                None,
            )
            .map_err(|(_, e)| e)
            .expect("compute pipeline");
        created[0]
    };

    // 命令:上传两图 → 屏障对(UNDEFINED→TRANSFER_DST→SHADER_READ_ONLY)→ dispatch
    let cmd_pool = unsafe {
        device
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(gfx_family),
                None,
            )
            .expect("命令池")
    };
    let cb = unsafe {
        device
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
            .expect("命令缓冲")[0]
    };
    unsafe {
        device
            .begin_command_buffer(
                cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .expect("begin");
        // 拷贝前:UNDEFINED → TRANSFER_DST(显存机制篇:layout 转换是 image 特有
        // 语义,不走 3.2 buffer barrier 模板;初见屏障 src 无需 access)
        let to_dst = [
            image_barrier(tex0.image, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL),
            image_barrier(tex1.image, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL),
        ];
        device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &to_dst,
        );
        for (image, staging) in [(tex0.image, &stage0), (tex1.image, &stage1)] {
            device.cmd_copy_buffer_to_image(
                cb,
                staging.buffer(),
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::BufferImageCopy::default()
                    .buffer_offset(0)
                    .buffer_row_length(0)
                    .buffer_image_height(0)
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .mip_level(0)
                            .base_array_layer(0)
                            .layer_count(1),
                    )
                    .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                    .image_extent(vk::Extent3D { width: 2, height: 2, depth: 1 })],
            );
        }
        // 拷贝后:TRANSFER_DST → SHADER_READ_ONLY(compute 采样读)
        let to_read = [
            image_barrier(tex0.image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
            image_barrier(tex1.image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
        ];
        device.cmd_pipeline_barrier(
            cb,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &to_read,
        );
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, pipeline);
        device.cmd_bind_descriptor_sets(
            cb,
            vk::PipelineBindPoint::COMPUTE,
            shapes.pipeline_layout,
            0,
            &sets,
            &[],
        );
        let push = push_bytes(1, 0); // push 路径:tex1(绿)+ LINEAR
        device.cmd_push_constants(
            cb,
            shapes.pipeline_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            &push,
        );
        device.cmd_dispatch(cb, 1, 1, 1);
        device.end_command_buffer(cb).expect("end");
        let fence = device
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .expect("fence");
        device
            .queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&[cb])], fence)
            .expect("提交");
        device
            .wait_for_fences(&[fence], true, 5_000_000_000)
            .expect("等 dispatch 完成");
        device.destroy_fence(fence, None);
    }

    // 读回三槽断言:纹理选择(绿/红)+ sampler 选择(NEAREST 红 / LINEAR 混色)
    let mut back = [0u8; 48];
    out.read(&mut back);
    let f32le = |i: usize| f32::from_le_bytes(back[i * 4..i * 4 + 4].try_into().expect("4 字节"));
    let out0 = [f32le(0), f32le(1), f32le(2), f32le(3)];
    let out1 = [f32le(4), f32le(5), f32le(6), f32le(7)];
    let out2 = [f32le(8), f32le(9), f32le(10), f32le(11)];
    assert_close(out0, EXPECT_GREEN, "out[0] push 路径(tex1+LINEAR)");
    assert_close(out1, EXPECT_RED, "out[1] gid0(tex0+NEAREST)");
    assert_close(out2, EXPECT_MIX, "out[2] gid1(tex0+LINEAR)");
    println!(
        "[采样] out = [{:?}, {:?}, {:?}]——push 一致索引选绿、NEAREST 取整红、LINEAR 混色全部正确",
        out0, out1, out2,
    );
    // 验收点纪律:目标路径 WARNING/ERROR 清净,非空即闸门未过(不静默)
    let vu = drain_vu();
    if vu.is_empty() {
        println!("[收账] 本组验证层消息零条——管线试验在验证层 + 同步验证下清净");
    } else {
        for m in &vu {
            println!("  {m}");
        }
        panic!("[{}] 组 C 收到验证层消息 {} 条——\"零告警\"验收点未过", cand.label, vu.len());
    }

    // 清场:等空闲再反序拆(销毁纪律)
    unsafe { device.device_wait_idle() }.expect("trial 收场 wait_idle");
    unsafe { device.destroy_pipeline(pipeline, None) };
    unsafe { device.destroy_shader_module(shader_module, None) };
    unsafe { device.destroy_descriptor_pool(pool, None) };
    drop(shapes);
    unsafe { device.destroy_sampler(sampler_linear, None) };
    unsafe { device.destroy_sampler(sampler_nearest, None) };
    tex1.destroy();
    tex0.destroy();
    drop(out);
    drop(indices);
    drop(ubo);
    drop(stage1);
    drop(stage0);
    unsafe { device.destroy_command_pool(cmd_pool, None) };
    unsafe { device.destroy_device(None) };
}

fn assert_close(actual: [f32; 4], expect: [f32; 4], label: &str) {
    for (i, (&a, &e)) in actual.iter().zip(expect.iter()).enumerate() {
        assert!(
            (a - e).abs() < 0.01,
            "{label} 通道 {i} 读回 {a} 期望 {e}(全值 {actual:?})"
        );
    }
}

fn sampler_info(filter: vk::Filter) -> vk::SamplerCreateInfo<'static> {
    vk::SamplerCreateInfo::default()
        .mag_filter(filter)
        .min_filter(filter)
        .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
}

fn image_barrier(
    image: vk::Image,
    old_layout: vk::ImageLayout,
    new_layout: vk::ImageLayout,
) -> vk::ImageMemoryBarrier<'static> {
    // access 由 layout 配对定案:进 TRANSFER_DST 带 TRANSFER_WRITE,进 SHADER_READ
    // 带 SHADER_READ;UNDEFINED 起点无前置访问,src access 恒空。
    let (src_access, dst_access) = match (old_layout, new_layout) {
        (vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL) => {
            (vk::AccessFlags::empty(), vk::AccessFlags::TRANSFER_WRITE)
        }
        (vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL) => {
            (vk::AccessFlags::TRANSFER_WRITE, vk::AccessFlags::SHADER_READ)
        }
        _ => unreachable!("探针只用这两对 layout 转换"),
    };
    vk::ImageMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .base_mip_level(0)
                .level_count(1)
                .base_array_layer(0)
                .layer_count(1),
        )
}

/// 探针内联的 host-visible(HOST_VISIBLE|HOST_COHERENT 必需)buffer:UBO/索引/
/// 输出三块的载体。3.3.1 正式施工时按需扩 BufferRole,本组不替正式段做决定。
struct PlainBuffer {
    device: Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
}

impl PlainBuffer {
    /// # Safety
    /// `device` 须为合法未销毁句柄(ash 约定:Vulkan 参数合法性由调用方担保)。
    unsafe fn new(
        device: &Device,
        contract: &MemoryContract,
        size: u64,
        usage: vk::BufferUsageFlags,
        label: &str,
    ) -> Self {
        unsafe {
            let buffer = device
                .create_buffer(&vk::BufferCreateInfo::default().size(size).usage(usage), None)
                .expect("{label} 创建");
            let reqs = device.get_buffer_memory_requirements(buffer);
            // HOST_VISIBLE|HOST_COHERENT 作必需:规范保证至少一个此类内存类型存在,
            // coherent 下免 flush/invalidate,探针路径最短。
            let ty = contract
                .find_type(
                    reqs.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                    vk::MemoryPropertyFlags::empty(),
                )
                .expect("coherent host 内存契约无解");
            let memory = device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(reqs.size)
                        .memory_type_index(ty),
                    None,
                )
                .expect("分配");
            device
                .bind_buffer_memory(buffer, memory, 0)
                .expect("绑定");
            let mapped = device
                .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                .expect("持久映射")
                .cast::<u8>();
            println!(
                "[buffer] {label}: usage {usage:?} → {}B,内存类型 {ty}(coherent,免 flush)",
                reqs.size
            );
            Self {
                device: device.clone(),
                buffer,
                memory,
                mapped,
            }
        }
    }

    /// coherent 内存:宿主写免 flush,提交顺序即对设备可见。
    fn write(&self, bytes: &[u8]) {
        // # Safety:映射覆盖整个分配,调用点字节数 ≤ 创建尺寸(探针内常量)
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.mapped, bytes.len()) };
    }

    /// coherent 内存:设备写经 fence 等待后对宿主可见,免 invalidate。
    fn read(&self, out: &mut [u8]) {
        // # Safety:同 write
        unsafe { std::ptr::copy_nonoverlapping(self.mapped, out.as_mut_ptr(), out.len()) };
    }
}

impl Drop for PlainBuffer {
    fn drop(&mut self) {
        // unmap 显式先行(同 GpuBuffer 纪律),再拆 buffer、还 memory
        unsafe {
            self.device.unmap_memory(self.memory);
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// 一张 2×2 RGBA8 OPTIMAL 贴图:image + view + 独立分配的 DEVICE_LOCAL 内存
/// (显存机制篇约束②:image 不复用 MeshPool;逐张 allocate 在 2 张时远不触及
/// maxMemoryAllocationCount,子分配归 3.3.1 施工)。
struct TestImage {
    device: Device,
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
}

impl TestImage {
    /// # Safety
    /// `device` 须为合法未销毁句柄(ash 约定:Vulkan 参数合法性由调用方担保)。
    unsafe fn new(device: &Device, contract: &MemoryContract, label: &str) -> Self {
        unsafe {
            let image = device
                .create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .extent(vk::Extent3D { width: 2, height: 2, depth: 1 })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    None,
                )
                .expect("image 创建");
            let reqs = device.get_image_memory_requirements(image);
            let ty = contract
                .find_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL, vk::MemoryPropertyFlags::empty())
                .expect("DEVICE_LOCAL 契约无解");
            let memory = device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(reqs.size)
                        .memory_type_index(ty),
                    None,
                )
                .expect("贴图内存分配");
            device.bind_image_memory(image, memory, 0).expect("贴图绑定");
            let view = device
                .create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(vk::Format::R8G8B8A8_UNORM)
                        .subresource_range(
                            vk::ImageSubresourceRange::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .base_mip_level(0)
                                .level_count(1)
                                .base_array_layer(0)
                                .layer_count(1),
                        ),
                    None,
                )
                .expect("贴图视图");
            println!(
                "[image] {label}: 2×2 R8G8B8A8_UNORM OPTIMAL,{}B 分配,内存类型 {ty}(DEVICE_LOCAL,独立于 MeshPool)",
                reqs.size
            );
            Self {
                device: device.clone(),
                image,
                view,
                memory,
            }
        }
    }

    /// 反序拆 view → image → memory(调用方已保证无在飞引用)。
    fn destroy(self) {
        unsafe {
            self.device.destroy_image_view(self.view, None);
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// 三组 set 布局 + pipeline layout + push 常量范围(组 C 试验与组 D 负例共用):
/// set0 = b0 SAMPLED_IMAGE×4(UAB|PARTIALLY_BOUND)+ b1 SAMPLER×4(同旗标);
/// set1 = b0 UNIFORM_BUFFER;set2 = b0/b1 STORAGE_BUFFER;push = COMPUTE 0..96。
struct InterfaceShapes {
    device: Device,
    set_layouts: [vk::DescriptorSetLayout; 3],
    pipeline_layout: vk::PipelineLayout,
}

impl InterfaceShapes {
    /// # Safety
    /// `device` 须为合法未销毁句柄;设备须已启用相关 descriptor-indexing 特性位。
    unsafe fn new(device: &Device) -> Self {
        let uab_partially = vk::DescriptorBindingFlags::UPDATE_AFTER_BIND
            | vk::DescriptorBindingFlags::PARTIALLY_BOUND;
        // UAB|PARTIALLY_BOUND 旗标不走 binding 自身字段(ash 0.38 的
        // DescriptorSetLayoutBinding 无此字段),经 VkDescriptorSetLayoutBinding-
        // FlagsCreateInfo 扩展结构挂进 layout 创建;bindingCount 须与 layout 的
        // binding 数一致,故 set0 两个 binding 都给旗标位。
        let set0_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(4)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(4)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let set0_binding_flags = [uab_partially, uab_partially];
        let mut set0_flags_info = vk::DescriptorSetLayoutBindingFlagsCreateInfo::default()
            .binding_flags(&set0_binding_flags);
        unsafe {
            let set_layouts = [
                device
                    .create_descriptor_set_layout(
                        // UAB 旗标三层配套之一:layout 自身必须带
                        // UPDATE_AFTER_BIND_POOL 旗标(VUID-03000)——首跑被验证层
                        // 抓到缺这层,正是"旗标配套"纪律的活证据
                        &vk::DescriptorSetLayoutCreateInfo::default()
                            .flags(vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL)
                            .bindings(&set0_bindings)
                            .push_next(&mut set0_flags_info),
                        None,
                    )
                    .expect("set0 布局(常驻表)"),
                device
                    .create_descriptor_set_layout(
                        &vk::DescriptorSetLayoutCreateInfo::default().bindings(&[
                            vk::DescriptorSetLayoutBinding::default()
                                .binding(0)
                                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                                .descriptor_count(1)
                                .stage_flags(vk::ShaderStageFlags::COMPUTE),
                        ]),
                        None,
                    )
                    .expect("set1 布局(每帧 UBO)"),
                device
                    .create_descriptor_set_layout(
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
                    .expect("set2 布局(试验 I/O)"),
            ];
            let push_range = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .offset(0)
                .size(96)];
            let pipeline_layout = device
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&set_layouts)
                        .push_constant_ranges(&push_range),
                    None,
                )
                .expect("pipeline layout");
            Self {
                device: device.clone(),
                set_layouts,
                pipeline_layout,
            }
        }
    }
}

impl Drop for InterfaceShapes {
    fn drop(&mut self) {
        unsafe {
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            for layout in &self.set_layouts {
                self.device.destroy_descriptor_set_layout(*layout, None);
            }
        }
    }
}

// ============ 组 D:负例诊断 ============

/// 负例双案:未启用 runtimeDescriptorArray 特性的设备上——
/// ① 原生 naga 词流(模块连 RuntimeDescriptorArray capability 都没声明);
/// ② capability 补丁词流(capability 已声明,只缺特性)。
/// 探针纪律:故意违规只收账不断言驱动行为——"成功返回 ≠ 规范许可";断言只
/// 落在 VU_LOG 是否出现预期 VUID(实测钉号入施工记录)。
fn group_d_negative(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    raw_words: &[u32],
    patched_words: &[u32],
) {
    println!("\n== 组 D(负例诊断:关 runtimeDescriptorArray 特性,双案)==");
    let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    let gfx_family = families
        .iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .expect("无 graphics 族") as u32;
    let mut vk12 = feature_set(false); // runtime 位关,其余照常
    let priority = [1.0f32];
    let queue_infos = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(gfx_family)
        .queue_priorities(&priority)];
    let device = unsafe {
        instance
            .create_device(
                pd,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_infos)
                    .push_next(&mut vk12),
                None,
            )
            .expect("负例 vkCreateDevice")
    };
    let shapes = unsafe { InterfaceShapes::new(&device) };
    // 负例管线形状对齐组 C:同一套 set 布局 + push 范围,违规只落在 SPIR-V 一侧
    for (case, words) in [
        ("①原生词流(缺 capability)", raw_words),
        ("②补丁词流(capability 已声明)", patched_words),
    ] {
        let shader_module = unsafe {
            device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(words), None)
                .expect("负例 shader module")
        };
        let creation = unsafe {
            device.create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .stage(
                        vk::PipelineShaderStageCreateInfo::default()
                            .stage(vk::ShaderStageFlags::COMPUTE)
                            .module(shader_module)
                            .name(c"main"),
                    )
                    .layout(shapes.pipeline_layout)],
                None,
            )
        };
        match creation {
            Ok(pipelines) => {
                println!(
                    "[负例{case}] 管线创建返回成功——\"成功返回 ≠ 规范许可\",以验证层收账为准"
                );
                unsafe { device.destroy_pipeline(pipelines[0], None) };
            }
            Err((pipelines, result)) => {
                println!(
                    "[负例{case}] 管线创建被驱动拒绝({result:?});部分句柄 {count} 个照拆",
                    count = pipelines.len()
                );
                for p in pipelines {
                    unsafe { device.destroy_pipeline(p, None) };
                }
            }
        }
        let vu = drain_vu();
        let hits: Vec<&String> = vu
            .iter()
            .filter(|m| m.to_ascii_lowercase().contains("runtimedescriptorarray"))
            .collect();
        if hits.is_empty() {
            println!("[负例{case}] 未见含 RuntimeDescriptorArray 的 VUID——执法未发生,如实注明");
        } else {
            for m in hits {
                println!("[负例{case}] 已被验证层收账: {m}");
            }
        }
        for m in &vu {
            if !m.to_ascii_lowercase().contains("runtimedescriptorarray") {
                println!("[负例{case}] 其他收账消息: {m}");
            }
        }
        unsafe { device.destroy_shader_module(shader_module, None) };
    }
    unsafe { device.device_wait_idle() }.expect("负例收场 wait_idle");
    drop(shapes);
    unsafe { device.destroy_device(None) };
}

/// 组 C/D 共用的 Vulkan12Features 配方:descriptor_indexing 伞位 + 三个具体位,
/// runtime 位按候选决定——"支持 ≠ 启用",每一位都显式声明。
fn feature_set(runtime: bool) -> vk::PhysicalDeviceVulkan12Features<'static> {
    vk::PhysicalDeviceVulkan12Features::default()
        .descriptor_indexing(true)
        .runtime_descriptor_array(runtime)
        .shader_sampled_image_array_non_uniform_indexing(true)
        .descriptor_binding_sampled_image_update_after_bind(true)
        .descriptor_binding_partially_bound(true)
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
    let formatted = format!("[{:?}] {msg}", severity);
    VU_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(formatted.clone());
    VU_TOTAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(formatted);
    vk::FALSE
}
