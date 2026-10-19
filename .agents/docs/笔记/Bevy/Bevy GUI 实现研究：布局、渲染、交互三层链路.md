# Bevy GUI 实现研究：布局、渲染、交互三层链路

> 侦察对象：本仓库 bevy 0.20.0-dev 源码（`crates/bevy_ui*`、`bevy_text`、`bevy_picking`、`bevy_input_focus`）。触发点：`examples/ui/styling/gradients.rs`——这个渐变示例里带着完整的 GUI（flex 布局画廊 + 按钮 + 文字标签 + 悬停/点击交互），虽小却把 GUI 三大件全走了一遍。本文按"数据怎么建 → 布局怎么算 → 像素怎么画 → 点击怎么来"展开，未加前缀的 `lib.rs`/`ui_node.rs` 等均指对应 crate 的 `src/` 下文件，断言均带 file:line。

## §0 判定线

- **Bevy 的 GUI 是 retained mode（保留模式）实体树，不是即时模式。** 每个 UI 元素是一个实体，样式是 `Node` 组件、布局结果是另一些组件，布局系统增量重算；egui 那种"每帧重新声明所有控件"是另一条路线，Bevy 原生 UI 不走。
- **一套 UI 三层分家（0.20 已拆成三个 crate）：** `bevy_ui`（样式组件 + taffy 布局 + 语义层，**不依赖 bevy_render**）、`bevy_ui_render`（RenderApp 侧全部 GPU 工作）、`bevy_ui_widgets`（无样式控件 + `Activate`/`ValueChange` 事件）；`bevy_text`（文字排版）、`bevy_picking`（统一命中管线）、`bevy_input_focus`（键盘焦点）横向供血。
- **布局引擎是 taffy 0.14（纯 Rust flexbox/grid），且 UI 有自己专属的变换体系**：`UiTransform`/`UiGlobalTransform` 是 2D 仿射（原点左上、y 向下，CSS 约定），与 3D 场景图的 `bevy_transform` 是两套东西（ui_transform.rs:144/:227）。
- **UI 渲染是独立 render pass，但借用主相机的颜色附件**：每个相机额外配一个 subview=1 的 UI 视图，`ui_pass` 挂在 Core2d/Core3d 的 PostProcess 之后、upscaling 之前，无深度模板附件（render_pass.rs:52-63）——UI 永远画在场景之上、不参与深度测试。
- **文字栈 0.20 已换血：cosmic-text → parley 0.11（Linebender 文本栈）+ swash 0.2.6（光栅化）**（bevy_text/Cargo.toml:44-45）。字形光栅进动态图集，以 TEXTURED 四边形走普通 UiPipeline；是**位图图集不是 SDF**（对照 TextMeshPro 的 SDF）。
- **交互是"统一 picking 管线的一个后端 + 观察者"，不是 GUI 回调**：winit 事件 → `PointerInput` → `ui_picking` 命中 → `PointerHits` → hover 差量 → trigger 指针事件 → 控件的全局 observer → `Activate`/`ValueChange` → 用户 observer。旧的 `Interaction`/`ui_focus_system` 在 0.20 已标弃用（focus.rs:61-70），但默认仍在跑。
- **裁剪全在 CPU（Sutherland–Hodgman 多边形裁剪，发生在 prepare 生成顶点时），shader 零 clip 逻辑**；反过来，圆角/边框全在 shader（SDF 距离场），每个顶点随身携带 radius/border/size/point 参数。
- **命中判定有一帧布局滞后**：`ui_picking` 跑在 PreUpdate，读的是上一帧 PostUpdate 算好的 `ComputedNode`/`UiStack`；同帧改布局当帧点击会有一次旧几何判定（通常无感，但机制上要知道）。

## §1 全景：六个 crate 与一帧的时间线

### 1.1 crate 分工

| crate | 职责 | 关键依赖 |
|---|---|---|
| `bevy_ui` | `Node` 样式组件、taffy 布局桥、`ComputedNode`/`UiStack`/`CalculatedClip` 等计算组件、`Text`/`ImageNode` 等 widget、UI picking 后端、accesskit 同步 | taffy 0.14、bevy_text；**无 bevy_render** |
| `bevy_ui_render` | RenderApp 侧 extract/queue/prepare/draw、五套渲染管线（Ui/Gradient/BoxShadow/TextureSlice/UiMaterial）、CPU 裁剪、批处理 | bevy_ui + bevy_text + bevy_sprite_render/bevy_render |
| `bevy_ui_widgets` | 14 个**无样式**控件（Button/Slider/Checkbox/TextInput…），全局 observer + `Activate`/`ValueChange` | bevy_ui + bevy_picking + bevy_input_focus + bevy_a11y |
| `bevy_text` | parley 排版、swash 光栅化、字形图集、`TextLayoutInfo` 输出 | parley 0.11、swash 0.2.6；**无 bevy_ui/bevy_render** |
| `bevy_picking` | 统一命中管线：指针实体、后端协议、hover、17 类指针事件 | 独立；UI 通过 feature 接入 |
| `bevy_input_focus` | `InputFocus` 资源、`FocusedInput<M>` 冒泡分发、Tab 导航 | 独立 |

