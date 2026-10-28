//! 描述符写入器：每次写入都按 [`SetLayoutDesc`] 声明校验（binding 存在、描述符种类
//! 匹配、数组元素在界内），`submit` 前再按声明查完备性——非 PARTIALLY_BOUND 的
//! binding 必须写满全部元素。错误发生在 `vkUpdateDescriptorSets` 之前，报的是
//! 声明名 + binding 号，不是 VUID。
//!
//! 完备性口径：布局是各管线共享的超集，所以完备性按“整份集”查，与某条管线用不用
//! 无关；带 PARTIALLY_BOUND 的 binding（bindless 表按槽发布）不做写满要求，但每次
//! 写入仍过种类与元素界校验。

use ash::{vk, Device};

use crate::common::error::{Result, VulkanError};


use super::set_layout::{DescKind, SetLayoutDesc};

/// 一条待提交的写入（句柄值拷贝，不借用外部数据；提交时再组装 WriteDescriptorSet）。
#[derive(Clone, Copy)]
enum Write {
    Buffer { binding: u32, element: u32, info: vk::DescriptorBufferInfo },
    Image { binding: u32, element: u32, info: vk::DescriptorImageInfo },
}

/// 按声明校验的描述符写入器。一次收集、一次提交；`check` 不通过整批拒写。
pub(crate) struct DescriptorWriter {
    desc: &'static SetLayoutDesc,
    set: vk::DescriptorSet,
    writes: Vec<Write>,
}

impl DescriptorWriter {
    pub(crate) fn new(desc: &'static SetLayoutDesc, set: vk::DescriptorSet) -> Self {
        Self { desc, set, writes: Vec::new() }
    }

    /// 写单个 buffer（整段）。种类须是 uniform/storage buffer。
    pub(crate) fn buffer(
        &mut self,
        binding: u32,
        buffer: vk::Buffer,
        offset: u64,
        range: u64,
    ) -> &mut Self {
        self.buffer_at(binding, 0, buffer, offset, range)
    }

    /// 写 buffer 数组的单个元素（bindless 按槽发布）。
    pub fn buffer_at(&mut self, binding: u32, element: u32, buffer: vk::Buffer, offset: u64, range: u64) -> &mut Self {
        self.writes.push(Write::Buffer {
            binding,
            element,
            info: vk::DescriptorBufferInfo::default().buffer(buffer).offset(offset).range(range),
        });
        self
    }

    /// 采样一张图（sampled/storage image；种类按声明核对）。
    pub fn image(&mut self, binding: u32, view: vk::ImageView, layout: vk::ImageLayout) -> &mut Self {
        self.image_at(binding, 0, view, layout)
    }

    pub(crate) fn image_at(&mut self, binding: u32, element: u32, view: vk::ImageView, layout: vk::ImageLayout) -> &mut Self {
        self.writes.push(Write::Image {
            binding,
            element,
            info: vk::DescriptorImageInfo::default().image_view(view).image_layout(layout),
        });
        self
    }

    /// 写一个采样器（独立 binding 或数组元素）。
    pub fn sampler(&mut self, binding: u32, sampler: vk::Sampler) -> &mut Self {
        self.sampler_at(binding, 0, sampler)
    }

    pub(crate) fn sampler_at(&mut self, binding: u32, element: u32, sampler: vk::Sampler) -> &mut Self {
        self.writes.push(Write::Image {
            binding,
            element,
            info: vk::DescriptorImageInfo::default().sampler(sampler),
        });
        self
    }

