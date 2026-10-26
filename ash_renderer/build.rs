//! 着色器编译管线：`shaders/*.wgsl`（入口模块；`read_dir` 不递归，`shaders/common/`
//! 下的公共片段不单独编译）→ 展开 `// #import <name>` → naga 30.0.1（wgsl-in →
//! spv-out）→ spirv-val 闸门 →（被 VUID-04680 拒的 runtime 模块）capability 补丁 →
//! 复跑 spirv-val 兜底 → `OUT_DIR/<stem>.spv`（小端词流），由
//! `include_bytes!(concat!(env!("OUT_DIR"), "/<stem>.spv"))` 消费。
//!
//! 公共片段：独占一行的 `// #import <name>` 展开为 `shaders/common/<name>.wgsl` 全文，
//! 递归展开；同一入口内每个片段只展开一次（菱形依赖去重），循环引用 = 构建错误。
//! 片段只放 struct / 常量 / 函数，`@group/@binding` 声明留在各入口（组号随管线
//! 布局走，同一资源在不同入口里组号/绑定号可能不同）；片段函数若引用全局绑定，
//! 由导入方声明，片段头注写明所需名字。展开结果落 `OUT_DIR/<stem>.expanded.wgsl`，
//! naga 报错的行列号以该文件为准。
//!
//! spirv-val 定位：`VULKAN_SDK` 环境变量优先，回退本机 1.4.357.0 默认安装路径。
//! - **有 spirv-val**：先验原生输出，仅被拒模块打补丁（补丁幂等，已声明的
//!   capability 不重复注）。
//! - **无 spirv-val**：全部模块无条件打补丁 + `cargo:warning`。补丁是纯声明性插词
//!   （capability 只收紧设备要求不放松合法性），合法性由运行期验证层在
//!   `vkCreateShaderModule` 内执法（VUID-04680/08740）兜底；构建日志明示证据链降级。
//!
//! 输出纪律：`cargo:warning` 只承载真正异常（缺 spirv-val 降级）；验证轨迹
//! （原生通过/预期被拒/补丁后通过）走 stderr——cargo 捕获下平时静默，仅在脚本
//! 失败时随错误展示（或 `cargo build -vv` 全量可见），不污染 rustc 警告流。

use std::path::{Path, PathBuf};

use naga::back::spv;
use naga::valid::{Capabilities, Validator, ValidationFlags};

// SPIR-V 语法常量（与 examples/bindless_probe.rs 同表：值由规范钉死）。
const OP_CAPABILITY: u16 = 17;
const OP_EXTENSION: u16 = 10;
const CAP_RUNTIME_DESCRIPTOR_ARRAY: u32 = 5302;
const CAP_SAMPLED_IMAGE_ARRAY_NON_UNIFORM: u32 = 5307;
const EXT_DESCRIPTOR_INDEXING: &str = "SPV_EXT_descriptor_indexing";

/// 公共片段导入指令（独占一行，前导空白允许）。
const IMPORT_DIRECTIVE: &str = "// #import ";

/// 不产 spv 的入口：`examples/bindless_probe.rs` 以 `include_str!` 读入源码后自行
/// 编译、扫描与 spirv-val，构建脚本的产物无人消费。
const STANDALONE_SOURCES: &[&str] = &["bindless_probe_fixed", "bindless_probe_runtime"];

fn main() {
    let shaders_dir = Path::new("shaders");
    let common_dir = shaders_dir.join("common");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("cargo 提供 OUT_DIR"));
    // 目录级 rerun-if-changed：逐文件声明对"新增文件"无感知，新落的 .wgsl 不会
    // 触发重编译。目录声明覆盖增删（cargo 递归扫描，含 common/），逐文件声明
    // 保留（内容变更逐文件精确触发）。
    println!("cargo:rerun-if-changed={}", shaders_dir.display());
    println!("cargo:rerun-if-env-changed=VULKAN_SDK");
    let mut wgsl_paths: Vec<PathBuf> = std::fs::read_dir(shaders_dir)
        .unwrap_or_else(|e| panic!("读 {shaders_dir:?} 失败: {e}"))
        .map(|e| e.unwrap_or_else(|e| panic!("枚举 {shaders_dir:?} 条目失败: {e}")).path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "wgsl"))
        .collect();
    wgsl_paths.sort();
    assert!(
        !wgsl_paths.is_empty(),
        "{shaders_dir:?} 下没有任何 .wgsl——着色器源是编译管线的输入，缺失即构建错误"
    );
    let val_exe = locate_spirv_val();
    if val_exe.is_none() {
        println!("cargo:warning=未找到 spirv-val（VULKAN_SDK 未设且默认路径不存在）——shader 构建降级为无条件补丁，合法证据只剩运行期验证层");
    }
    for path in &wgsl_paths {
        let stem = path.file_stem().expect("有扩展名的路径必有文件名").to_string_lossy().into_owned();
        println!("cargo:rerun-if-changed={}", path.display());
        if STANDALONE_SOURCES.contains(&stem.as_str()) {
            continue;
        }
        let source = expand_imports(path, &common_dir);
        let expanded = out_dir.join(format!("{stem}.expanded.wgsl"));
        std::fs::write(&expanded, &source).unwrap_or_else(|e| panic!("写 {expanded:?} 失败: {e}"));
        let words = compile(&source, &expanded, &stem);
        let words = gate_and_patch(words, &stem, val_exe.as_deref());
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let out = out_dir.join(format!("{stem}.spv"));
        std::fs::write(&out, &bytes).unwrap_or_else(|e| panic!("写 {out:?} 失败: {e}"));
    }
    // MSVC 默认主线程栈 1MB 不够 init 链（Context::new 的 ash 枚举 + InitChain 元组）
    // 的栈深，会稳定栈溢出；主线程栈保留提到 16MB（Rust 子线程 2MB 默认不受影响）。
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg=/STACK:16777216");
    }
}

