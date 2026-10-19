# 渲染管线模式与 VisBuffer：GDR 的着色层搭档

> 2026-10-15 建档。出处：5.2 开工前三轮问答沉淀——"GPU-driven 一般用什么渲染管线模式"、"vis-buffer 是什么"、"已有 G-buffer 还需不需要 vis-buffer / G-buffer 是不是 vis-buffer 生成的"。与[《为什么引擎没有都上 GPU 驱动》](为什么引擎没有都上 GPU 驱动：CPU 镜像、回读墙与收益边界.md)分工：那篇讲**决策与提交层**（谁画、画哪些），本篇讲**着色层**（光照信息从哪取、像素中间产物存什么）——两篇合起来才是"GDR 在渲染管线全景里的位置"。上游参考见《GPU-Driven Rendering》与《延迟渲染》（`C:\Users\zengkaixiang\.agents\docs\游戏制作\游戏引擎\渲染\渲染管线\`，绝对路径引用）。着色架构升级本身已登记[外置拓展条目 5](../../外置拓展需求.md)。

## 0. 结论先行

1. **GPU-driven 与 forward/deferred 正交**：GDR 管"决策与提交"层（画哪些、怎么发、参数从哪来，关心**对象数**）；forward/deferred 管"着色数据组织"层（光照信息从哪取，关心**光源数与材质带宽**）。两个独立决策，组合谱系两种都有。
2. **vis-buffer 不是加在管线上的另一个 buffer，是 G-buffer 同一岗位的另一种格式**——存"取货单"（三角形 ID+重心坐标）而不存"属性结果"（albedo/法线/roughness）。二选一，不并存；"G-buffer 由 vis-buffer 生成"只是 UE5 融合形态的特例。

## 1. 组合谱系：两种管线模式都在用 GDR

| 引擎/案例 | 着色管线 | GDR 形态 |
|---|---|---|
| UE5 Nanite | deferred | indirect + cluster 微三角形 + vis-buffer |
| Frostbite（GDC 2016 奠基分享） | deferred | compute 剔除 → indirect |
| Ubisoft（SIGGRAPH 2015） | deferred lighting | GPU 剔除发 indirect |
| Unity 6 GPU Resident Drawer | URP=forward+ / HDRP=deferred 都支持 | 实例数据常驻 + GPU 剔除，opt-in |
| DOOM 系列（id Tech） | forward（clustered） | mesh draw command + GPU 剔除 |
| COD 系 | forward+（clustered） | GPU 剔除、indirect |

大世界 3A 多落 deferred 一侧（历史惯性+光源多），但这不是 GDR 的要求——id Tech 和 COD 证明 forward+ 照样跑全套 GPU 剔除和 indirect。**管线模式的选择由光源数、材质复杂度、MSAA、透明占比决定，与对象数无关；GDR 的选择由对象数决定。两个决策独立做。**

## 2. 与 GDR 真正绑定的两件事（都不是 forward/deferred 之分）

- **不透明主通道**：能写深度的几何才有资格进 GDR indirect 通道；半透明永远另走独立的 forward pass。所以任何 GDR 引擎实际都是"混合形态"——不透明 GPU-driven、半透明传统 forward。这一点与选 deferred 还是 forward 无关，deferred 引擎的半透明同样是独立 forward pass（借不透明留下的深度），机制同构。
- **vis-buffer / deferred material**：最极致的 GDR 引擎（Nanite、id Tech、Frostbite）都长成这个形状——不是"GDR 必须用它"，是 GDR 的深化反过来倒逼着色层进化（§3 三条理由）。

## 3. VisBuffer 机制：存取货单，不存结果

**与 G-buffer 的对比：**

| | G-buffer（传统 deferred） | Vis-buffer |
|---|---|---|
| 每像素存什么 | albedo、法线、roughness、metallic、运动向量…… | **三角形 ID（+instance/cluster 号）+ 重心坐标** |
| 大约字节数 | 40+ B/px（MRT 好几张 RT） | ~8 B/px（一张 RT） |
| 属性从哪来 | 光栅化时算好、按 RT 格式量化写死 | resolve 阶段拿 ID 回查顶点数据**现场插值** |
| 法线/UV 精度 | 受 RT 格式量化损失 | 从源数据精确重算，无损 |

**管线四步**：①光栅化全部几何，每像素只写"被哪个三角形盖住+重心坐标"（ID 是按 draw 顺序生成的连续编号，微小）；②全屏 resolve：逐像素拿 ID → 回查索引/顶点缓冲 → 重心坐标现场插值法线/UV/切线 → 拿材质 ID 走 **bindless** 读材质参数；③在这份"现在才凑齐"的材质上做光照；④输出颜色。

**为什么它和 GDR 天生一对**，三条环环相扣：

1. **微三角形根本没法存属性**：Nanite 一帧光栅化几十亿个三角形，cluster/instance 粒度大到不可能给每个像素提前插值属性——唯一可行的每像素载荷就是 ID。先有"GPU 自主画一大堆微几何"的需求，vis-buffer 是唯一配得上的像素格式。
2. **bindless 是同一刀**：像素持 ID 回表取材质参数 = 5.1"push 96B → 递 4B 实例槽号"在**像素层**的原样重放——CPU 递索引不递数据，像素也递索引不递数据。对象表/材质表往 set2 一放，resolve 天然就是这套基建的又一个消费者。
3. **带宽账**：4K 下 G-buffer 40+ B/px 是数十 GB/s 级写读流量，vis-buffer 一张 8B RT 砍一个数量级——id Tech 与 Frostbite 在 Nanite 之前采用它的原因（Frostbite 的叫法是"deferred material"）。

## 4. 岗位观：G-buffer 和 vis-buffer 是同一岗位的两种格式

管线上有个固定岗位："几何 pass 写下的每像素中间产物，留给后面的着色 pass 用"。两种格式抢的都是它，三种管线形状：

```text
传统 deferred（本仓库现状的远亲）:
  光栅化：现场算属性 → 写 G-buffer → 延迟光照读 G-buffer → 颜色

