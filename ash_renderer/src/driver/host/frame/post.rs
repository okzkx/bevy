//! 延迟光照与后处理的帧侧单元：取用束（GBufferInput/PostInput）与帧槽 UBO 写入、
//! Bloom/SMAA 录制输入准备。管线本体与录制在 [`crate::render::passes`]。

use bevy::{ecs::system::SystemParam, prelude::*};

use crate::{
    common::{bytes::put_f32s, syntax::ResultTierExt},
    render::{
        pack_bloom_params, BloomFrameDraw, BloomParamsData, BloomPipelines, BloomSettings,
        BloomTargets, ColorScopeBindings, ColorScopeDraw, ColorScopePipelines, ColorScopeSettings,
        ColorScopeTargets, CompositePipeline, Context, ExposureDraw, ExposureParamsData,
        ExposurePipelines, ExposureSettings, ExposureTargets, GBufferPipeline, GBufferTargets,
        PresentTonemapPipeline, SmaaEdgeBlendDraw, SmaaNbDraw, SmaaPipelines, SmaaSettings,
        SmaaTargets,
    },
};

/// G-buffer 管线与附件（附件随帧槽深度重建）。
#[derive(SystemParam)]
pub(super) struct GBufferInput<'w> {
    pub(super) pipeline: Res<'w, GBufferPipeline>,
    pub(super) targets: ResMut<'w, GBufferTargets>,
}

/// 延迟光照与后处理：G-buffer + HDR 中间目标（随 resize 重建）、
/// 合成管线（G-buffer 光照 → HDR）、present tonemap（HDR + Bloom → ACES →
/// LDR）、Bloom 链与 SMAA 的资源/管线/开关。
#[derive(SystemParam)]
pub(super) struct PostInput<'w> {
    pub(super) gbuffer: GBufferInput<'w>,
    pub(super) composite: Res<'w, CompositePipeline>,
    pub(super) tonemap: Res<'w, PresentTonemapPipeline>,
    pub(super) bloom: ResMut<'w, BloomTargets>,
    pub(super) bloom_pipelines: Res<'w, BloomPipelines>,
    pub(super) bloom_settings: Res<'w, BloomSettings>,
    pub(super) smaa: ResMut<'w, SmaaTargets>,
    pub(super) smaa_pipelines: Res<'w, SmaaPipelines>,
    pub(super) smaa_settings: Res<'w, SmaaSettings>,
    /// 自动曝光（独立显示段模块）：目标/管线/开关与时间步长。
    pub(super) exposure: ResMut<'w, ExposureTargets>,
    pub(super) exposure_pipelines: Res<'w, ExposurePipelines>,
    pub(super) exposure_settings: Res<'w, ExposureSettings>,
    /// 颜色校验采样（9.9）：三域直方图 bins、采样管线与开关。
    pub(super) color_scope: ResMut<'w, ColorScopeTargets>,
    pub(super) color_scope_pipelines: Res<'w, ColorScopePipelines>,
    pub(super) color_scope_settings: ResMut<'w, ColorScopeSettings>,
    pub(super) ctx: Res<'w, Context>,
    pub(super) time: Res<'w, Time>,
}

impl PostInput<'_> {
    /// 后处理帧槽 UBO：Bloom 五标量 + SmaaInfo（rt_metrics + 开关）。
    pub(super) fn write_slot_ubos(&mut self, slot: usize) -> Result<(), BevyError> {
        let bloom = &self.bloom_settings;
        self.bloom
            .ubo(slot)
            .write(
                0,
                &pack_bloom_params(&BloomParamsData {
                    strength: bloom.strength,
                    enabled: f32::from(bloom.enabled),
                    tonemap: f32::from(bloom.tonemap),
                    curve: bloom.curve.as_f32(),
                    gain: bloom.gain,
                    tonemap_strength: bloom.tonemap_strength,
                }),
            )
            .or_fatal("Bloom 参数 UBO 写失败，渲染链无法继续")?;
        let ldr = self.smaa.ldr_extent();
        let (sw, sh) = (ldr.width.max(1), ldr.height.max(1));
        let mut smaa_info = [0u8; 32];
        put_f32s(
            &mut smaa_info,
            0,
            &[
                1.0 / sw as f32,
                1.0 / sh as f32,
                sw as f32,
                sh as f32,
                f32::from(self.smaa_settings.enabled),
                0.0,
                0.0,
                0.0,
            ],
        );
        self.smaa
            .info_mut(slot)
            .write(0, &smaa_info)
            .or_fatal("SmaaInfo UBO 写失败，渲染链无法继续")?;
        // 自动曝光：参数 UBO + 本槽 HDR 描述符（HDR 随 resize 重建，故每帧按视图比对）
        let settings = &self.exposure_settings;
        let dt = self.time.delta_secs().clamp(1.0 / 240.0, 0.1);
        self.exposure
            .write_params(
                slot,
                &ExposureParamsData {
                    mode: settings.mode.as_u32(),
                    key: settings.key,
                    min_scale: settings.min_scale,
                    max_scale: settings.max_scale,
                    rate_brighten: settings.rate_brighten,
                    rate_darken: settings.rate_darken,
                    dt,
                },
            )
            .or_fatal("曝光参数 UBO 写失败，渲染链无法继续")?;
        let hdr_view = self.gbuffer.targets.hdr_view(slot);
        let ctx = &self.ctx;
        self.exposure
            .bind_hdr(ctx, slot, hdr_view)
            .or_fatal("曝光描述符集写失败，渲染链无法继续")
    }
}

