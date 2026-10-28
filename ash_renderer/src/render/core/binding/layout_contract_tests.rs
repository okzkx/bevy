//! 布局契约测试：全部管线配方与 shader ABI 对账、全部 set 声明自检，以及逐类负例
//!（改错 set 顺序/binding/描述符种类/阶段/数组形状/push/入口/布局旗标都必须被拒）。
//! 不需要 GPU：对账只读静态声明与 build.rs 生成的 ABI 表。

use ash::vk::{self, Handle};

use super::pipeline_layout::{PipelineDesc, PushRange};
use super::set_layout::{BindingDesc, DescKind, SetLayout, SetLayoutDesc, SetLayouts};
use super::shader_abi::{self, Shader};
use crate::render::passes::gbuffer::gbuffer_pipeline::GBUFFER_WRITE_PIPELINE;
use crate::render::passes::gbuffer::gbuffer_targets::GBUFFER_SAMPLED_SET;
use crate::render::passes::lighting::cluster::{CLUSTER_BUILD_PIPELINE, LIGHT_CLUSTER_SET};
use crate::render::passes::lighting::composite_pipeline::GBUFFER_COMPOSITE_PIPELINE;
use crate::render::passes::overlay::overlay_pipeline::OVERLAY_PIPELINE;
use crate::render::passes::post::bloom::{
    BLOOM_DOWN_PIPELINE, BLOOM_PREFILTER_PIPELINE, BLOOM_TEX_SET, BLOOM_UP_PIPELINE, BLOOM_UP_SET,
};
use crate::render::passes::post::present_tonemap_pipeline::PRESENT_TONEMAP_PIPELINE;
use crate::render::passes::post::smaa::{
    SMAA_A_SET, SMAA_BLEND_PIPELINE, SMAA_B_SET, SMAA_EDGE_PIPELINE, SMAA_NEIGHBORHOOD_PIPELINE,
};
use crate::render::passes::shadow::shadow_build::SHADOW_BUILD_PIPELINE;
use crate::render::passes::shadow::shadow_map::{SHADOW_SET, SHADOW_WRITE_PIPELINE, SHADOW_BINDING_UNIFORM};
use crate::render::passes::transparent::{
    TRANSPARENT_COLLECT_PIPELINE, TRANSPARENT_FORWARD_PIPELINE, TRANSPARENT_GATHER_PIPELINE, TRANSPARENT_SET,
    TRANSPARENT_SORT_PIPELINE,
};
use crate::render::passes::visibility::cull::CULL_PIPELINE;
use crate::render::passes::visibility::hiz::{HIZ_DOWNSAMPLE_PIPELINE, HIZ_OCCLUSION_SET, HIZ_REDUCE_SET};
use crate::render::passes::visibility::indirect::BUILD_INDIRECT_PIPELINE;
use crate::render::passes::visibility::lod::LOD_SELECT_PIPELINE;
use crate::render::passes::visibility::occlusion::OCCLUSION_PIPELINE;
use crate::render::resident::descriptors::{BINDLESS_SET, FRAME_SET, TABLE_CAPACITY};
use crate::render::resident::instance_table::INSTANCE_SET;

/// 宿主建的全部管线。新增管线要登记在这里，否则 `every_shader_entry_has_a_pipeline` 会报。
const ALL_PIPELINES: &[&PipelineDesc] = &[
    &GBUFFER_WRITE_PIPELINE,
    &SHADOW_WRITE_PIPELINE,
    &TRANSPARENT_FORWARD_PIPELINE,
    &OVERLAY_PIPELINE,
    &GBUFFER_COMPOSITE_PIPELINE,
    &BLOOM_PREFILTER_PIPELINE,
    &BLOOM_DOWN_PIPELINE,
    &BLOOM_UP_PIPELINE,
    &SMAA_EDGE_PIPELINE,
    &SMAA_BLEND_PIPELINE,
    &SMAA_NEIGHBORHOOD_PIPELINE,
    &PRESENT_TONEMAP_PIPELINE,
    &CULL_PIPELINE,
    &BUILD_INDIRECT_PIPELINE,
    &SHADOW_BUILD_PIPELINE,
    &OCCLUSION_PIPELINE,
    &LOD_SELECT_PIPELINE,
    &CLUSTER_BUILD_PIPELINE,
    &TRANSPARENT_COLLECT_PIPELINE,
    &TRANSPARENT_SORT_PIPELINE,
    &TRANSPARENT_GATHER_PIPELINE,
    &HIZ_DOWNSAMPLE_PIPELINE,
];

