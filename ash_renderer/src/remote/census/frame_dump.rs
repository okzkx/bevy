//! `ash_renderer/frame_dump`：把 LDR 后链目标（tonemap 之后、SMAA 之前的完整帧）
//! 拷回宿主并写 PPM（P6）。PrintWindow/屏幕抓图在锁屏、DLP 遮挡或远程会话下会
//! 拿到白图/桌面（见截图伪影清单），渲染器自身的回读不依赖桌面会话，是画面取证的
//! 兜底通道。只进验证，不进绘制路径。

use std::io::Write;

use bevy::{prelude::*, remote::BrpResult};
use serde_json::{json, Value};

use ash_macros::system;

use crate::render::{Context, SmaaTargets, MAX_FRAMES_IN_FLIGHT};

use crate::remote::common::params::{internal_err, invalid_params, parse_params};

/// `ash_renderer/frame_dump` 参数：`path` 必填（宿主机绝对路径，写 .ppm），
/// `slot` 可选（帧槽 0/1，缺省 0；两槽轮转，读到的帧 ≤1 帧陈旧）。
#[derive(serde::Deserialize)]
struct FrameDumpParams {
    path: String,
    #[serde(default)]
    slot: Option<u32>,
}

/// `ash_renderer/frame_dump`：读回 LDR 后链目标并写 PPM。帧内容 = 当前着色模式/
/// 漫反射编码下的完整画面（含天空与 UI 之后的 SMAA 输入态），用来做 A/B 与视觉
/// 验证的取证。
///
/// # Errors
/// 参数缺失 / 槽号越界 / Vulkan 资源缺席 / 读回或写文件失败。
#[system]
pub(crate) fn frame_dump(In(params): In<Option<Value>>, world: &mut World) -> BrpResult {
    let p: FrameDumpParams = parse_params(params, "params 缺失：需要 path（写 .ppm）")?;
    let slot = p.slot.unwrap_or(0) as usize;
    if slot >= MAX_FRAMES_IN_FLIGHT {
        return Err(invalid_params(format!(
            "slot {slot} 越界（帧槽 0..{MAX_FRAMES_IN_FLIGHT}）"
        )));
    }
    let (Some(ctx), Some(smaa)) = (
        world.get_resource::<Context>(),
        world.get_resource::<SmaaTargets>(),
    ) else {
        return Err(invalid_params("Vulkan 资源缺席——SMAA/LDR 目标未初始化"));
    };
    let rgba = smaa
        .readback_ldr(ctx, slot)
        .map_err(internal_err("LDR 帧读回失败"))?;
    let extent = smaa.ldr_extent();
    let (width, height) = (extent.width as usize, extent.height as usize);
    // PPM P6：头 + RGB（LDR 是 R8G8B8A8_UNORM，alpha 丢弃）
    let mut ppm = Vec::with_capacity(width * height * 3 + 32);
    write!(&mut ppm, "P6\n{width} {height}\n255\n").expect("写内存缓冲不失败");
    for texel in rgba.as_chunks::<4>().0 {
        ppm.extend_from_slice(&texel[..3]);
    }
    std::fs::write(&p.path, &ppm).map_err(|e| internal_err("写 PPM 失败")(e))?;
    Ok(json!({
        "path": p.path,
        "width": width,
        "height": height,
        "slot": slot,
        "bytes": ppm.len(),
    }))
}
