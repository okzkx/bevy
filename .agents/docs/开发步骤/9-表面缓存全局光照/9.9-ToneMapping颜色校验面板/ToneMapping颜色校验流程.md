# ToneMapping 颜色校验面板：功能流程导读

> 这是 9.9 段的全局导读，回答"这东西怎么跑起来、数据从哪来到哪去、每个文件管哪一段"。逐任务的记录在 `9.9.1`–`9.9.9` 各篇里。

## 它解决什么问题

后处理链在 tone mapping 前后各取一个点，把"场景有多亮 / 曲线吃了什么 / 最终显示成什么颜色"画成直方图。没有它，调曝光和 ACES 增益只能靠眼睛猜；有了它，天空变白能立刻看出是"输入 p50 偏高"还是"增益太小"，室内偏暗能看出是测光值不对还是曲线压太狠。

## 数据流

```
HDR (RGBA16F) ──┬────────────────────────────► A 域：原始 HDR，log2 轴 −16…+8 EV
                │
Bloom ──► + ──► × exposure_scale ──► B 域：校色输入，同一 log2 轴
                │        (tonemap_input_color，present 与 scope 共用)
                ▼
          ACES/Narkowicz ──► × 校色强度 ──► LDR (RGBA8 线性) ──► C 域：sRGB 编码后 0…1 轴 + CbCr
                                                │
                                                ▼
                                          SMAA → swapchain（写 SRGB view 时做一次 sRGB）
```

每帧在 `post_chain.record_to_ldr` 里，LDR 转 `SHADER_READ_ONLY`（`color_to_sampled`）之后、SMAA 之前，如果 `enabled || once` 且链启用，就录一个 `vkCmdFillBuffer`（清 bins）→ 屏障 → `cs_main`（8×8，一线程一点，stride 4）→ 屏障（COMPUTE/SHADER_WRITE → TRANSFER/TRANSFER_READ）。

回读是另一条路：面板 8 Hz 调 `ColorScopeReader::refresh`，或 BRP `color_scope_get` 直接调 `ColorScopeTargets::read_bins`，两者都走 one-shot command buffer：`cmd_copy_buffer(bins → staging)` → `finish_and_read` → `ColorScopeSnapshot`。面板、BRP、CSV 复核脚本读的都是这一个结构，没有第二份解析。

## 模块边界

| 层 | 文件 | 职责 |
| --- | --- | --- |
| 着色器公共段 | `shaders/common/tonemap_shared.wgsl` | `BloomParams`（6 f32）、`exposure_scale_of`、`tonemap_input_color`、`apply_tonemap_strength` —— present 与 scope 的唯一真源 |
| 采样内核 | `shaders/post/color_scope.wgsl` | 三域采样、log2/0…1 轴、CbCr、五类计数、组内计数合并、头区写入 |
| 管线与资源 | `render/passes/post/color_scope.rs` | 常量、set layout/管线、`ColorScopeTargets`（bins/描述符/池）、录制 `ColorScopeDraw`、`ColorScopeSnapshot`、`ColorScopeReader` |
| 后链 | `render/passes/post/post_chain.rs` | 可选 `color_scope` 字段，在 LDR 屏障后、SMAA 前录制 |
| 帧侧 | `driver/host/frame/post.rs` | `color_scope_draw`：双闸（enabled/once + 链启用）、组 bindings、begin_frame、组装 Draw |
| 初始化 | `driver/host/init.rs`、`driver/host/mod.rs` | 建 targets/pipelines、并进 `SetLayouts`、进 `InitChain`；注册 settings/reader |
| 登记 | `render/layout_registry.rs`、`render/mod.rs` | 契约测试的清单与对外导出 |
| 面板 | `overlay/tool_window/color.rs`、`tool_window/mod.rs` | 「颜色校验」页、`ToolPage::Color`、参数注入 |
| 遥控 | `remote/features/color_scope.rs`、`remote/mod.rs` | `color_scope_set` / `color_scope_get` |
| 复核 | `tools/color_scope_check.py` | 账平/轴/样本数/B 域对齐校验，出 CSV |

## 代码改动解析（git show --stat 实数）

### `00f873e3b` 主体（24 文件，+2156 / −40）