const ALL_SETS: &[&SetLayoutDesc] = &[
    &BINDLESS_SET,
    &FRAME_SET,
    &INSTANCE_SET,
    &GBUFFER_SAMPLED_SET,
    &LIGHT_CLUSTER_SET,
    &TRANSPARENT_SET,
    &SHADOW_SET,
    &BLOOM_TEX_SET,
    &BLOOM_UP_SET,
    &SMAA_A_SET,
    &SMAA_B_SET,
    &HIZ_REDUCE_SET,
    &HIZ_OCCLUSION_SET,
];

/// 只给 examples 用、不归宿主管线的 shader。
const PROBE_ONLY: &[&Shader] = &[&shader_abi::DESCRIPTOR_PROBE];

const ALL_STAGES: vk::ShaderStageFlags = vk::ShaderStageFlags::ALL;

/// 静态的 set 列表（数组字面量里取 static 的地址不会被提升成 'static）。
macro_rules! sets {
    ($($set:expr),*) => {{
        const SETS: &[&SetLayoutDesc] = &[$($set),*];
        SETS
    }};
}

/// 拿一条真实配方改一处，返回对账问题。
fn variant(desc: &PipelineDesc, change: impl FnOnce(&mut PipelineDesc)) -> Vec<String> {
    let mut changed = PipelineDesc { ..*desc };
    change(&mut changed);
    changed.problems()
}

fn assert_rejected(problems: &[String], needle: &str) {
    assert!(problems.iter().any(|p| p.contains(needle)), "期望被拒并提到「{needle}」，实际：{problems:#?}");
}

#[test]
fn every_pipeline_matches_shader_abi() {
    for desc in ALL_PIPELINES {
        if let Err(e) = desc.check() {
            panic!("{e}");
        }
    }
}

#[test]
fn every_set_declaration_is_valid_and_uniquely_named() {
    for (i, desc) in ALL_SETS.iter().enumerate() {
        assert!(desc.problems().is_empty(), "{}: {:?}", desc.name, desc.problems());
        assert!(ALL_SETS[..i].iter().all(|d| d.name != desc.name), "{} 重名", desc.name);
    }
    for desc in ALL_PIPELINES {
        for set in desc.sets {
            assert!(ALL_SETS.iter().any(|d| std::ptr::eq(*d, *set)), "{} 用了未登记的 {}", desc.label, set.name);
        }
    }
}

#[test]
fn every_shader_entry_has_a_pipeline() {
    for shader in shader_abi::ALL {
        if PROBE_ONLY.iter().any(|s| std::ptr::eq(*s, *shader)) {
            continue;
        }
        for entry in shader.entries {
            let used = ALL_PIPELINES
                .iter()
                .any(|d| std::ptr::eq(d.shader, *shader) && d.entries.iter().any(|e| e.to_str() == Ok(entry.name)));
            assert!(used, "{} 的入口 {} 没有任何管线配方使用", shader.name, entry.name);
        }
    }
}

#[test]
fn rejects_sets_in_wrong_order() {
    let problems = variant(&GBUFFER_WRITE_PIPELINE, |d| d.sets = sets!(&FRAME_SET, &BINDLESS_SET, &INSTANCE_SET));
    assert_rejected(&problems, "shader 是 SampledImage，frame 声明的是 UniformBuffer");
}

