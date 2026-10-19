# 虚拟纹理调研：页表、feedback 与流送

## 0. 判定线

- **虚拟纹理（VT）解决的问题只有一个：贴图总数据量远超显存**。4096/8192 显存墙、地形与大地貌的超大贴图、多材质组合的去重——都是这一个问题的不同表现。
- **VT 的代价是两条链**：shader 端两次采样（先查页表再采物理页）；CPU 端一条"GPU 记录实际用页 → 异步回读 → 流送线程换页"的反馈回路。**第二条链是读回，与 GPU-driven 理念正面冲突**（库里《为什么引擎没有都上 GPU 驱动》的回读墙，在 VT 上原样存在）。
- **sparse image（Vulkan sparse residency）不是 VT**：它只解决"地址空间可以稀疏映射到物理内存"（尾 mip、空洞不占显存），没有页表寻址、没有按需流送的策略层——把它当 VT 会失望。
- **Unity SVT 到今天仍是 experimental**，UE 的虚拟纹理（RVT/LWC）成熟度领先；自研渲染器上 VT 是"大世界数据量到了才值回票价"的技术，先储备不清算。
- **VT 与 bindless 同构**：都是"巨大虚拟空间 + 查询时寻址"，页表可以直接进 bindless 采样数组——我们渲染器的现有形态接 VT 是顺的，不是对抗的。
- **登记状态（2026-10-19）**：不立项，已登记[外置拓展条目 6](../../外置拓展需求.md)——与 6.3/6.4 流送段的分工、恢复条件与实施钉点见该条目；本文保留为知识正本。同日 6.4 细粒度流送已**转正立案**（[段施工计划](../../6-高级剔除LOD与流送/6.4-细粒度流送/施工计划：细粒度流送拆解.md)——整级 mip / LOD 档粒度，非 VT 路线），VT 本体仍不立项，恢复条件见条目 6 状态追记。

## 1. 核心机制：两级映射

经典 VT（Megatexture 谱系，Unity SVT 同构）：

```
虚拟贴图：逻辑上 16K×16K 的巨大贴图，按 tile（如 128×128）分页
     │
     ▼ 第一次采样
页表（indirection texture）：每 texel 对应一个虚拟 tile，
     存 [物理页池里的页号 | 有效位 | mip 层级]
     │  查到页号 + 虚拟 UV 的 tile 内偏移
     ▼ 第二次采样
物理页池（cache texture）：固定大小的纹理数组/图集，放着热页
```

- **为什么能突破显存上限**：显存只放"实际被看到的页"（屏幕覆盖的 tile 集合通常远小于全图），页池大小由屏幕分辨率和 tile 尺寸决定，与源资产尺寸无关。一份 128K² 的地形纹理，驻留的可能只有几十 MB。
- **mip 与页表层级**：每个 mip 层的 tile 数量减半，页表本身也按层级组织——采样低 mip（远处）查的是更小的页表。缺页时沿 mip 向上找常驻父页兜底。
- **UV 约束**：tile 边界处三线性/各向异性过滤会跨页采到脏边，工业解法 = 物理 tile 内 padding（复制边缘 texel）或页内 mip 塞进 tile 的右下角（idTech 的 tile 内 mip 方案）。

## 2. feedback 回路（经典路线的心脏）

1. 渲染时额外绑一个**反馈渲染目标**：像素着色把实际采到的 tile ID（+ mip 层）写进去（通常一像素一个 ID，低分辨率即可）。
2. **CPU 异步读回**该目标（独立线程，与渲染解耦），合并出"本帧实际用到的页集合"。
3. 流送线程据此发出磁盘 IO → 解压 → 拷进物理页池 → 更新页表。
4. **未就绪兜底**：请求的页还没到位时，采样自动回退到更高 mip（更粗的常驻页），画面先糊后清。Unity SVT 明文如此（"takes from milliseconds to seconds until the requested tiles load"）。

**结构性问题**：这是"渲染了才知道要什么"的滞后一帧（甚至更久）逻辑，首次出现的新区域必然先糊后清。Unity SVT 文档原话：tile 加载毫秒到秒级，极端情况可能永远不加载。这与库里已有的"回读墙"结论同源：**feedback 一旦断流（回读延迟/线程卡顿），画面质量立即降级**。

## 3. GPU 常驻路线（绕开读回）

GPU-driven 时代的变体：

- **永驻 mip 链**：高 mip（粗粒度全图）常驻显存，低 mip（细节页）才走流送；缺页时降级的画面至少是"完整低清"而不是黑洞。页池分配器与页表更新也尽量在 GPU 侧（compute 维护 free list，与我们的 MeshPool free list 同思想）。
- **shader 内降级**：采样前按页表有效位直接选 mip，不等 CPU 回读修正——把"缺页兜底"从 CPU 策略搬进 GPU 查询路径。
- **页表也常驻 + 预测式流送**：CPU 只按相机运动预测加载（地形场景够用），feedback 只做统计不作硬依赖。这是"回读墙"与"零读回"之间的工程折衷。

## 4. sparse image：Vulkan 原生方案到底给了什么

