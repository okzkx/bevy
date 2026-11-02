# src 顶层目录依赖分层

#架构 #结构

ash_renderer/src 六个顶层目录形成一棵单向无环的依赖树，箭头全部向下：

```
                 driver（宿主桥/编排）
                /   |    \      \
         overlay  scene  render  remote
              \    |    /  |
               \   |   /   |
                render ─────┘
                   |
                 common
```

实测口径：按各目录内 `crate::` 路径引用归类（量级随代码漂移，方向不漂移）。

## 依赖矩阵

| 目录 | 依赖 | 被依赖 | 职责一句话 |
|---|---|---|---|
| `common` | 无 | render、overlay、driver | 错误类型、`or_fatal`/`or_warn` 糖、字节打包等机械样板 |
| `render` | common | scene、overlay、remote、driver | Vulkan 执行层：bindless 表、环、管线、录制提交 |
| `scene` | render（仅 mechanism/assets 三文件） | overlay、remote、driver | 场景事实：账本、相机、灯光、资产登记 |
| `overlay` | render、scene、common | driver | egui 立即模式 UI 与绘制半边 |
| `remote` | scene、render | 无 | BRP 调试读口（GapSnapshot/账本取证） |
| `driver` | 全部四层 | 无 | 宿主桥：系统注册、帧编排、错误分流 |

## 三条撑结构的规则

### 1. 决策与执行分层，箭头永远向下

`render` 只认数据形状（表、环、管线、`FrameDraw` 契约），不知道谁在画、画什么。
它对 common 的 67 处引用全是执行层样板下沉（错误枚举、糖家族、字节打包）；
`FrameDraw` 定义在消费者 `record_frame` 旁边，契约跟着消费方走。

### 2. scene→render 是唯一允许的业务层穿透

场景资产驻留要知道 GPU 形态（MeshPool 槽位现查、Swapchain 存在才跑 tick），
依赖方向合法且单向——render 里没有任何指向 scene 的引用。除这 5 处（集中在
`mechanism/assets/` 的 residency/lod_registry），scene 只认 bevy。

### 3. overlay/remote 是纯消费端，driver 唯一"引用一切、不被引用"

overlay 读 scene 状态 + render 管线画界面；remote 读 GapSnapshot/账本做调试
取证——都不做帧决策。driver 把四层接进 Bevy 调度表（Startup/Update/PostUpdate/Last），
自己不持有 Vulkan 状态；对 overlay 的唯一引用是 `draw_frame` 调 `paint_overlay`。

## 判定线：新依赖该不该建

- 箭头必须向下（driver→业务层→render→common），出现反向引用即结构缺陷。
- 平级模块之间互不 import，需要共享就走共同的下层（如常量下沉 common）。
- 跨层协作走窄契约，契约定义在消费者旁（FrameDraw/GapSnapshot 同款）。

## 已知瑕疵（低优先级，不影响正确性）

- **P2**：`residency_tick` 的 run_if 全限定引用 `crate::render::BindlessTextureTable`/`Swapchain`——
  scene 感知 render 资源存在性；且"draw_frame 写 GapSnapshot → residency_tick 消费 →
  flush_uploads 排后"的三方时序契约散落三处，无单一归属点。
- **P3**：`MAX_LOD_LEVELS` 常量住 render 被 scene 引用，可下沉 common。

## render 是否再拆"底层/中层"——已评估，暂不拆

render 内部已是严格分层树（实测引用）：`core`（device/context/memory/swapchain/
binding/pipeline/record/target）零上层引用 ← `resident` ← `passes` ← `frame`，
`stats` 旁挂零依赖。"中层/底层"边界已由 `render/core/` 目录表达。

- **目录升格**（拆 `src/gfx/` + `src/render/`）：可读性小赚，约束力零变化（同 crate
  靠纪律），四层消费者几十处 import 重排——不值。
- **拆 crate**（底层独立 crate，参照 wgpu-hal/wgpu）：唯一有编译器硬约束的方案，
  触发条件 = 底层出现第二个消费者（另一渲染后端/工具复用 device 层）；此前不做。
- **注意**：错误通道已穿透底层——`core/device/swapchain.rs` 与 `frame/frame_pool.rs`
  返回 `BevyError`（让路语义内化）。若将来拆 crate，错误类型需先抽到 common。

## 姊妹篇

- [常驻绘制数据组织](常驻绘制数据组织.md)——环与账本的数据流细节
- [indirect 命令环](indirect%20命令环.md)——GPU 侧命令组装