#[test]
fn rejects_missing_set() {
    let problems = variant(&GBUFFER_COMPOSITE_PIPELINE, |d| {
        d.sets = sets!(&FRAME_SET, &GBUFFER_SAMPLED_SET, &LIGHT_CLUSTER_SET);
    });
    assert_rejected(&problems, "@group(3) @binding(0)：管线只声明了 3 个 set");
}

#[test]
fn rejects_missing_binding() {
    static NO_B2: SetLayoutDesc = SetLayoutDesc::new(
        "instance_without_b2",
        &[
            BindingDesc::new(0, DescKind::StorageBuffer, ALL_STAGES),
            BindingDesc::new(1, DescKind::StorageBuffer, ALL_STAGES),
        ],
    );
    let problems = variant(&GBUFFER_WRITE_PIPELINE, |d| d.sets = sets!(&BINDLESS_SET, &FRAME_SET, &NO_B2));
    assert_rejected(&problems, "@group(2) @binding(2)：instance_without_b2 里没有这个 binding");
}

#[test]
fn rejects_wrong_descriptor_kind() {
    static FRAME_AS_STORAGE: SetLayoutDesc =
        SetLayoutDesc::new("frame_as_storage", &[BindingDesc::new(0, DescKind::StorageBuffer, ALL_STAGES)]);
    let problems = variant(&PRESENT_TONEMAP_PIPELINE, |d| {
        d.sets = sets!(&FRAME_AS_STORAGE, &GBUFFER_SAMPLED_SET, &BLOOM_UP_SET);
    });
    assert_rejected(&problems, "shader 是 UniformBuffer，frame_as_storage 声明的是 StorageBuffer");
}

#[test]
fn rejects_missing_stage() {
    static INSTANCE_COMPUTE_ONLY: SetLayoutDesc = SetLayoutDesc::new(
        "instance_compute_only",
        &[
            BindingDesc::new(0, DescKind::StorageBuffer, vk::ShaderStageFlags::COMPUTE),
            BindingDesc::new(1, DescKind::StorageBuffer, vk::ShaderStageFlags::COMPUTE),
            BindingDesc::new(2, DescKind::StorageBuffer, vk::ShaderStageFlags::COMPUTE),
        ],
    );
    let problems = variant(&GBUFFER_WRITE_PIPELINE, |d| {
        d.sets = sets!(&BINDLESS_SET, &FRAME_SET, &INSTANCE_COMPUTE_ONLY);
    });
    assert_rejected(&problems, "vs_main @group(2) @binding(0)：instance_compute_only 的可见阶段");
}

#[test]
fn rejects_runtime_array_without_partially_bound() {
    static BINDLESS_FULLY_BOUND: SetLayoutDesc = SetLayoutDesc::new(
        "bindless_fully_bound",
        &[
            BindingDesc::new(0, DescKind::SampledImage, ALL_STAGES).array(16, vk::DescriptorBindingFlags::empty()),
            BindingDesc::new(1, DescKind::Sampler, ALL_STAGES).array(16, vk::DescriptorBindingFlags::empty()),
        ],
    );
    let problems = variant(&OVERLAY_PIPELINE, |d| d.sets = sets!(&BINDLESS_FULLY_BOUND));
    assert_rejected(&problems, "shader 是 runtime 数组，bindless_fully_bound 缺 PARTIALLY_BOUND");
}

#[test]
fn rejects_array_count_for_single_binding() {
    static FRAME_ARRAY: SetLayoutDesc = SetLayoutDesc::new(
        "frame_array",
        &[BindingDesc::new(0, DescKind::UniformBuffer, ALL_STAGES).array(2, vk::DescriptorBindingFlags::empty())],
    );
    let problems = variant(&PRESENT_TONEMAP_PIPELINE, |d| {
        d.sets = sets!(&FRAME_ARRAY, &GBUFFER_SAMPLED_SET, &BLOOM_UP_SET);
    });
    assert_rejected(&problems, "shader 不是数组，frame_array 声明了 2 个");
}

