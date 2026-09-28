# 显存机制：Buffer与Image之别、swizzle不透明与访问路径特化

> 2026-09-24。3.3 贴图链路开工前的讨论沉淀，回答四个问题：Image 底层是不是 Buffer？同一块显存能同时装两者吗？为什么硬件不给 Image 特化存储？"swizzle 不透明"是什么意思？结论直接约束 3.3 的实现选型。

## 0. 判定线

**显存的存储层始终是类型无关的 1D 字节序列。Buffer 与 Image 的全部差异不在存储，而在访问路径与绑定时的解释参数；图像的"多维"由 swizzle 函数在解释层注入——虚拟层与物理层都只有 1D。**

由此推出三条实现约束，3.3 直接引用：

1. **OPTIMAL tiling image 的内容不能按字节读写**：上传走 `vkCmdCopyBufferToImage`，读回走 `vkCmdCopyImageToBuffer`，重排由驱动侧机器完成，CPU 不参与。
2. **image 分配不复用 MeshPool 的 buffer 池**：image 内容与 mesh 数据无可共享的按字节读取语义，混池的唯一收益是 aliasing 省显存——那是 rendergraph 级手段，上传管线不需要。
3. **image 的 layout 转换与 buffer 的读写次序是两类问题**：前者在切换"访问路径该用哪套解释参数"，不能拿 3.2 buffer barrier 的模板套（3.3 README 已有此提醒，本篇给出机制解释）。

## 1. Buffer 与 Image：平级资源，一块 typeless 地基

两者是平级的一等资源，谁也不是谁的底层。唯一共同地基是 `VkDeviceMemory`：`vkGet*MemoryRequirements` → `allocate_memory` → `vkBind*Memory(2)`。绑定之后分叉：

| | VkBuffer | VkImage |
|---|---|---|
| 本质 | 线性字节数组 | 带格式与排布的 texel 网格 |
| 布局 | 无（平坦可寻址） | tiling（LINEAR/OPTIMAL）+ 运行时 layout |
| 字节排布 | 按偏移即得 | OPTIMAL 下驱动私有，不可按字节寻址 |
| 硬件描述符 | 基址 + 大小（+ view 格式） | 基址 + tiling 模式 + 格式 + 尺寸 + mip 布局 |

关键差异是 **layout 维度**：buffer 天生"平坦可寻址"，而 image 的字节排布与"当前可被谁访问"（layout）是分离的两个概念。所以 image barrier 比 buffer barrier 多出 `oldLayout/newLayout` 与 `subresourceRange`（aspect、mip、layer）。

内存本身不带类型，两个推论：

- **别名（aliasing）合法**：多个资源可重叠绑定同一 `VkDeviceMemory` 区间，buffer↔image 亦可；限制是别名资源不得同时访问（须 barrier 隔开）。
- **不存在 buffer↔image 的 cast**：buffer 可取 device address（`vkGetBufferDeviceAddress`），image 永远不能；两者内存不能互换 reinterpret。

## 2. swizzle：多维如何折叠进 1D

物理访存粒度是 32B/64B 的 sector，1D 地址。在此基础上：

- **LINEAR tiling**：行主序，`rowPitch/depthPitch/arrayPitch` 全部公开（`vkGetImageSubresourceLayout` 可查），CPU mmap 后就是普通 1D 数组——多维数据的平凡 1D 编码。
- **OPTIMAL tiling**：`offset = swizzle(x, y, mip, layer)`，驱动私有。主流做法是小块内 Morton 序（Z 序）+ 分层瓦片：NVIDIA 用 GOB（2D 约 64×8 texel 一组），AMD 用 8×8 micro-tile 再拼宏 tile。

Morton 的具象例子（4×4 图像）：把 x、y 的二进制位逐位交织成偏移。行主序里相邻 texel `(1,0)` 与 `(0,1)` 下标是 1 和 4（隔一整行）；Morton 里是 1 和 2（连续）。双线性要的 2×2 足迹因此落在 1～2 个 sector 里。位交织在硬件上是**纯接线**（x、y 各 bit 直接路由到地址线不同位），零计算、零额外延迟。

tile 层之上还有无损压缩（AMD DCC、NVIDIA compressible memory，`VK_EXT_image_compression_control` 可查询），metadata 单独存放。这就是 image 的 `memoryRequirements.size` 常大于 `W×H×bpp` 的原因：padding、tile 对齐、metadata 都算在内，且 mip 链、array layer 的内存先后顺序也是驱动私有的。

## 3. "不透明"：spec 刻意关上的门

**不透明 = 字节↔texel 的映射只存在于 TMU 与 copy 引擎的硬件参数里，应用拿不到、算不出、不得依赖。** API 层面的证据：

- `vkGetImageSubresourceLayout` 只对 LINEAR（或显式 DRM modifier）有意义，OPTIMAL 下返回值不可依赖；
- `vkMapMemory` 映射 OPTIMAL image 内存后，CPU 写入结果未定义——写进去的字节不会被解释成 texel；
- 不存在"按坐标写一个 texel"的命令，只有经驱动翻译的通道：copy/blit 命令、attachment 写入、storage image 访问；
- 驱动还会按 usage 挑 tiling 模式（如 scanout 用途选显示控制器兼容排布），不通知应用。

为什么关这扇门：swizzle 公式一旦进 API 契约，每代 GPU 的排布调优就被冻死，且 A 家写的数据到 B 家卡上全错。交易是：**应用放弃字节级解释权，换驱动逐图选最优排布 + 同一份代码跨厂商正确**。应用拿到的保证全部在"边界操作"上——copy、blit、采样、layout 转换在所有实现上行为一致。