| 文件 | 行数 | 说明 |
| --- | --- | --- |
| `shaders/common/tonemap_shared.wgsl` | +49 | 新文件；抽取的公共函数（当时 5 f32 的 `BloomParams`） |
| `shaders/post/color_scope.wgsl` | +165 | 新文件；三域直方图内核 |
| `shaders/post/present_tonemap.wgsl` | 34 | 删本地 struct、import 公共段、fs_main 改调公共函数 |
| `render/passes/post/color_scope.rs` | +747 | 新文件；管线/资源/快照/节流/录制 |
| `overlay/tool_window/color.rs` | +506 | 新文件；颜色页与 CbCr 纹理缓存 |
| `remote/features/color_scope.rs` | +191 | 新文件；两个 BRP 方法 |
| `tools/color_scope_check.py` | +255 | 新文件；CSV 复核 |
| `driver/host/frame/post.rs` | 56 | `PostInput` 加三个字段、`color_scope_draw` |
| `driver/host/frame.rs` | 11 | 提前取可变借、`PostChainDraw.color_scope` |
| `driver/host/init.rs` | 27 | targets/pipelines/layout/InitChain |
| `driver/host/mod.rs` | 11 | 注册 settings/reader |
| `overlay/tool_window/mod.rs` | 33 | 新页、导航分组、参数 |
| `render/core/record/compute.rs` | 28 | 新增二维 `dispatch_grid` / `record_grid` |
| `render/passes/post/bloom.rs` | 21 | `tonemap_strength` 字段、布局、打包、默认值 |
| `render/passes/post/exposure.rs` | +6 | `state_buffer(slot)` 访问器 |
| `render/passes/gbuffer/gbuffer_targets.rs` | +7 | 暴露共享线性采样器 |
| `remote/features/render_settings.rs` | 19 | `bloom_set` 支持并校验 `tonemap_strength` |
| `layout_registry.rs` / `render/mod.rs` / `post/mod.rs` / `post_chain.rs` / `remote/mod.rs` / `features/mod.rs` / `pipeline.rs` | 5 / 8 / 1 / 9 / 4 / 1 / 2 | 登记、导出、模块声明、后链字段、方法注册、滑杆范围 0.5…=16 |

### `d290a144f` 设备本地内存与 stride 4（5 文件，+163 / −93）

`color_scope.wgsl` 100 行上下：头区改 GPU 写、push constant（`var<immediate>`）传帧号、组内标量计数合并、越界改条件块；`color_scope.rs` 128 行上下：`DeviceStorage`、去掉 CPU 写头/`scratch`、`begin_frame` 简化、`ColorScopeDraw` 带 `bins` + `frame_index`、补 transfer 屏障；`frame/post.rs` 8 行：传帧号与 buffer；`render/mod.rs` 12 行：删 `pack_color_scope_header` 导出；`tools/color_scope_check.py` 8 行：stride 4 的样本期望。

### `a811d55e3` 室内外标定（2 文件，+41 / −22）

`exposure.rs` 57 行：默认 key 0.01305、min_scale 0.05、注释与单测重写；`bloom.rs` 6 行：gain 6.13 与注释。

## 关键决定与理由

1. **B 域只有一个定义**：抽公共函数，而不是让面板重算。抽取边界刻意不含曲线和 `mode` 分支——那些留在各自入口，避免公共段变成第二个后处理入口。
2. **bins 必须是设备本地内存**：宿主可见内存上做百万级原子，p95 直接 40 ms；改用 `DeviceStorage` 后 CPU 不再写头，改成 GPU 填充 + 着色器写头。
3. **回读节流放 reader 不在面板**：面板只声明"我要最新数据"，8 Hz 是 reader 的策略；BRP 用 `force` 绕过节流，CSV 脚本用 `--once`。
4. **`once` 在录制侧消费**：否则单帧开关会一直重复采样。
5. **`tonemap_strength >= 1` 提前返回**：`mix(x, y, 1.0)` 不保证逐位等于 `y`，而默认值必须逐位不改画面。
6. **key 与 gain 必须同改**：曲线输入恒等于 `key × gain`，只改一个会让室内画面整体漂移；天空的问题只能靠 `min_scale` 解决——这是 9.9.9 全部调参的依据。

## 踩过的坑（浓缩）

- 域基址与 shader 常量是两份声明，`0/1029/2058` vs `16/1045/2074` 的错位让面板把曝光快照当直方图 → 已加单测钉死。
- RGB parade 与 luma 共用 256 窗口 → luma 和变成 4×样本数 → 加 `CHANNEL_STRIDE`。
- C 域零样本在 `add_unit_axis` 与 `cs_main` 各记一次 → 加 1824 → 计数收到 `cs_main` 一处。
- B 域与测光值固定差 `log2(scale)` → 期望值改成 `avg_log2 + log2(scale)`，这不是放水，是把定义算进去。
- `Workgroup` 原子数组过不了 `spirv-val`；`push_constant` 在 naga 30 叫 `immediate`；ash 0.38 的复制阶段叫 `TRANSFER` 不叫 `COPY`。
- egui 0.36 的 `ScrollArea` 会把内容压到视口高（"优先压缩不滚动"），底部图片要 `set_max_height(f32::INFINITY)` + 自己 `painter().image` 画。
- `tools/__pycache__` 会被 workspace 的 `tools/*` glob 当成 crate，跑完脚本要删。

## 想改的时候从哪下手

- 加一个域（比如"Bloom 输出"）：`color_scope.wgsl` 加 `BASE_D`、`color_scope.rs` 加缓冲与枚举、`ColorScopeSnapshot` 加访问器、面板加一块、`color_scope_check.py` 的账平循环加一项；`bins` 总长度常量同步（有单测断言 `16 + 3*1029 + 4096 == 7199`，会提醒你）。
- 换轴范围：`COLOR_SCOPE_AXIS_MIN_EV` / `SPAN_EV` 与 shader 的 `AXIS_MIN_EV` / `BIN_PER_EV` 必须同改，`axis_bin_ev` 单测会自动暴露错位。
- 提成本上限：先关掉面板测基线，再按 stride 4 → 共享内存 → 4 Hz 的顺序降级（计划 §4 的顺序），不要先动 bins 布局。