- **sparse residency**（`sparseResidency*` 特性族）：图像可以映射到 64KB 粒度的物理内存块上，未绑定块不占显存；尾 mip（小于页粒度的部分）用 mipTail 特殊布局处理。
- **它解决的**：一张巨大逻辑纹理的"物理内存按需分配"——相机看不到的部分、没生成的 mip 不占显存；绑定/解绑由驱动管理。
- **它没解决的**：没有页表寻址（采样还是一张完整纹理，UV 就是 UV）；没有"哪些页该驻留"的策略（驱动只管已绑定块）；换页是显式的 `vkQueueBindSparse` 操作，策略层（哪页该进该出）仍要自己写；无反馈机制。
- **选型口径**：要"省尾 mip/省空洞"用 sparse；要"超大数据按视口流送"才是 VT。两者可组合：物理页池本身用 sparse 图像承载。

## 5. Unity SVT 现状

来源：[How Streaming Virtual Texturing works - Unity Manual](https://docs.unity3d.com/6000.0/Documentation/Manual/svt-how-it-works.html)（Unity 6.0 文档）

- **状态**：官方明示 experimental，"not ready for production use"。
- **机制**（与第 1、2 节同构，官方文档确认）：采样 = 查 indirection texture → 算非虚拟 UV → 采 cache texture；tile ID 通过**额外绑定的渲染目标**收集，CPU 异步拷回主存、独立线程处理请求，由异步读管理器从磁盘换页；缺页自动回退高 mip。
- **Texture Stack**：同时以同一 UV 采样的多张贴图（albedo/normal/roughness…）打包进一个 stack 一起换页——**把"页"从单贴图单位提升到材质单位**，避免各通道独立流送导致的不对齐；代价是要求同 stack 内尺寸布局一致。
- **调试**：HDRP 全屏调试模式 RequestedVirtualTextureTiles，按 tile ID 着色、色相表示 mip 层。
- **限制**：部分管线/平台支持不全（与某些特性不共存）；与传统 texture streaming（mip streaming）是两套系统。

## 6. UE 对照

- **RVT（Runtime Virtual Texture）**：页由 **GPU 在运行时生成**（地形材质逐页烘焙进页池），不落盘——"虚拟地址空间的页是现场画的"，配合 Virtual Texture Material 消 terrain 大量 draw 与过度绘制；与 SVT"磁盘流送"是互补的两种页来源。
- **LWC（Large World Coordinates）+ 虚拟纹理**：UE5 大世界地形的标配组合；世界分区流送与 VT 页流送叠加。
- 启示：**页来源可以多元化**（磁盘 / GPU 生成 / 程序化），页表与查询机制不变。

## 7. 与 bindless 的两种接法

1. **页表走 bindless**：物理页池 = 常驻纹理数组（我们已有的 bindless 采样表天然能装），shader 两次采样都是普通 `texture2d` 查询；页表本身是另一张常驻纹理。**这是自研 bindless 渲染器的自然接法**，无新绑定模型。
2. **sparse image 单句柄**：整张超大 sparse 纹理一个 descriptor，绑定层由驱动管；shader 端零改动（单次采样），但流送策略受限于 sparse 绑定粒度与 `vkQueueBindSparse` 的节奏。

对比：接法 1 灵活可控（页池大小/替换策略全在自己手里，且天然 GPU-driven），接法 2 省实现但策略权在驱动。**按我们"结构自主"的项目原则，接法 1 是主路线。**

## 8. 落地评估

**什么时候才值得上**（回本条件）：

- 贴图资产总量超出"全量 DEVICE_LOCAL 驻留"预算（数 GB 级源数据），或
- 出现真正的大地貌（地形 16K+ 贴图、超大全景城市），5.4/5.5 的程序化大场景若贴图组合爆炸就会到这一步；
- 现阶段（头盔场景 + 程序化实例铺量）**不需要**——我们缺的是实例数量不是贴图数量。

**最小可用件清单**（若立项，依赖顺序）：

1. 物理页池 + 分配器（纹理数组，free list——可复用 MeshPool 思想）
2. 页表纹理（mip 分级，update 走 staging 上传——三环家族既有通道）
3. shader 两次采样封装 + tile 内 mip/padding 布局
4. feedback 目标 + 异步回读线程（唯一的新"环"）
5. 流送线程 + 磁盘页格式 + 高 mip 兜底
6. （可选）tile 内 mip 方案、压缩格式（BC/ASTC 页级解码）

工程量判断：1~3 是纹理管理工程，风险低；4~5 引入异步 IO 与回读线程，是真正的复杂度来源；**建议与"回读墙"文档联读后再拍板**。

## 来源清单

- [How Streaming Virtual Texturing works - Unity Manual（6000.0）](https://docs.unity3d.com/6000.0/Documentation/Manual/svt-how-it-works.html)
- [Streaming Virtual Texturing 总览 - Unity Manual](https://docs.unity3d.com/6000.0/Documentation/Manual/svt-streaming-virtual-texturing.html)
- Vulkan 规范：sparse residency / `vkQueueBindSparse`（API 细节以 VUID 为准，本文只做机制定位）
- UE Virtual Texturing / RVT：Epic 官方文档（对照参考，未逐条核对）
- 库内互链：《为什么引擎没有都上 GPU 驱动：CPU 镜像、回读墙与收益边界》（引擎分类姊妹篇）——feedback 回读墙的通论