依赖方向单向：`bevy_ui_widgets → bevy_ui → bevy_text`，渲染层 `bevy_ui_render` 只被 app 侧引用。用户什么都不用手动注册——`DefaultPlugins` 已含 `UiWidgetsPlugins`（bevy_internal/src/default_plugins.rs:100）与 picking 插件，gradients.rs 没手动挂任何 picking 插件却收得到 `PointerOver`，即为实证。

### 1.2 一帧时间线（UI 相关）

```
First        picking::input  读 WindowEvent → PointerInput（指针实体收动作）
PreUpdate    InputFocus      dispatch_focused_input → FocusedInput<M> 冒泡 trigger
             ProcessInput    PointerInput::receive 写指针位置
             Backend         bevy_ui::ui_picking → PointerHits   ← 用上一帧布局
             Hover           generate_hovermap → pointer_events
                             trigger PointerOver/Click/Drag…（observer + 消息双通道）
Update→PostUpdate 之间       SpawnScene schedule：bsn! 场景 resolve + 排队实例化
PostUpdate   UiSystems 链（chain_weak，bevy_ui/lib.rs:157-168）：
             Prepare(propagate_ui_target_cameras：解析相机/缩放/视口)
             → Propagate(ComputedUiTargetCamera/RenderTargetInfo 层级传播)
             → Content(measure_text_system：文本变化 → 生成 measure)
             → Layout(ui_layout_system：Node↔taffy 同步 + 计算 + 写回)
             → PostLayout(update_clipping_system、text_system(产出最终 glyph)、a11y)
             UiSystems::Stack(ui_stack_system)：独立 set，产出 UiStack 排序
RenderApp    Extract 链 15 个系统（变更驱动）→ queue_uinodes → sort
             → prepare_uinodes(裁剪+顶点+批次) → ui_pass(Core2d/3d)
```

两处时间差值得钉死：**交互（PreUpdate）读上一帧布局**（见 §0）；**渲染（RenderApp extract）读本帧布局**——布局在 PostUpdate 刚算完，RenderApp 接着抽，所以画面不滞后。

## §2 数据侧：Node 与它的十三件随从

### 2.1 Node：样式即组件

`Node`（ui_node.rs:462-544）就是一份 CSS 属性清单：`display`、`position_type`、`left/right/top/bottom`、`width/height/min/max`、`aspect_ratio`、`align_items/justify_content`、`flex_*` 全家、`grid_*` 全家、`margin/padding/border`、`border_radius`、`row_gap/column_gap`、`overflow`、`direction`。字段全部是 CSS 对应物，配合 `Val` 单位（geometry.rs:32：Px/Percent/Vw/Vh/VMin/VMax/Em/Rem）。

它用 `#[require(...)]` 挂了 13 个隐式组件（ui_node.rs:463-477）——spawn 一个 Node 自动获得全套：

| 组件 | 职责 |
|---|---|
| `ComputedNode` | 布局计算结果（size/content_size/border/padding/scroll_position…，ui_node.rs:33-71）。**注意位置不在里面**，在 `UiGlobalTransform` |
| `UiTransform`/`UiGlobalTransform` | UI 专属 2D 仿射（translation 支持响应式 Val2），布局系统自顶向下累乘 |
| `ContentSize` | 装可选 measure 闭包，让 taffy 反推叶子大小（见 §3.3） |
| `BackgroundColor`/`BorderColor` | 填充色/四边独立边框色；`Outline` 外围描边不占布局 |
| `FocusPolicy` | 旧命中体系的 Block/Pass（新体系用 `Pickable`，见 §6） |
| `ScrollPosition` | overflow=Scroll 时的逻辑像素滚动偏移 |
| `Visibility` | 不可见节点不参与命中与渲染 |
| `ZIndex`/`GlobalZIndex` | 兄弟间局部排序 / 跨层级全局排序 |
| `EmSize` | 解析 Val::Em 用，文本节点由 `sync_font_size_to_em_size` 维护 |
| `ComputedUiTargetCamera`/`ComputedUiRenderTargetInfo` | 归属相机 + 渲染目标的 scale_factor/物理尺寸（层级传播） |

还有几个重要的非 require 组件：`UiTargetCamera`（ui_node.rs:3408，根节点手动指定相机）、`FixedNode`（:3539，无视父级、直接相对视口定位——做 HUD 用）、`CalculatedClip`（:2776，继承的裁剪矩形链）、`OverrideClip`（:2846，丢弃继承裁剪）、`Pickable`（bevy_picking 提供，控制挡不挡下层）。

### 2.2 渐变组件