UE5 Nanite 形状（链式融合）:
  光栅化：只写 ID → resolve 回表算属性 → 写进 G-buffer → 延迟光照照旧 → 颜色
                （vis-buffer 在 G-buffer 上游，resolve 就是"G-buffer 生成器"）

纯 vis-buffer 形状（id Tech / Frostbite）:
  光栅化：只写 ID → resolve 回表算属性 → 直接算光照 → 颜色（没有 G-buffer）
```

UE 的链式接法是**融合选择**：vis-buffer 只接管几何半边，resolve 完落回 G-buffer，后面整套光照/后处理一概不动——复用既有 deferred 管线。但这是特例不是定义：纯 vis-buffer 引擎 resolve 完直接着色，根本不落这份中间产物。

**两个格式的存在前提正好相反**，这是"什么时候需要 vis-buffer"的判据：

- G-buffer 的前提：光栅化时**能**算属性（每三角形够大，插值成本可摊），带宽扛得住 40+ B/px 读写。
- vis-buffer 的前提：光栅化时**不该**算属性（微三角形，插值成本爆炸）或带宽逼你瘦身。

判断顺序：没有海量微几何、没有带宽压力 → G-buffer 就是终态，vis-buffer 是负优化；出现了 → 纯替换（id Tech 式）或上游替换（UE5 式）。**vis-buffer 永远不是"额外需要"的东西。**

## 5. 代价与边界

- resolve 是一次全屏 pass + 对索引/顶点缓冲的**随机读**（缓存不友好，三角形越碎越散），属性插值成本从光栅化挪到全屏着色——可见像素才付一次是赚，但 pass 结构变深。
- **透明冲突**（《为什么引擎没有都上 GPU 驱动》§4 的机制落点）：blend 要逐像素读改写**颜色**，vis-buffer 阶段写的是 ID，一个 RT 共存不了，顺序也无法与 resolve 交织——半透明永远在这条流水线之外的独立 forward pass。注意两种格式在这点上难兄难弟：岗位本身是"先攒齐再算"，透明需要"现在就混"，任何攒法都挤不进。
- MSAA、运动向量要单独安排（vis-buffer 本身不带，Nanite 另写 depth + motion）。

## 6. 本项目落点

- 现状：最简单的单 pass forward（3.5 方向光 + 材质三模式），**没有** G-buffer——本仓库连 deferred 都还不是，谈 vis-buffer 替换为时尚早。
- 步骤 5 的 compute 剔除与 indirect 闭环**完全不改变着色层**——它们换提交方式，着色还是老样子。
- 两级升级路径（已登记[外置拓展条目 5](../../外置拓展需求.md)，未进编号体系）：光源上量级 → **forward+**（clustered light，shader 现场索引光源列表，不动 GDR 基建）；走上微三角形/带宽压力或决意学 vis-buffer → **deferred material**（须先过 5.3 indirect 闭环）。
- **信号判据**：哪天对象表/材质表之外再出现"三角形 ID 表"且全屏 resolve 阶段来读它，就是踩上 vis-buffer 路线的信号。目前它对学习线的价值是解释"为什么最极致的 GDR 引擎都长成 deferred material 形状"：决策权逐层下放（CPU→GPU、draw→indirect、参数→索引），**像素格式是最后一环**。

---

相关阅读：[《为什么引擎没有都上 GPU 驱动》](为什么引擎没有都上 GPU 驱动：CPU 镜像、回读墙与收益边界.md)（决策与提交层的边界论证，本篇的姊妹篇）；[GPU 驱动第一课](../../5-GPU驱动与常驻大场景/5.1-常驻绘制数据/GPU驱动第一课：从逐draw推参数到实例索引读表——新旧两套写法对照.md)（"递索引不递数据"那把刀的出处）；上游《GPU-Driven Rendering》《延迟渲染》（绝对路径见文首）。