#[test]
fn rejects_bad_push_range() {
    assert_rejected(&variant(&CULL_PIPELINE, |d| d.push = None), "管线没声明 push 范围");
    let small = PushRange { stages: vk::ShaderStageFlags::COMPUTE, size: 16 };
    assert_rejected(&variant(&CULL_PIPELINE, |d| d.push = Some(small)), "push 范围只有 16B");
    let wrong_stage = PushRange { stages: vk::ShaderStageFlags::FRAGMENT, size: 4 };
    assert_rejected(&variant(&SHADOW_WRITE_PIPELINE, |d| d.push = Some(wrong_stage)), "不含 VERTEX");
}

#[test]
fn rejects_unknown_entry() {
    assert_rejected(&variant(&CULL_PIPELINE, |d| d.entries = &[c"main"]), "cull 里没有入口 main");
}

#[test]
fn rejects_bad_set_declarations() {
    static UAB_WITHOUT_POOL: SetLayoutDesc = SetLayoutDesc::new(
        "uab_without_pool",
        &[BindingDesc::new(0, DescKind::SampledImage, ALL_STAGES).array(4, vk::DescriptorBindingFlags::UPDATE_AFTER_BIND)],
    );
    assert_rejected(&UAB_WITHOUT_POOL.problems(), "布局缺 UPDATE_AFTER_BIND_POOL");
    static UAB_ON_STORAGE: SetLayoutDesc = SetLayoutDesc {
        name: "uab_on_storage",
        bindings: &[BindingDesc::new(0, DescKind::StorageBuffer, ALL_STAGES)
            .array(1, vk::DescriptorBindingFlags::UPDATE_AFTER_BIND)],
        flags: vk::DescriptorSetLayoutCreateFlags::UPDATE_AFTER_BIND_POOL,
    };
    assert_rejected(&UAB_ON_STORAGE.problems(), "设备没启用这一类的 UPDATE_AFTER_BIND");
    static DUPLICATE: SetLayoutDesc = SetLayoutDesc::new(
        "duplicate",
        &[BindingDesc::new(0, DescKind::Sampler, ALL_STAGES), BindingDesc::new(0, DescKind::Sampler, ALL_STAGES)],
    );
    assert_rejected(&DUPLICATE.problems(), "b0 重复声明");
    // 管线对账连带检查它引用的声明
    assert_rejected(
        &variant(&OVERLAY_PIPELINE, |d| d.sets = sets!(&UAB_WITHOUT_POOL)),
        "uab_without_pool: b0 带 UPDATE_AFTER_BIND",
    );
}

#[test]
fn pool_sizes_and_flags_derive_from_declarations() {
    use crate::render::core::binding::set_layout::merged_pool_sizes;

    // storage_sets 的形状判定：dense storage 集 vs bindless（runtime 数组）
    assert_eq!(LIGHT_CLUSTER_SET.dense_storage(), Some(4));
    assert_eq!(TRANSPARENT_SET.dense_storage(), Some(4));
    assert_eq!(BINDLESS_SET.dense_storage(), None);

    // shadow 每份集 = 1 UBO + 4 阴影图 + 5 storage；×2 份
    let sizes = SHADOW_SET.pool_sizes_vec(2);
    let get = |ty| sizes.iter().find(|s| s.ty == ty).map(|s| s.descriptor_count);
    assert_eq!(get(vk::DescriptorType::UNIFORM_BUFFER), Some(2));
    assert_eq!(get(vk::DescriptorType::SAMPLED_IMAGE), Some(8));
    assert_eq!(get(vk::DescriptorType::STORAGE_BUFFER), Some(10));

    // 合并：一份常驻双表（每 binding 满载 TABLE_CAPACITY）+ 2 帧槽 UBO
    let merged = merged_pool_sizes(&[(&BINDLESS_SET, 1), (&FRAME_SET, 2)]);
    let get = |ty| merged.iter().find(|s| s.ty == ty).map(|s| s.descriptor_count);
    assert_eq!(get(vk::DescriptorType::SAMPLED_IMAGE), Some(TABLE_CAPACITY));
    assert_eq!(get(vk::DescriptorType::SAMPLER), Some(TABLE_CAPACITY));
    assert_eq!(get(vk::DescriptorType::UNIFORM_BUFFER), Some(2));

    // UAB pool 旗标从声明推导：bindless 要，frame 不要
    assert!(BINDLESS_SET.needs_uab_pool());
    assert!(!FRAME_SET.needs_uab_pool());
}