`BackgroundGradient`/`BorderGradient`（gradients.rs:544/:552）装 `Vec<Gradient>`，`Gradient` 三态 Linear（角度+stops）/Radial（形状+位置）/Conic（起始角+stops），每个 stop 是 `ColorStop{color, position: Option<Val>}`，插值色彩空间 `InterpolationColorSpace`（11 种：Oklaba/Oklcha/Srgba/Hsla/Okhsla 及 Long 变体等）由渐变自带、可在运行时切换——gradients.rs 的按钮切的就是它。

## §3 布局：ECS↔taffy 双向桥

### 3.1 UiSurface：一个资源包住整棵 taffy 树

`UiSurface`（layout/ui_surface.rs:67-75）持三样东西：`entity_to_taffy: EntityHashMap<LayoutNode>`（实体→taffy 节点映射）、`TaffyTree<NodeMeasure>`（taffy 树本体，node context 装可选 measure）、根实体→隐式视口节点的映射。**ECS 是唯一真相源，taffy 树是布局系统每帧按需同步的影子**：`upsert_node`（:116-147）对已存在实体只 `set_style`（Node 的每个字段经 `convert::from_node`（layout/convert.rs:93-175）翻译成 taffy::Style，Px 乘 scale_factor、Percent 除 100、Vw/Vh 用视口物理尺寸），新实体则 `new_leaf`。

细节：每个 UI 根下面会垫一个**隐式视口节点**（Display::Grid、宽高 100%，ui_surface.rs:192-223），让根节点的百分比尺寸有参照。

### 3.2 ui_layout_system 四步（layout/mod.rs:104-293）

1. **同步**：遍历 `(Ref<Node>, Ref<EmSize>, Ref<ContentSize>, …)`，任一变化（配合渲染目标信息构造 `LayoutContext`）就 upsert；删除实体走 `RemovedComponents` 清 taffy 节点。注意变更检测的粒度——只有 Node/EmSize/ContentSize/目标信息变化才碰 taffy，`BackgroundColor`/渐变变化不触发布局重算。
2. **层级**：从 `UiRootNodes` 递归 `update_children`，同步父子关系到 taffy。
3. **计算**：每个根调 `ui_surface.compute_layout(root, 物理尺寸)`（:272-277），以渲染目标尺寸为 `AvailableSpace::Definite`，taffy 内部回调 measure 闭包（见 §3.3）。
4. **写回**：`update_uinode_geometry_recursive`（:296-522）递归把 taffy 结果写回组件——`ComputedNode` 的 size/content_size/border/padding/em_size/滚动 clamp，以及 `UiGlobalTransform`：`inherited *= (local_affine 平移到中心)`（:399-412），局部平移 = 布局位置 − 父滚动 + 居中修正。

### 3.3 测量回调：文本/图片怎么反推自己的大小

flexbox 里文字节点"由内容决定大小"是反向问题：taffy 算到该叶子时不知道文字多宽。解法是 measure 回调：

- `ContentSize`（measurement.rs:139-145）装 `Option<NodeMeasure>`，枚举 `Fixed/Text/Image/Custom(Box<dyn Measure>)`（:106-111）避免装箱常用类型。
- `measure_text_system`（widget/text.rs:288-359）在文本/字体变化时调 bevy_text 的 `TextPipeline::create_text_measure` 生成 `TextMeasureInfo`，写进 ContentSize；`update_image_content_size_system`（widget/image.rs:372-420）同理装图片尺寸。
- `ui_layout_system` 把 measure 作为 **leaf context** 交给 taffy；`compute_layout_with_measure` 回调里（ui_surface.rs:249-283）`TextMeasure::measure`（widget/text.rs:204-276）按 BorderBox 扣掉 padding/border，在 Definite/MinContent/MaxContent 三种约束下让 parley `break_all_lines` 试排，返回 clamp 后的尺寸。

这就是文本参与布局的唯一通道——**布局阶段文本只算尺寸，不产出字形**；最终 glyph 在 PostLayout 的 `text_system` 才生成（bevy_ui/lib.rs:252/:265）。

### 3.4 UiStack：绘制顺序的权威

`ui_stack_system`（stack.rs:52-135）产出资源 `UiStack{ uinodes: back-to-front 全序, partition: 按目标相机切片 }`（:30-35），并给每个节点写 `ComputedStackIndex`。算法：收集"栈根"（无父 UI 根 + 任意带 `GlobalZIndex` 的节点）→ 按 `{GlobalZIndex, ZIndex, 新增, 变更}` 排序 → 递归子树内按 `ZIndex` 稳定排序（同 ZIndex 后加的在上）。**渲染正序消费、命中逆序消费**（最上层先测）——一份数据两个方向用。

## §4 渲染：从组件到顶点缓冲

### 4.1 视图：每相机一个 UI subview

`extract_ui_camera_view`（bevy_ui_render/lib.rs:1247-1382）给每个 Camera2d/3d 配第二个 `ExtractedView`：`RetainedViewEntity` 的 subview 固定为 `UI_CAMERA_SUBVIEW=1`（:1211-1216），投影是左上原点正交 `orthographic(0, w, h, 0, 0, 0, FAR)`（:1286-1294）——所以 UI 坐标系就是"像素坐标系"。主相机 render entity 挂 `UiCameraView(ui视图)`、UI 视图挂 `UiViewTarget(主相机)`，一对组件互链（:1313-1329）。