    /// 校验每个写入：binding 存在、描述符种类匹配写入形状、元素在声明数量内。
    /// 返回全部问题，空 = 通过。
    fn write_problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for write in &self.writes {
            let (binding, element, shape) = match *write {
                Write::Buffer { binding, element, .. } => (binding, element, "buffer"),
                Write::Image { binding, element, info } => {
                    let shape = if info.sampler != vk::Sampler::null() { "sampler" } else { "image" };
                    (binding, element, shape)
                }
            };
            let Some(b) = self.desc.binding(binding) else {
                out.push(format!("b{binding}：{} 里没有这个 binding", self.desc.name));
                continue;
            };
            let kind_ok = matches!(
                (shape, b.kind),
                ("buffer", DescKind::UniformBuffer)
                    | ("buffer", DescKind::StorageBuffer)
                    | ("image", DescKind::SampledImage)
                    | ("image", DescKind::StorageImage)
                    | ("sampler", DescKind::Sampler)
            );
            if !kind_ok {
                out.push(format!("b{binding}：声明是 {:?}，不能按 {shape} 写", b.kind));
            }
            if element >= b.count {
                out.push(format!("b{binding}：元素 {element} 超出声明的 {} 个", b.count));
            }
        }
        out
    }

    /// 完备性：非 PARTIALLY_BOUND 的 binding 必须写满全部元素（声明是共享超集，
    /// 集是整份绑定的，缺一个就是绑定时读未定义描述符）。
    fn coverage_problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for b in self.desc.bindings {
            if b.flags.contains(vk::DescriptorBindingFlags::PARTIALLY_BOUND) {
                continue;
            }
            let mut covered = vec![false; b.count as usize];
            for w in &self.writes {
                let (wb, we) = match *w {
                    Write::Buffer { binding, element, .. } => (binding, element),
                    Write::Image { binding, element, .. } => (binding, element),
                };
                if wb == b.binding && we < b.count {
                    covered[we as usize] = true;
                }
            }
            let missing = (0..b.count as usize).filter(|&i| !covered[i]).count();
            if missing > 0 {
                out.push(format!("b{}：声明要求写满 {} 个，还缺 {missing}", b.binding, b.count));
            }
        }
        out
    }

    /// 只校验不提交（单测与诊断用；库内写入走 `submit`）。
    ///
    /// # Errors
    /// 任一写入不合法，或完备性不满足。
    #[cfg_attr(not(test), expect(dead_code, reason = "生产路径走 submit，check 只被契约测试调用"))]
    pub(crate) fn check(&self) -> Result<()> {
        let mut problems = self.write_problems();
        problems.extend(self.coverage_problems());
        if problems.is_empty() {
            Ok(())
        } else {
            Err(VulkanError::Init(format!(
                "声明 {} 的描述符写入对不上：{}",
                self.desc.name,
                problems.join("；")
            )))
        }
    }

    /// 校验（写入项 + 完备性）后整批提交。
    ///
    /// # Errors
    /// 任一写入不合法，或完备性不满足。
    pub(crate) fn submit(self, device: &Device) -> Result<()> {
        let mut problems = self.write_problems();
        problems.extend(self.coverage_problems());
        if !problems.is_empty() {
            return Err(VulkanError::Init(format!(
                "声明 {} 的描述符写入对不上：{}",
                self.desc.name,
                problems.join("；")
            )));
        }
        // 描述符类型以声明为准（种类匹配已在上面核对），不由写入形状猜
        let kinds: Vec<vk::DescriptorType> = self
            .writes
            .iter()
            .map(|w| {
                let binding = match *w {
                    Write::Buffer { binding, .. } => binding,
                    Write::Image { binding, .. } => binding,
                };
                self.desc.binding(binding).expect("write_problems 已核对过存在性").kind.vk()
            })
            .collect();
        let mut kind_i = kinds.iter();
        let buffer_infos: Vec<vk::DescriptorBufferInfo> =
            self.writes.iter().filter_map(|w| match *w {
                Write::Buffer { info, .. } => Some(info),
                _ => None,
            }).collect();
        let image_infos: Vec<vk::DescriptorImageInfo> =
            self.writes.iter().filter_map(|w| match *w {
                Write::Image { info, .. } => Some(info),
                _ => None,
            }).collect();
        let mut infos_b = buffer_infos.iter();
        let mut infos_i = image_infos.iter();
        let writes: Vec<vk::WriteDescriptorSet> = self
            .writes
            .iter()
            .map(|w| {
                let ty = *kind_i.next().unwrap();
                match *w {
                    Write::Buffer { binding, element, .. } => vk::WriteDescriptorSet::default()
                        .dst_set(self.set)
                        .dst_binding(binding)
                        .dst_array_element(element)
                        .descriptor_type(ty)
                        .buffer_info(std::slice::from_ref(infos_b.next().unwrap())),
                    Write::Image { binding, element, .. } => vk::WriteDescriptorSet::default()
                        .dst_set(self.set)
                        .dst_binding(binding)
                        .dst_array_element(element)
                        .descriptor_type(ty)
                        .image_info(std::slice::from_ref(infos_i.next().unwrap())),
                }
            })
            .collect();
        unsafe { device.update_descriptor_sets(&writes, &[]) };
        Ok(())
    }
}