/// 自动曝光的帧录制输入：测光 → 适应两条 dispatch + tonemap 读倍数的 set。
/// 调试显示模式下 `None`（HDR 里是可视化直出，不该影响适应状态）。
pub(super) fn exposure_draw(post: &PostInput, slot: usize) -> ExposureDraw {
    let pipelines = &post.exposure_pipelines;
    ExposureDraw {
        measure: pipelines.measure(),
        adapt: pipelines.adapt(),
        set: post.exposure.set(slot),
        state_set: post.exposure.state_set(slot),
    }
}

/// Bloom 链录制输入：开关开才录九个全屏 pass；关 = None（tonemap 侧 shader
/// 不采样 bloom，up0 陈旧无消费）。down 集从 level 1 起（level 0 由 prefilter 写）。
pub(super) fn bloom_draw(post: &PostInput, slot: usize) -> Option<BloomFrameDraw> {
    if !post.bloom_settings.enabled {
        return None;
    }
    let (targets, pipelines) = (&post.bloom, &post.bloom_pipelines);
    Some(BloomFrameDraw {
        prefilter: pipelines.prefilter_rec(),
        down: pipelines.down_rec(),
        up: pipelines.up_rec(),
        gbuffer_set: post.gbuffer.targets.sampled_set(slot),
        down_sets: std::array::from_fn(|i| targets.set_down(i + 1, slot)),
        up_sets: std::array::from_fn(|i| targets.set_up(i, slot)),
        images_down: std::array::from_fn(|i| targets.down_image(i)),
        images_up: std::array::from_fn(|i| targets.up_image(i)),
        views_down: std::array::from_fn(|i| targets.down_view(i)),
        views_up: std::array::from_fn(|i| targets.up_view(i)),
        extents: std::array::from_fn(|i| targets.level_extent(i)),
    })
}

/// SMAA 录制输入：edge/blend 开关开 = Some；neighborhood 恒在（LDR → swapchain
/// 的唯一出口）。
pub(super) fn smaa_draws(post: &PostInput, slot: usize) -> (Option<SmaaEdgeBlendDraw>, SmaaNbDraw) {
    let (targets, pipelines) = (&post.smaa, &post.smaa_pipelines);
    let edge_blend = post.smaa_settings.enabled.then(|| SmaaEdgeBlendDraw {
        edge: pipelines.edge_rec(),
        blend: pipelines.blend_rec(),
        edge_set: targets.set_edge(slot),
        blend_set: targets.set_blend(slot),
        edges_view: targets.edges_view(slot),
        edges_image: targets.edges_image(slot),
        blend_view: targets.blend_view(slot),
        blend_image: targets.blend_image(slot),
        extent: targets.ldr_extent(),
    });
    let nb = SmaaNbDraw {
        nb: pipelines.neighborhood_rec(),
        nb_set: targets.set_nb(slot),
        ldr_view: targets.ldr_view(slot),
        ldr_image: targets.ldr_image(slot),
        ldr_extent: targets.ldr_extent(),
    };
    (edge_blend, nb)
}

/// 颜色校验采样录制输入（9.9）：开关开且 tonemap 链启用才录一条 dispatch。
/// `chain_active` 由调用方按"非调试模式 + tonemap 开关"给出——调试直出时 LDR 里是
/// 原始 HDR 可视化，C 域没有意义，面板显示"链未启用，无 C 域"。
pub(super) fn color_scope_draw(
    post: &mut PostInput,
    slot: usize,
    chain_active: bool,
) -> Result<Option<ColorScopeDraw>, BevyError> {
    let request_once = post.color_scope_settings.once;
    if !(post.color_scope_settings.enabled || request_once) || !chain_active {
        return Ok(None);
    }
    if request_once {
        post.color_scope_settings.once = false;
    }
    let PostInput {
        gbuffer,
        bloom,
        exposure,
        smaa,
        ctx,
        color_scope,
        color_scope_pipelines,
        ..
    } = post;
    let extent = smaa.ldr_extent();
    let _frame = color_scope
        .begin_frame(slot, extent)
        .or_fatal("颜色校验 bins 清零/写头失败，渲染链无法继续")?;
    let bindings = ColorScopeBindings {
        hdr: gbuffer.targets.hdr_view(slot),
        blit_sampler: gbuffer.targets.sampler(),
        bloom: bloom.up_view(0),
        bloom_sampler: bloom.sampler(),
        bloom_params: bloom.params_buffer(slot),
        exposure_state: exposure.state_buffer(slot),
        ldr: smaa.ldr_view(slot),
    };
    color_scope
        .bind_resources(ctx, slot, &bindings)
        .or_fatal("颜色校验描述符集写失败，渲染链无法继续")?;
    Ok(Some(ColorScopeDraw::new(
        color_scope_pipelines.handles(),
        color_scope.set(slot),
        extent,
    )))
}
