// 反射段第一道：把合成写完的 HDR 抄一份到 `hdr_copy`（10.3.1）。
//
// 为什么需要这一步：SSR 合成 pass 要「读合成结果 + 写回合成结果」，同一张图不能一步里
// 既采样又写（反馈环），所以先抄一份让合成 pass 读拷贝、写原图。抄的是全分辨率 RGBA16F
// （1600×900 约 11.5 MB 写带宽），关档时连同本 pass 一起跳过。
//
// 集合：只有 G-buffer 采样集（b2 = HDR 中间目标、b4 = 采样器），不需要帧 UBO。

// #import fullscreen

@group(0) @binding(2) var hdr_tex: texture_2d<f32>;
@group(0) @binding(4) var gbuf_sampler: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    // 逐像素抄，alpha 无用（透明层还没画，此处 HDR 的 a 恒 1）
    return vec4f(textureSampleLevel(hdr_tex, gbuf_sampler, in.uv, 0.0).rgb, 1.0);
}