### 4.2 Extract：变更驱动，不是全量重抽

`ExtractedUiNodes`（lib.rs:407-417）是"主世界实体 → 该实体的渲染子项 map"两级结构 + 本帧变更集。`extract_uinode_changes`（:578-710）先用变更检测收集要更新的实体（背景色/图片/边框/文本/渐变/阴影/viewport…各一个 extract 系统，只遍历变更集），渲染世界每实体一个 `commands.spawn_empty()` 的临时 render entity 存子项。**这是"World 状态 → 渲染世界快照"的标准 extract 模式，与 3.1 学过的官方 Extract 同构，但粒度到组件级变更。**

### 4.3 Queue 与排序：z 偏移表

`queue_uinodes`（:2047-2109）把每个子项按 `stack_index + z_offset` 塞进对应相机的 `TransparentUi` phase。z_offset 表（lib.rs:117-130）是理解"一个节点怎么拆着画"的钥匙：

```
BOX_SHADOW -0.1 → BACKGROUND 0 → BORDER 0.01 → GRADIENT 0.02
→ BORDER_GRADIENT 0.03 → IMAGE 0.04 → MATERIAL 0.05 → TEXT_SELECTION 0.055
→ TEXT 0.06 → TEXT_STRIKETHROUGH 0.07 → TEXT_CURSOR 0.08 → INLINE_IMAGE 0.09
```

同一节点的背景、边框、渐变、文字不是一次画完，而是拆成多个 phase item 按微偏移排序——**顺序靠排序不靠 draw 顺序约定**。

### 4.4 Prepare：裁剪 + 顶点 + 批次

`prepare_uinodes`（:2116-2428）把每个子项变成四边形顶点写入全局 `UiMeta`（单一 `RawBufferVec<UiVertex>`，:1997-2012），按贴图 AssetId 分批（同贴图连续顶点区间一个 batch）。两个关键动作：

- **CPU 裁剪**：`clip_polygon`（clipping.rs:16-66）用 Sutherland–Hodgman 把四边形对 `CalculatedClip` 链里每个裁剪矩形的四条边做凸裁剪、线性插值属性，输出三角扇。裁掉全部就整项跳过。`CalculatedClip` 本身在主世界由 `update_clipping_system`（bevy_ui/layout/clipping.rs:13-117）递归传播（overflow 节点把自身 rect 变换后压进继承链）。**GPU 端无 scissor、shader 无 clip。**
- **顶点布局**（pipeline.rs:58-80）：position(3) + uv(2) + color(4) + flags(1u32) + **radius_x(4) + radius_y(4) + border(4) + size(2) + point(2)**——后五个是 SDF 参数，普通四边形也随身带，圆角矩形画法见下。

### 4.5 ui.wesl：SDF 圆角矩形是主角

fragment 的核心是带椭圆角的 SDF：`sd_rounded_box`（ui.wesl:102-121）按 point 象限选角，直线区用 `max(corner_to_point.x, .y)`，角区用 Taubin 近似 `distance_to_ellipse_approx`；边框 = `max(外SDF, −内SDF)`（`sd_inset_rounded_box` 内缩求内缘，:123-162）；抗锯齿就一行 `antialias(d) = saturate(0.5 - d)`（:186-189），由管线特殊化 def `ANTI_ALIAS` 开关。四边 `BorderColor` 独立着色的实现很实在：每条非透明边**单独抽成一个节点**（BORDER_FLAGS 合并同色边，bevy_ui_render/lib.rs:1107-1159），shader 里 `nearest_border_active` 判断最近的边该上哪条颜色（ui.wesl:164-183）。

管线本身极简：group(0)=ViewUniform、group(1)=纹理+filtering sampler（pipeline.rs:22-39），特殊化维度只有 `target_format` 和 `anti_alias`（:48-52）。`UiPipelineKey` 这么小，是因为所有花活（渐变/阴影/九宫格）都拆成了独立管线插件。

### 4.6 渐变与阴影：参数走顶点属性，不走 uniform

- **Gradient**（gradient.rs）是内建专属管线（`GradientPlugin`，:44-72），不是 ui_material 机制。CPU 在 extract 阶段把 LinearGradient/Radial/Conic 解析成 `ResolvedGradient`（起止点/中心/方向，:506-670），stop 的 Val 位置、隐式 stop 插值、排序全在 CPU 完成；prepare 时**每对相邻 stop 生成一段独立三角形**，颜色预先转换到目标色彩空间（:792-833）。GPU 参数（`g_start/g_dir/两端颜色/端点距离/hint`）全在顶点属性里（:773-790，15 个属性），唯一 bind group 是 view uniform。色彩空间是**管线特殊化维度**（`IN_OKLAB/IN_HSV…` 编译期分支，gradient.wesl:171-226）——gradients.rs 那 11 种空间 = 11 条管线变体，示例故意这么演示。渐变复用 ui 包的 SDF 画边框，所以渐变节点同样有圆角。
- **BoxShadow**（box_shadow.rs）同为内建管线：fragment 用解析法——沿 y 采样 `SHADOW_SAMPLES`（shader def，默认 4，lib.rs:198-206）条高斯积分线，erf 多项式近似（box_shadow.wesl:27-70）。**不批处理**，每项独立 batch（box_shadow.rs:570-572 注释）。

