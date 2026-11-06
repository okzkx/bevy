# 整帧执行时序表（ash_renderer · record_frame_steps）

> 行 = 录制调用单元（`record_frame_steps` 与各模块 record 的连续录制段），行内拆真命令：**动作**（dispatch/draw/fill，有执行事件）、**同步**（barrier，纯排序）、**状态**（bind 系，烤进动作参数，无执行事件）。真命令包 = 一行含多只同类动作命令，数量随场景/分辨率变化，公式标注。
> 同步机制列回答"这一步靠什么保证顺序"：Handoff（compute.rs dispatch 后自动录）｜布局/内存屏障（手录或 helper 内录）｜framebuffer 语义｜提交级等待｜fence/跨帧。

| # | 段 | 命令 / 操作 | 层级 | 写资源 @access | 读资源 @access | 同步机制 | src→dst stage | 执行形态 | 跨队列 / 跨帧 | 备注 |
|---|----|-------------|------|----------------|----------------|----------|---------------|----------|---------------|------|
| 1 | ① | **1×布局屏障** 交换链 UNDEFINED→COLOR_ATTACHMENT_OPTIMAL | 布局屏障 | swapchain image（进场写） | presentation engine 上一轮读（对面） | 布局屏障 + 提交级 image_available | COLOR_ATTACHMENT_OUTPUT→COLOR_ATTACHMENT_WRITE | 等 acquire 放手 | ✓ 对手是 presentation engine | 与进场屏障一配对：提交在 CAO 等 image_available |
| 2 | ① | **1×布局屏障** 槽深度 UNDEFINED→附件 | 布局屏障 | 槽深度（进场写） | — | 布局屏障 + 帧槽 fence | EARLY\|LATE_FRAGMENT_TESTS→深度附件写 | 整批串行点 | ✓ 跨帧：上轮使用被 fence 等完 | srcAccess 空合法：无并发引用 |
| 3 | ① | **4×布局屏障** G-buffer 三色 + HDR UNDEFINED→COLOR_ATTACHMENT_OPTIMAL | 布局屏障 | G-buffer 四件（进场写） | — | 布局屏障 + 帧槽 fence | COLOR_ATTACHMENT_OUTPUT→颜色写 | 整批串行点 | ✓ 跨帧：同槽 fence | loadOp CLEAR 衔接在 ④ begin 里 |
| 4 | ① | **1×动作** fill_buffer（排序键预填 −inf）+ **1×内存屏障** | 内存屏障 | t_keys @TRANSFER_WRITE | t_keys @SHADER READ\|WRITE | 手录内存屏障 | TRANSFER→COMPUTE_SHADER | fill 与屏障串行 | — | 全容量预填，真实条目 bitonic 沉尾 |
| 5 | ② | cull：**[bind 烤] + 1×dispatch + 1×Handoff 屏障** | Handoff | 可见位 @SHADER_WRITE | 实例表、阴影视锥标记 @SHADER_READ | Handoff::VertexComputeReadWrite | COMPUTE/SHADER_WRITE→VERTEX\|COMPUTE / READ\|WRITE | 与后续 compute 串行；与 ③④ 无全量边可重叠 | — | dst 含 WRITE：同 buffer 后续写要 WAW 依赖 |
| 6 | ② | occlusion：**[bind 烤] + 1×dispatch + 1×Handoff 屏障** | Handoff（+跨帧读） | 遮挡位 @SHADER_WRITE | **两帧前** Hi-Z 金字塔 @SHADER_READ | Handoff::VertexComputeRead | COMPUTE/SHADER_WRITE→VERTEX\|COMPUTE READ | 同上 | ✓ 跨帧：读两帧前金字塔，靠同槽 fence 接力 | 本行当帧无屏障接 Hi-Z——跨帧节拍 |
| 7 | ② | lod：**[bind 烤] + 1×dispatch + 1×Handoff 屏障** | Handoff | 档位环 @SHADER_WRITE | 实例表、遮挡位 | Handoff::VertexComputeRead | COMPUTE/SHADER_WRITE→VERTEX\|COMPUTE READ | 同 cull | — | 关闭时 host 预填档位环 0 |
| 8 | ② | indirect 组装：**[bind 烤] + 1×dispatch + 1×Handoff 屏障** | Handoff | 场景命令环 @SHADER_WRITE | 可见位、档位环 | Handoff::IndirectCommandRead | COMPUTE/SHADER_WRITE→DRAW_INDIRECT/INDIRECT_COMMAND_READ | 与 ④ 命令消费串行 | — | 场景绘制 draw 的命令环 |
| 9 | ② | shadow_build：**[bind 烤] + 1×dispatch + 1×Handoff 屏障** | Handoff | shadow 命令环 @SHADER_WRITE | 视锥标记、实例表 | Handoff::IndirectCommandRead | 同上 | 与 ③ 命令消费串行 | — | 关闭/空场整行跳过 |
| 10 | ② | cluster_build：**[bind 烤] + 1×dispatch + 1×Handoff 屏障** | Handoff | 光源列表 @SHADER_WRITE | 灯光数据、深度 | Handoff::ComputeFragmentRead | COMPUTE/SHADER_WRITE→COMPUTE\|FRAGMENT READ | 与 ⑤ 片元串行 | — | 空场为 None |
| 11 | ② | **真命令包·全帧最大**：透明三连 = collect **[bind 烤]+1×dispatch+屏障** + sort **[bind 烤]+105×dispatch+105×屏障** + gather **1×dispatch+屏障** | Handoff ×107 | 键/列表/命令环 @SHADER 读写 | 排序键、列表、实例表 | collect=ComputeRead；sort=ComputeReadWrite×105（轮间 WAW）；gather=IndirectCommandRead | COMPUTE→COMPUTE（gather→DRAW_INDIRECT） | 105 轮严格串行（bitonic 轮间数据依赖）；轮内 16K 线程并行 | — | 容量 2¹⁴：dispatch 数 = log₂n·(log₂n+1)/2 = 105，容量变按公式重算；优化立案见外置拓展 #9 |
| 12 | ③ | shadow：**1×布局屏障**（进场）+ **[bind 烤] + draw_indirect ×级联数** + **1×布局屏障**（出场） | 布局屏障 | 级联深度图 @深度附件写 | shadow 命令环 @DRAW_INDIRECT、实例表 | 进出场布局屏障每帧恒录（合成描述符总绑） | EARLY\|LATE→FRAGMENT READ（出场 depth→sampled） | 逐级联 draw 串行在 pass 内 | — | 描述符总绑所以屏障恒录，哪怕 pass 跳过 |
| 13 | ④ | G-buffer：**begin_rendering（含 4×loadOp CLEAR 动作）+ [bind/viewport/VB-IB 烤] + draw_indirect ×1 + end_rendering** | framebuffer 语义 | 三色附件 + 深度 @附件写 | 命令环 @DRAW_INDIRECT、实例表/顶点索引 | framebuffer 语义（重叠像素）+ ④ 命令环 Handoff 边 + 顶点读 cull/lod 产出 | （命令环边：COMPUTE→DRAW_INDIRECT） | 几千 draw 自由重叠，重叠像素有序；与 ②③ 无全量边、可重叠（资源不相交） | — | VUID-06179 附件数与管线声明一致 |
| 14 | ④→⑤ | **4×布局屏障**：三色 →SHADER_READ_ONLY + 深度 →READ_ONLY | 布局屏障 | G-buffer 四件（store 完成声明） | ⑤ 合成采样、⑥ Hi-Z 采样 | 手录布局屏障 | COLOR_ATTACHMENT_OUTPUT\|EARLY/LATE→FRAGMENT\|COMPUTE READ | 整批串行点 | — | dst 含 COMPUTE 是给 ⑥ Hi-Z 归约 |
| 15 | ⑤ | 合成：**begin_rendering（clear HDR）+ [bind 烤] + draw(3 顶点) ×1 + end_rendering** | framebuffer 语义 | HDR 目标 @COLOR_WRITE | G-buffer 四件 + 光源列表 + 阴影图 @SHADER_READ | 行 14 布局屏障 + framebuffer 语义 | — | 全屏单 draw | — | 无深度附件；depth==0 判背景 |
| 16 | ⑤ | 透明前向：**1×内存屏障 + 1×布局屏障（深度转回附件，同录）+ [bind 烤] + draw_indirect ×1**（end 由段尾收） | 内存屏障 | HDR @COLOR READ\|WRITE（LOAD+混合） | HDR、透明命令环 @DRAW_INDIRECT | 显式内存屏障（两次动态渲染间无光栅序保证） | COLOR_ATTACHMENT_WRITE→READ\|WRITE | 与合成串行（屏障钉住） | — | 深度只测不写；storeOp DONT_CARE |
| 17 | ⑤→⑥ | **1×布局屏障** HDR →SHADER_READ_ONLY | 布局屏障 | HDR（store 完成） | ⑦ Bloom / ⑧ tonemap 采样 | 手录布局屏障 | COLOR_ATTACHMENT_OUTPUT→FRAGMENT READ | 整批串行点 | — | — |
| 18 | ⑥ | **真命令包**：Hi-Z 归约 = **1×金字塔屏障**（GENERAL→GENERAL）+ dispatch ×(levels−1) + 级间屏障 ×(levels−2) | Handoff（级间） | 金字塔全 mip @SHADER_WRITE | 槽深度 @textureLoad、上 mip | 进场屏障 + Handoff::ComputeRead 级间串 | COMPUTE/SHADER_READ→COMPUTE/SHADER_WRITE | 逐级串行（下级读上级写） | — | levels = 完整 mip 链（1080p→11）；末级写→下轮遮挡读由下帧 cull 屏障罩住 |
| 19 | ⑦ | **真命令包**：Bloom = prefilter + down(N−1) + up(N−1)，每只 pass = 进场屏障 + draw(3 顶点) + 出场屏障 | 全屏 helper 内录屏障 | down/up 各级 @COLOR WRITE | 上级/down 邻级、HDR @SHADER_READ | 逐 pass 进出场布局屏障（record_fullscreen_sampled） | CAO→FRAGMENT READ（每只） | pass 间串行，pass 内全屏并行 | — | 2N−1 只 draw + 2(2N−1) 道屏障，N 随分辨率 |
| 20 | ⑧ | tonemap：**1×进场屏障 + draw ×1 + 1×出场屏障** | 布局屏障 | LDR @COLOR WRITE | HDR + Bloom 终品 + 参数 @SHADER_READ | 布局屏障（record_fullscreen_sampled） | TOP_OF_PIPE→…→FRAGMENT READ | 全屏单 draw | — | 坑：LDR 上轮停在 SHADER_READ_ONLY，不录 UNDEFINED 进场触发 VUID-09592 |
| 21 | ⑧ | SMAA：**真命令小包** = edge（进场屏障+draw+出场屏障）+ blend（同构） | 布局屏障 | edges/blend 中间目标 @COLOR WRITE | LDR、edges @SHADER_READ | 每轮进出布局屏障 | TOP_OF_PIPE→…（同 tonemap） | 两 pass 串行 | — | 关闭为 None；中间目标同槽上轮 fence 等完 |
| 22 | ⑧ | neighborhood + UI：**begin_rendering（写 swapchain view，不清）+ [bind 烤] + draw ×(1+UI 批次) + end_rendering** | framebuffer 语义 | swapchain image @COLOR WRITE | LDR + blend + 图集 @SHADER_READ | 同渲染实例光栅序（UI 盖场景） | — | 同一次渲染实例内按序叠加 | — | SMAA 关闭时 shader 内直通 |
| 23 | ⑨ | **1×布局屏障** COLOR_ATTACHMENT_OPTIMAL→PRESENT_SRC_KHR | 布局屏障 | swapchain image（写完成声明） | presentation engine（下一轮读） | 布局屏障 + render_finished 信号量交接 | COLOR_ATTACHMENT_OUTPUT+COLOR_WRITE→BOTTOM_OF_PIPE | 整批串行点 | ✓ 对手是 presentation engine | present 消费 render_finished |

**提交（非 cmb 命令）**：`queue_submit` 是 host 对队列的操作，不录进命令流；它挂的等待三元组也是提交参数——① `image_available` @COLOR_ATTACHMENT_OUTPUT（图门，① 段进场屏障的对面）；② 上传票据 timeline @ALL_COMMANDS（快照值，CPU 不阻塞，兼 transfer 写跨队列可见性）；signal `render_finished` + 帧槽 fence。

**计数快照**（1080p、容量 2¹⁴、无 UI）：真命令 ≈ 动作 135+（其中 sort 105+2）、屏障 130+（其中 sort 106）、其余为状态命令与 begin/end rendering。