/// 展开入口模块的 `// #import <name>`（见模块注释）。
fn expand_imports(entry: &Path, common_dir: &Path) -> String {
    let mut out = String::new();
    let mut done: Vec<String> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    expand_into(entry, common_dir, &mut out, &mut done, &mut stack);
    out
}

fn expand_into(
    path: &Path,
    common_dir: &Path,
    out: &mut String,
    done: &mut Vec<String>,
    stack: &mut Vec<String>,
) {
    let source = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("读 {path:?} 失败: {e}"));
    for line in source.lines() {
        let Some(name) = line.trim_start().strip_prefix(IMPORT_DIRECTIVE) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let name = name.trim();
        assert!(
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "{path:?}: 非法片段名 `{name}`（只允许 [A-Za-z0-9_]，对应 shaders/common/<name>.wgsl）"
        );
        if stack.iter().any(|n| n == name) {
            panic!("{path:?}: 公共片段循环引用 {} -> {name}", stack.join(" -> "));
        }
        if done.iter().any(|n| n == name) {
            continue;
        }
        let fragment = common_dir.join(format!("{name}.wgsl"));
        assert!(fragment.is_file(), "{path:?}: 导入的片段 {fragment:?} 不存在");
        println!("cargo:rerun-if-changed={}", fragment.display());
        stack.push(name.to_owned());
        out.push_str(&format!("// ---- #import {name} begin\n"));
        expand_into(&fragment, common_dir, out, done, stack);
        out.push_str(&format!("// ---- #import {name} end\n"));
        stack.pop();
        done.push(name.to_owned());
    }
}

/// 展开后的 WGSL → SPIR-V 词流。编译失败 = 构建错误（panic 带完整错误链，不静默）。
fn compile(source: &str, expanded: &Path, stem: &str) -> Vec<u32> {
    let expanded_name = expanded.display().to_string();
    let module = match naga::front::wgsl::parse_str(source) {
        Ok(m) => m,
        Err(e) => panic!(
            "[{stem}] WGSL 解析失败（行列号对应展开文件 {expanded_name}）:\n{}",
            e.emit_to_string_with_path(source, &expanded_name)
        ),
    };
    assert_host_layout(&module, stem);
    // 能力面与 bindless 探针同款显式声明：binding 数组、非一致索引、var<immediate>。
    let caps = Capabilities::IMMEDIATES
        | Capabilities::TEXTURE_AND_SAMPLER_BINDING_ARRAY
        | Capabilities::TEXTURE_AND_SAMPLER_BINDING_ARRAY_NON_UNIFORM_INDEXING;
    let mut validator = Validator::new(ValidationFlags::all(), caps);
    let info = match validator.validate(&module) {
        Ok(i) => i,
        Err(e) => panic!(
            "[{stem}] naga IR 校验失败（行列号对应展开文件 {expanded_name}）:\n{}",
            e.emit_to_string_with_path(source, &expanded_name)
        ),
    };
    let mut options = spv::Options::default();
    // 坑点：全部管线的正高度 viewport 朝向依赖 naga 对 @builtin(position) 注入的
    // Y 翻转；默认值已含此位，显式写出防默认值变动后画面整体上下颠倒。
    options.flags.insert(spv::WriterFlags::ADJUST_COORDINATE_SPACE);
    match spv::write_vec(&module, &info, &options, None) {
        Ok(w) => w,
        Err(e) => panic!("[{stem}] SPIR-V 生成失败: {e}"),
    }
}