## 4. 硬件特化在哪：访问路径，不在存储

存储层统一（§0 判定线），特化发生在三条访问路径：

```
                 ┌─ load/store 单元（SSBO/顶点取数/原子）─ 数据 cache ─┐
VA 空间 ─ 描述符 ─┤                                                   ├─ L2 ─ 显存控制器
                 └─ TMU/ROP（swizzle、格式、滤波、压缩）─ 纹理 cache ─┘
                 （另：copy 引擎 ── 两条路都会走的搬运工）
```

1. **地址生成**：buffer 地址 = 基址 + 偏移（一个加法器）；image 的 TMU 内置 swizzle 地址生成器，参数来自 image view 描述符（tiling 模式、格式、尺寸、mip 布局）。
2. **处理单元**：buffer 走通用 load/store（字节粒度、scatter/gather、原子）；image 走 TMU/ROP：格式转换、sRGB、双线性滤波、BCn 就地解压、渲染目标写后压缩——DCC 这类无损压缩只搭 image 路径，metadata 由 ROP 写、TMU 读。纹理 cache 与数据 cache 端口分开。
3. **copy 引擎**：同时懂 linear 与各 tiling 模式，`vkCmdCopyBufferToImage` 能在拷贝同时完成重排；可独占传输队列（[3.2.1 的 transfer 队列族](../3.2-buffer侧上传/3.2.1-Context扩展：transfer队列族与timeline信号量开关.md)对应的就是它）。

存储不特化的三个理由：

1. **容量经济学**：纹理以 GB 计，任何专用快速存储（SRAM）只有 MB 级，纹理主体注定住共享 DRAM；GDDR/HBM 是采购的标准颗粒（接口即 1D 突发），厂商不造"2D 寻址的内存芯片"。
2. **swizzle 到 DRAM 眼里已全是线性**：TMU 聚合后，2×2 足迹已是 1～2 个 sector 的线性突发，"2D 寻址内存"省零字节；把 swizzle 下沉进内存控制器反而要每次请求查询资源元数据——往最慢的环节（访存延迟）加一跳。
3. **分区 = 僵化**：负载逐帧漂移（compute 帧全 buffer，材质切换帧全纹理），DRAM 分区后总有一侧闲置。typeless 共享是页迁移、维护迁移（[3.2.2.3](../3.2-buffer侧上传/3.2.2.3-容量与销毁：显式维护迁移与停顿分账.md) 的扩容路径）、别名、sparse 全部机制的前提——任意物理页可服务任意资源。

片上特化**真实存在**，形态是"小而快的 SRAM 附着在路径上"：移动端 TBDR 的 tile memory（整帧 render target 常驻片上，MSAA 近乎免费）、纹理 L1 cache、DCC metadata。桌面 GPU 没有带宽压力到那一步，用压缩代替专用存储。

## 5. 三层模型：1D 在哪层，多维在哪层

```
虚拟层：VA，1D 线性编号（页表缝合物理不连续）
物理层：拓扑多维（通道/bank/行/列，HBM 是 3D 堆叠），但数据是 1D 字节
解释层：swizzle(x, y, mip, layer) → 偏移  ← 多维性只住在这里
```

§1 的别名实验是这条判定线的实验依据：**同一块物理内存**，挂 buffer 描述符读是 1D 数组，挂 image 描述符读是 swizzled 网格——内存一个字节没变，"多维"出现了，说明多维不是存储的属性，是访问方式的属性。

历史佐证：PS1/GBA 时代没有 VA 抽象、没有 TMU 地址生成器，tile/swizzle 是**写进公开文档的内存布局**，CPU 必须手算交织地址。现代 GPU 的演变方向是把多维性从公共契约降级为私有实现：1D VA 给软件，swizzle 藏进 TMU，中间用 copy 命令翻译。

## 6. 落回 ash_renderer

- **池建在 buffer 层，不在 memory 层**：[MeshPool](../../../../ash_renderer/src/vulkan/pool.rs) 是两条 DEVICE_LOCAL 大 buffer（顶点/索引）+ bump 偏移 + `PoolRange`，解决的是 draw 侧问题（bind 引脚 `vertex_base`/`first_index`、合批拷贝、整段迁移）。[GpuBuffer](../../../../ash_renderer/src/vulkan/resources.rs) 每条独占一块 `VkDeviceMemory`——buffer 数量少时无所谓；image 进场后逐张 `allocate_memory` 会撞 `maxMemoryAllocationCount`（常见 4096）与分配开销，届时才引入 memory 层子分配（一块大内存按偏移绑多资源，对齐按各自 requirements）。
- **3.3 贴图上传的形态**：复用 3.2 的 stage/票据骨架 → `vkCmdCopyBufferToImage`（重排由 copy 引擎完成，应用永不参与 swizzle）→ copy 前后 image barrier 对（`UNDEFINED → TRANSFER_DST_OPTIMAL → SHADER_READ_ONLY_OPTIMAL`）。
- **读回校验**走 `vkCmdCopyImageToBuffer` 回线性 buffer；**LINEAR + host-visible image** 只用于调试/读回（usage 受限、离散卡上功能不全），不做上传主路径。
- **RenderDoc 能看"解码后"纹理**是走调试器与驱动的互操作、由驱动替它解 swizzle——不是布局公开了。