#[test]
fn writer_validates_writes_against_declaration() {
    use crate::common::error::VulkanError;
    use crate::render::core::binding::descriptor_writer::DescriptorWriter;
    use crate::render::passes::shadow::shadow_map::SHADOW_SET;

    // 合法写入：shadow 集 b0 UBO + 4 张图 + 5 storage，一次写满
    let set = vk::DescriptorSet::from_raw(7);
    let mut w = DescriptorWriter::new(&SHADOW_SET, set);
    w.buffer(0, vk::Buffer::from_raw(1), 0, 64);
    for c in 0..4u32 {
        w.image(1 + c, vk::ImageView::from_raw(10 + c as u64), vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        w.buffer(6 + c, vk::Buffer::from_raw(20 + c as u64), 0, 128);
    }
    w.buffer(5, vk::Buffer::from_raw(30), 0, 128);
    assert!(w.check().is_ok());

    // 种类错：往 UBO 槽写图
    let mut w = DescriptorWriter::new(&SHADOW_SET, set);
    w.image(SHADOW_BINDING_UNIFORM, vk::ImageView::from_raw(1), vk::ImageLayout::GENERAL);
    assert!(matches!(w.check(), Err(VulkanError::Init(_))));

    // binding 不存在
    let mut w = DescriptorWriter::new(&SHADOW_SET, set);
    w.buffer(99, vk::Buffer::from_raw(1), 0, 4);
    assert!(w.check().is_err());

    // 完备性：b1..b4 没写
    let mut w = DescriptorWriter::new(&SHADOW_SET, set);
    w.buffer(0, vk::Buffer::from_raw(1), 0, 4);
    let e = w.check().unwrap_err().to_string();
    assert!(e.contains("b1：声明要求写满 1 个，还缺 1"), "{e}");

    // PARTIALLY_BOUND 不要求写满：bindless 只按槽发布
    let mut w = DescriptorWriter::new(&BINDLESS_SET, set);
    w.image_at(0, 5, vk::ImageView::from_raw(1), vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    w.sampler_at(1, 5, vk::Sampler::from_raw(2));
    assert!(w.check().is_ok());

    // 数组元素越界
    let mut w = DescriptorWriter::new(&BINDLESS_SET, set);
    w.image_at(0, TABLE_CAPACITY, vk::ImageView::from_raw(1), vk::ImageLayout::GENERAL);
    assert!(w.check().is_err());
}

#[test]
fn registry_resolves_by_declaration_identity() {
    static IMPOSTOR: SetLayoutDesc =
        SetLayoutDesc::new("frame", &[BindingDesc::new(0, DescKind::UniformBuffer, ALL_STAGES)]);
    let fake = vk::DescriptorSetLayout::from_raw(1);
    let layouts = SetLayouts::new([SetLayout::new(&FRAME_SET, fake)]).expect("单条登记");
    assert_eq!(layouts.get(&FRAME_SET).ok(), Some(fake));
    assert!(layouts.get(&IMPOSTOR).is_err(), "同名不同声明不能顶替");
    assert!(layouts.get(&BINDLESS_SET).is_err(), "未登记的声明要报错");
    assert!(SetLayouts::new([SetLayout::new(&FRAME_SET, fake), SetLayout::new(&FRAME_SET, fake)]).is_err());
    assert!(SetLayouts::new([SetLayout::new(&FRAME_SET, vk::DescriptorSetLayout::null())]).is_err());
}
