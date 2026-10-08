//! 着色器编译管线（3.3.3 落地，兑现 2026-09-28 "runtime+capability 补丁器"定案，
//! 见 3.3 前置闸门施工记录 §4）：
//!
//! `examples/shaders/*.wgsl` → naga 30.0.1（wgsl-in → spv-out）→ spirv-val 闸门 →
//! （被 VUID-04680 拒的 runtime 模块）capability 补丁 → 复跑 spirv-val 兜底 →
//! `OUT_DIR/<stem>.spv`（小端词流）。探针经
//! `include_bytes!(concat!(env!("OUT_DIR"), "/<stem>.spv"))` 消费，与 3.4 正式
//! 管线同一编译路径。
//!
//! spirv-val 定位：`VULKAN_SDK` 环境变量优先，回退本机 1.4.357.0 默认安装路径。
//! - **有 spirv-val**：先验原生输出，仅被拒模块打补丁（补丁幂等，已声明的
//!   capability 不重复注）——与闸门组 B 完全同流程，"原生即合法"的证据保留。
//! - **无 spirv-val**：构建降级——全部模块无条件打补丁 + 响亮警告。补丁是纯
//!   声明性插词（capability 只收紧设备要求不放松合法性），合法性最终由运行期
//!   验证层在 `vkCreateShaderModule` 内嵌执法（VUID-04680/08740）兜底；但"缺
//!   官方工具的静默不冒充证据"红线不变——此分支下构建日志会明示证据链降级。

use std::path::{Path, PathBuf};

use naga::back::spv;
use naga::valid::{Capabilities, Validator, ValidationFlags};

// SPIR-V 语法常量（与前置闸门探针 bindless_probe.rs 同表：值由规范钉死）。
const OP_CAPABILITY: u16 = 17;
const OP_EXTENSION: u16 = 10;
const CAP_RUNTIME_DESCRIPTOR_ARRAY: u32 = 5302;
const CAP_SAMPLED_IMAGE_ARRAY_NON_UNIFORM: u32 = 5307;
const EXT_DESCRIPTOR_INDEXING: &str = "SPV_EXT_descriptor_indexing";

fn main() {
    let shaders_dir = Path::new("examples/shaders");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("cargo 提供 OUT_DIR"));
    let mut wgsl_paths: Vec<PathBuf> = std::fs::read_dir(shaders_dir)
        .unwrap_or_else(|e| panic!("读 {shaders_dir:?} 失败: {e}"))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
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
        let words = compile(path, &stem);
        let words = gate_and_patch(words, &stem, val_exe.as_deref());
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let out = out_dir.join(format!("{stem}.spv"));
        std::fs::write(&out, &bytes).unwrap_or_else(|e| panic!("写 {out:?} 失败: {e}"));
    }
}

/// WGSL → SPIR-V 词流。编译失败 = 构建错误（panic 带完整错误链，不静默）。
fn compile(path: &Path, stem: &str) -> Vec<u32> {
    let source = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("读 {:?} 失败: {e}", path));
    let module = match naga::front::wgsl::parse_str(&source) {
        Ok(m) => m,
        Err(e) => panic!("[{stem}] WGSL 解析失败: {e}"),
    };
    // 能力面与闸门探针同款显式声明：binding 数组、非一致索引、var<immediate>。
    let caps = Capabilities::IMMEDIATES
        | Capabilities::TEXTURE_AND_SAMPLER_BINDING_ARRAY
        | Capabilities::TEXTURE_AND_SAMPLER_BINDING_ARRAY_NON_UNIFORM_INDEXING;
    let mut validator = Validator::new(ValidationFlags::all(), caps);
    let info = match validator.validate(&module) {
        Ok(i) => i,
        Err(e) => panic!("[{stem}] naga IR 校验失败: {e}"),
    };
    match spv::write_vec(&module, &info, &spv::Options::default(), None) {
        Ok(w) => w,
        Err(e) => panic!("[{stem}] SPIR-V 生成失败: {e}"),
    }
}

/// spirv-val 闸门 + 按需补丁（定案流程：原生被 VUID-04680 拒 → 补丁 → 复跑兜底）。
/// 无 spirv-val 时降级为无条件补丁（见模块注释）。
fn gate_and_patch(words: Vec<u32>, stem: &str, val_exe: Option<&Path>) -> Vec<u32> {
    match val_exe {
        Some(val_exe) => match run_spirv_val(val_exe, &words) {
            Ok(()) => {
                println!("cargo:warning=[{stem}] spirv-val 原生输出通过（零补丁）");
                words
            }
            Err(native_error) => {
                println!("cargo:warning=[{stem}] 原生输出被 spirv-val 拒（预期缺口：RuntimeDescriptorArray capability 不发射）——打补丁重验");
                let patched = patch_descriptor_indexing(&words);
                if let Err(patch_error) = run_spirv_val(val_exe, &patched) {
                    panic!(
                        "[{stem}] 补丁后仍被 spirv-val 拒——模块无合法形态\n-- 原生错误 --\n{native_error}\n-- 补丁后错误 --\n{patch_error}"
                    );
                }
                println!("cargo:warning=[{stem}] 补丁后通过 spirv-val（runtime+补丁器定案流程）");
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
    let spv_path = std::env::temp_dir().join("ash_renderer_build_val.spv");
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

/// 最小确定性补丁（与闸门组 B 同款）：capability 段注入缺失的
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