### 4.7 文本渲染与自定义材质

- 文本走同一个 UiPipeline：glyph 是 **TEXTURED 四边形**，纹理是字形图集 Image；extract 按图集纹理分组（lib.rs:1585-1607）、prepare 出顶点（:2345-2415）。阴影=换色重发 glyph，下划线/删除线=从 `RunGeometry` 的度量出矩形。
- `UiMaterial` trait（ui_material.rs:103-126）是自定义 UI 材质扩展点：`AsBindGroup` 资产 + 可覆写 shader/`stack_z_offset`/`specialize`，`MaterialNode<M>` 组件 `#[require(Node)]` 挂任意节点。**无批处理**——每个材质节点独立 draw（ui_material_pipeline.rs:110-117），适合少量特效节点，不适合大规模。

## §5 文字：parley + swash + 动态图集

0.20 的 bevy_text 已与早期版本（cosmic-text 时代）完全不同栈：

- **排版 = parley 0.11**（Linebender）：`TextPipeline::update_buffer`（pipeline.rs:72-295）把所有 span 文本喂给 parley ranged_builder，样式（FontFamily/字号/行高/字距/字重/特性）作为 StyleProperty 逐段压入，`break_all_lines` 断行 + align。字体数据库是 parley 的 fontique：`Font` 资产 = `fontique::Blob`（font.rs:29-45），加载时以"家族名 + `asset_id:{id}` 别名"双名字注册进全局 Collection（font.rs:81-99）。
- **光栅化 = swash**：`update_text_layout_info`（pipeline.rs:343-476）遍历 parley 排版结果，按 run 组 `FontAtlasKey`（字体数据 id+字号+变体哈希+平滑模式），swash `Scaler` 出字形轮廓，`FontAtlas`（font_atlas.rs:32-41）用 `DynamicTextureAtlasBuilder` 光栅进 RGBA8 动态图集（2px padding，放不下开新 pow2≥512 图集）；alpha mask 转白色+alpha（可 tint），彩色 emoji 直通（font_atlas.rs:215-259）。
- **组件结构**：`Text` 在 bevy_ui（widget/text.rs:114-128，require 8 件），子实体 `TextSpan`（bevy_text/text.rs:191-201）带自己的 TextFont/TextColor；`TextFont.font` 是 `FontSource`——句柄、家族名、CSS 风格家族列表都行（text.rs:283-304），`FontSize` 支持 Px/Vw/Rem 等（:782-796）。三个上下文资源 FontCx/LayoutCx/ScaleCx（parley_context.rs:36-171）由 TextPlugin 初始化。
- **给布局的接口**：`create_text_measure`（pipeline.rs:298-340）→ `TextMeasureInfo::compute_size`（:600-613）——就是 §3.3 那个 measure 回调的后半截。`TextLayoutInfo`（:483-507）是最终产物：glyphs + run_geometry（下划线/删除线度量）+ 光标 + 选区矩形 + inline_boxes。
- **给命中的接口**：`TextLayoutInfo.run_geometry.bounds` 让 `ui_picking` 能按字形 run 命中到具体 span 实体（bevy_ui/picking_backend.rs:278-297）。

## §6 交互：从 winit 到 Activate

### 6.1 指针全链

```
winit WindowEvent(CursorMoved/MouseInput)
  → bevy_winit 转成 bevy WindowEvent 消息（bevy_winit/state.rs:207+）
  → bevy_picking::input（First）转成 PointerAction 写指针实体（input.rs:121-201；
      鼠标指针是 Startup 就 spawn 的实体，触摸每个触点一个）
  → ProcessInput：PointerInput::receive 更新位置/按键（pointer.rs:326）
  → Backend：bevy_ui::ui_picking 命中测试（picking_backend.rs:102-276）
  → Hover：generate_hovermap（按后端 order/depth 排序 + Pickable 阻挡规则 + 指针捕获）
  → pointer_events（events.rs:816+）：hover 差量 → Over/Out/Enter/Leave；
      Press/Release/Move → Press/Click/Drag 系列；每类事件**同时 trigger（observer）
      与写消息（MessageReader）双通道**
  → 控件全局 observer → Activate/ValueChange → 用户 observer
```