/// 跨端布局互证闸门：与宿主打包逐字节同源的记录结构体，naga 算出的
/// 结构体 span（字节数）必须与宿主常量相等——WGSL 侧结构体的成员对齐
///（如 `vec3<u32>` 对齐 16）会把步长撑得比宿主打包大，材质/对象槽 ≥1
/// 全体错位（实例：MaterialRecord 尾垫用 vec3<u32> 时步长 80 ≠ 宿主 64）。
/// 宿主常量正本 = `src/render/assets/instance_table.rs`（MATERIAL_RECORD_SIZE
/// / OBJECT_RECORD_SIZE 等）与 `src/render/shading/light_table.rs`
/// （LIGHT_RECORD_SIZE）；新增记录结构时在此登记。
/// 注：span = 结构体字节数；storage 数组步长 = span 向上取整到对齐，span
/// 相等且对齐不超宿主步长即步长相等。
const HOST_LAYOUT_GATE: &[(&str, u32)] = &[
    ("MaterialRecord", 64),
    ("ObjectRecord", 48),
    ("LightRecord", 64),
    ("LodEntry", 12),
    ("IndirectCommand", 20),
];

fn assert_host_layout(module: &naga::Module, stem: &str) {
    for (_, ty) in module.types.iter() {
        let Some(name) = &ty.name else { continue };
        let Some((_, expect)) = HOST_LAYOUT_GATE.iter().find(|(n, _)| n == name) else {
            continue;
        };
        let naga::TypeInner::Struct { span, .. } = &ty.inner else {
            continue;
        };
        assert!(
            span == expect,
            "[{stem}] WGSL 结构体 {name} 跨端布局破缺:naga span {span}B ≠ 宿主记录 {expect}B\
             ——成员对齐把步长撑大了,shader 端 storage 数组索引将与宿主打包错位\
             （垫尾用标量 u32,不用 vec3/vec4;详见 assert_host_layout 注释）"
        );
    }
}

/// spirv-val 闸门 + 按需补丁（原生被 VUID-04680 拒 → 补丁 → 复跑兜底）。
/// 无 spirv-val 时降级为无条件补丁（见模块注释）。
fn gate_and_patch(words: Vec<u32>, stem: &str, val_exe: Option<&Path>) -> Vec<u32> {
    match val_exe {
        Some(val_exe) => match run_spirv_val(val_exe, &words) {
            Ok(()) => {
                eprintln!("[{stem}] spirv-val 原生输出通过（零补丁）");
                words
            }
            Err(native_error) => {
                eprintln!("[{stem}] 原生输出被 spirv-val 拒（预期缺口：RuntimeDescriptorArray capability 不发射）——打补丁重验");
                let patched = patch_descriptor_indexing(&words);
                if let Err(patch_error) = run_spirv_val(val_exe, &patched) {
                    panic!(
                        "[{stem}] 补丁后仍被 spirv-val 拒——模块无合法形态\n-- 原生错误 --\n{native_error}\n-- 补丁后错误 --\n{patch_error}"
                    );
                }
                eprintln!("[{stem}] 补丁后通过 spirv-val");
                patched
            }
        },
        None => {
            // 降级分支：无条件补丁。声明多余 capability 只收紧设备要求，运行期
            // 验证层仍按 VUID-04680/08740 执法兜底
            patch_descriptor_indexing(&words)
        }
    }
}

/// 定位 SDK 的 spirv-val：先 VULKAN_SDK 环境变量，回退 1.4.357.0 默认安装路径。
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

/// 跑一次 spirv-val：词流写临时 .spv，--target-env vulkan1.3。被拒返回 Err 文本。
fn run_spirv_val(val_exe: &Path, words: &[u32]) -> Result<(), String> {
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    // 临时文件名混 pid：并发 cargo 调用（多 target/多工作区共享构建脚本路径）
    // 不互踩同一 .spv
    let spv_path = std::env::temp_dir().join(format!(
        "ash_renderer_build_val_{}.spv",
        std::process::id()
    ));
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

/// 最小确定性补丁（与 examples/bindless_probe.rs 组 B 同款）：capability 段注入缺失的
/// RuntimeDescriptorArray / SampledImageArrayNonUniformIndexing + 扩展段注入
/// SPV_EXT_descriptor_indexing。capability 必须插在 extension 区段之前
///（SPIR-V 逻辑布局顺序），分两个插入点拼接。
fn patch_descriptor_indexing(words: &[u32]) -> Vec<u32> {
    let mut caps_present: Vec<u32> = Vec::new();
    let mut exts_present: Vec<String> = Vec::new();
    // 第一遍：吃掉开头连续的 OpCapability，记下 capability 区段结束点
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
    // 第二遍：从 capability 结束点继续吃 OpExtension，记下 extension 区段结束点
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

/// SPIR-V 字符串字面量解码：NUL 结尾、每字 4 字节小端、零填充。
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