`bevy_picking` 的核心设计：**后端没有 trait，"写 `PointerHits` 消息的系统"就是后端**（lib.rs:122-134）。3D mesh 是一个后端、UI 是一个后端（`order = camera.order + 0.5` 保证 UI 压在 mesh 上，picking_backend.rs:268-274）、窗口兜底是一个后端，hover 阶段统一合并排序。UI 后端的命中测试：按 `UiStack` 逆序（最上层先）+ `ComputedNode::contains_point`（圆角矩形 SDF 同款判定，ui_node.rs:211-234）+ clip 判定，遇到 `Pickable.should_block_lower` 就停。

### 6.2 Button 的 Activate 触发链（ui_widgets/button.rs）

1. picking 发 `PointerPress` → 全局 observer `button_on_pointer_down`（:78-100）插入 `Pressed` 组件；
2. 松开产生 `PointerClick` → `button_on_pointer_click`（:60-76）校验 `pressed && !disabled` → `commands.trigger(Activate { entity })`；
3. 用户侧 `entity.observe(|_ev: On<Activate>| …)` 或 `add_observer` 收到（examples/ui/widgets/button.rs:63-71）。
4. 键盘路径：`On<FocusedInput<KeyboardInput>>`（:39-58）——焦点实体上 Enter/Space 同样发 Activate。

### 6.3 键盘焦点与无障碍

- `bevy_input_focus`：`InputFocus` 资源记录焦点实体，`dispatch_focused_input`（lib.rs:394-436）把原始键盘消息包成 `FocusedInput<M>` 沿 `ChildOf` 链**从焦点实体向上冒泡 trigger**；`PointerFocusPlugin` 的 `click_to_focus`（pointer_focus.rs:37-64）在点击时迁移焦点。控件用 `TabGroup`/`TabIndex` 参与 Tab 导航。
- 无障碍三层：控件自带 `AccessibilityNode(Role::…)`（require）；bevy_ui 的 `AccessibilityPlugin` 把几何/标签/状态同步进 accesskit（accessibility.rs:53-190）；bevy_winit 的桥把树更新推给系统级 API（accesskit 0.25）。
- **旧体系并存但已弃用**：`ui_focus_system`（bevy_ui/focus.rs:172-365）仍注册在 PreUpdate 维护 `Interaction`/`RelativeCursorPosition`，0.20 注释明说待删——新代码一律走 picking + `Hovered`/`Pressed` 组件。

## §7 控件层与 bsn! 场景语法

### 7.1 bevy_ui_widgets：无样式受控组件

设计宣言在 crate 文档（lib.rs:5-48）：**控件不持有业务状态（MVC 的 V），发 `ValueChange<T>` 事件由 app 决定写不写状态**——即 React 的受控组件模式；例外是 Button（无状态只发 `Activate`）和 TextInput（自管编辑状态）。14 个控件（button/checkbox/slider/scrollbar/scrollarea/text_input/list/menu/dialog/modal/radio/tabs/tree/popover）全部是"状态组件 + `#[require]` 几何/角色组件 + 全局 observer"的组合，零样式——样式由用户给 Node，或上 `bevy_feathers` 主题层（FeathersPlugins，见 examples/ui/widgets/feathers_*.rs）。

全局 observer 是这套控件的统一手法：插件 `add_observer` 注册一次，observer 内部用 `Query<…, With<Button>>` 过滤实体，`event.propagate(false)` 显式消费冒泡（"consume what you use"）。

### 7.2 bsn!：场景即代码

`bsn!` 是 proc-macro（bevy_scene/macros/src/lib.rs:176-179），展开成 `impl Scene` 的构造代码；`Scene` trait（bevy_scene/scene.rs:49-70）只有 `resolve` 一个核心方法，可组合（元组即组合）。`Commands::spawn_scene` 把 resolve+spawn 排队，由 **SpawnScene schedule（位于 Update 与 PostUpdate 之间）** 统一执行（bevy_scene/lib.rs:931-945）。语法要点（macros/src/lib.rs:26-155）：组件 patch（只写字段覆盖默认值）、`template(|ctx|)` 动态组件（可读 Resource）、`@scene()` 嵌套场景、`#Name` 实体引用、`on(|ev: On<E>|)` 内联观察者、`Children [ a -- b ]` 子实体表。本质上是**声明式的 Prefab**——对照 Unity 的 Prefab/UXML。

## §8 调试 UI：Bevy 自家全狗粮，生态接 egui

### 8.1 核心的调试 UI 没有独立通道

全仓 grep 零 egui/imgui/dear 依赖——Bevy 核心不内置任何外部 GUI 框架，调试 UI 全部走自家渲染设施（吃狗粮立场：调试 UI 也是 UI，正好当 bevy_ui 的实弹测试）。0.20 的调试工具集在 `bevy_dev_tools`：

| 调试工具 | 绘制手段 |
|---|---|
| FPS 覆盖层（fps_overlay.rs:21-26） | **bevy_ui 实体**：Node + Text + GlobalZIndex，还用了 `MaterialNode` 自定义 UI 材质 |
| picking 调试（picking_debug.rs:1-14） | bevy_ui Text 画指针事件流覆盖层 |
| 指针可视化（bevy_picking/cursor.rs） | 自绘 mesh 指针 |
| UI 布局调试（bevy_ui_render/debug_overlay.rs:41） | `UiDebugOptions` 线框，画在 ui_pass 内部 |
| render_debug | 自建专用管线读纹理做渲染图可视化 |

### 8.2 生态的调试 UI：bevy_egui，接入形状与 frenderer 接 Dear ImGui 同构

`bevy_egui`（第三方，Bevy 核心零依赖）的模式就是"即时模式 GUI 引擎 + 宿主渲染器适配层"，三段式：

```
Update 阶段   EguiContexts 拿上下文 → egui_ctx.run(...) 闭包声明控件 → egui 出 FullOutput
输入桥        bevy WindowEvent → egui raw_input；focus 仲裁：
              egui 想要鼠标/键盘时（wants_pointer_input）挡掉游戏输入
RenderApp     一个 render graph 节点：抽 epaint 的 ClippedPrimitive（clipped 三角形列表）
              → 顶点/索引缓冲 → 一个管线 + 字体图集 → 画在主 pass 之后
```

这与 frenderer 接 Dear ImGui 的结构（NewFrame → build → 顶点缓冲 → 自己的 Vulkan 管线画出来）一模一样——这一层 egui 和 imgui 没有本质区别，差异在数据模型归属。

### 8.3 问答：为什么 Bevy 核心不用现成 GUI 框架（2026-09-29 问答落档）

- **拒绝的不是现成库，是外来框架替它定义 retained 数据模型**。"UI=实体"是 bevy_ui 的价值主张：能 Query、能变更检测、能进 scene（bsn!）、能接动画、能挂 observer、能出 AccessKit 树——即时模式给不了任何一样。
- **零件全买的现成**：taffy（布局算法）、parley+swash（文字栈）、AccessKit（无障碍）、winit（窗口输入）。准确说法是"买了发动机变速箱轮胎，没买整车"——整车（数据模型+集成）正是引擎要自己拥有的。
- **生态自发形成分界线：运行时产品 UI 用 bevy_ui，调试工具用 egui**——与 Unity 同构（Editor 里全是 IMGUI，运行时产品 UI 是 UITK retained+flexbox）。
- **代价客观说**：bevy_ui 长期是 Bevy 公认短板，被 egui 压着打了好几个版本；0.20 三 crate 拆分 + 文本换 parley 就是重注补课。

## §9 回看 gradients.rs：一个示例走全链路

```rust
commands.spawn(Node { flex_direction: Column, row_gap: px(20), … })   // §2：样式组件
    .with_children(|c| c.spawn((Node{…}, BackgroundGradient::from(LinearGradient{angle, stops}), …)))
commands.spawn_scene(buttons_scene())                                 // §7.2：bsn 场景排队实例化
```

- **布局**：Node 树每帧 PostUpdate 进 taffy（§3），`px()/percent()` 是 Val 语法糖；`aspect_ratio: Some(1.)` 参与约束求解。
- **动画渐变**：`update` 系统每帧 `*angle += 0.5 * dt`——只改 `BackgroundGradient`，不碰 Node，所以**不触发 taffy 重算**，只走 extract 渐变系统 → 每帧重新解析 + 重铺 stop 三角形（§4.6）。这正是 0.20 变更检测分层的好处：布局与外观各自独立的失效域。
- **按钮**：bsn 里 `Button`（ui_widgets）+ Node 样式 + `on(|ev: On<PointerOver>|…)` 悬停换 `BorderColor`（§6 链路）；`add_observer(on_activate_change_space)` 收 `Activate` 后按 `Has<PreviousButton>/Has<NextButton>` 区分按钮，改所有 `BackgroundGradient` 的 `InterpolationColorSpace`——**色彩空间是管线特殊化维度，切换瞬间换 pipeline 变体**（§4.6）。
- **label**：`template(|ctx|)` 读 `AppSettings` 资源生成初始 `Text`，后续由 observer 直接改 `Text.0`——文本变更 → measure 重算 → 布局重排 → PostLayout 重新光栅（§5），一条完整的响应链。

## §10 Unity/UE 对照

| 概念 | Bevy 0.20 | Unity |
|---|---|---|
| retained UI + flexbox | bevy_ui 实体树 + taffy | **UI Toolkit**（UXML/USS + 内置 flexbox 布局引擎）——Bevy 这套整体上最接近 UITK |
| 即时模式 | egui（第三方） | IMGUI（OnGUI） |
| 样式 | `Node` 组件字段（无样式表，全代码/bsn） | USS 样式表 / UXML |
| 挂到哪 | 每相机一个 UI subview，借主相机颜色附件，PostProcess 后画 | Screen Space-Overlay / Screen Space-Camera / World Space |
| 命中 | bevy_picking 统一管线，UI 是一个后端（mesh/窗口也是） | UGUI：EventSystem + GraphicRaycaster；UITK：PanelEventHandler |
| 事件 | trigger 指针事件 + observer（可冒泡、一对多、消息/观察者双通道） | C# 委托回调（onClick 一对一注册）；EventSystem 接口 |
| 控件 | bevy_ui_widgets 无样式 + bevy_feathers 主题 | UITK 控件 + 主题样式表 |
| 状态管理 | 受控组件：控件发 ValueChange，app 写状态 | UITK 同款思路（binding/SetValue）；UGUI 常直接改控件 |
| 文字 | parley+swash，**位图字形图集**（放大会糊） | TextMeshPro **SDF 图集**（无级缩放）——这点 Unity 更强 |
| 圆角/边框 | shader SDF，顶点带参数，运行时零开销改圆角 | UITK 9-slice/UGUI Image sprite（贴图驱动） |
| 场景描述 | bsn! 宏（代码即 Prefab） | Prefab / UXML |
| z 顺序 | UiStack：ZIndex/GlobalZIndex + 兄弟序，渲染排序实现 | sibling index / sorting order |

最大的架构差异一句话：**Unity 的 UI 事件是"注册回调"，Bevy 是"事件总线上挂观察者"**——后者天然支持一对多、冒泡拦截（tooltip、对话框吞事件）、观察者复用，代价是数据流不如回调直观。

## §11 与本项目的关系

当前路线（3.4 管线绘制、3.5 光照）没有运行时 UI，本篇是纯机制侦察，不做运行时落地计划。两条线：

### 11.1 三条可抄的渲染决策

1. **独立 UI pass + 借主 pass 颜色附件、无深度附件**——最小侵入地把 UI 叠在已有渲染输出上，不用改主管线（render_pass.rs:52-63）。
2. **顶点随身带 SDF 参数画圆角矩形/边框**——一个 draw 画完带圆角边框的实心块，比贴图 9-slice 省资产省采样器；这套顶点布局（radius/border/size/point）可以整段搬。
3. **CPU 侧 Sutherland–Hodgman 多边形裁剪替代 scissor**——嵌套滚动裁剪不需要 per-item 动态 scissor，也不吃 GPU 特性。

### 11.2 调试 UI 定案（2026-09-29 用户拍板：egui）

本项目（练习项目）明确不需要运行时 UI，但**窗口内调试 UI 要做**；BRP web console 是另一条线（见《bevy_remote：BRP远程协议与自定义方法》），两线互补——**egui=窗口内 overlay（帧统计、实时调渲染参数），console=进程外状态检查器（World 全量查询、渲染器内部状态）**。

- **路线：egui 裸接 ash**。宿主禁 bevy_render，`bevy_egui` 挂不上（必须挂在 bevy_render 的 render graph 上）；egui 官方后端只有 wgpu/glow，Vulkan 集成要自己写，但工作量是一小块：消费 epaint 的 `ClippedPrimitive`（顶点 pos/uv/color + 裁剪矩形）→ 一个管线 + 字体图集纹理 → blend → 画在场景之后（swapchain image 以 LOAD 模式接画，同 §4.1 bevy ui_pass 借附件的形状）。
- **接入点**（对齐现有宿主结构）：build 在 Update；输入桥落 bevy_winit→WindowEvent 消息处，一个系统翻译成 egui raw_input（bevy_egui 同款做法，或在维护中的 bevy_winit fork 里直接喂 egui-winit）；绘制在 draw_frame 末尾。字体图集上传复用 3.2/3.3 已有的 flush_uploads/GpuImage 链。
- **定位：3.4 管线绘制落地后的验收题**——管线、顶点缓冲、描述符、blend、纹理图集全用上，与已做的字形图集思路同构。
- **blend 判据**：epaint 输出的混合约定（是否预乘 alpha）不拍脑袋，以 egui 官方 wgpu 后端的 render pass 源码为准。
- **对照与胜出理由**：Dear ImGui（frenderer 已接）是备选——imgui-rs 是 C 绑定、状态在闭包；egui 全 Rust，且自己写的集成段恰好覆盖 3.4 全部知识点，故选 egui。
- **订正记录**：本节旧文写"egui 走 egui-wgpu"是错的——egui-wgpu 是 wgpu 后端，本项目是裸 ash，须自己消费 epaint 顶点写 ash 管线。2026-09-29 订正。

## 附：示例地图（examples/ui/）

- 布局 `layout/`：flex_layout、grid、z_index、fixed_node、ghost_nodes
- 样式 `styling/`：borders、box_shadow、**gradients**（本篇触发点）、stacked_gradients
- 滚动 `scroll_and_overflow/`：scroll、scrollbars、overflow 系列
- 文本 `text/`：text_input、ime_support、font_*
- 控件 `widgets/`：button、standard_widgets、feathers_*（主题层）、tab_navigation
- 交互 `../picking/`：低层 picking 演示；`ui_drag_and_drop.rs`、`window_fallthrough.rs`
