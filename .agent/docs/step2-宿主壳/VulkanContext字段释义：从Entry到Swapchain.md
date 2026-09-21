# VulkanContext 字段释义：从 Entry 到 Swapchain

> 2026-09-21，step2 施工③配套。对象 = `ash_renderer/src/vulkan.rs` 的 `VulkanContext`（施工③版）。
> 读者约定：Unity 图形程序员，每个字段回答三问——**是什么 / Vulkan 为什么要拆出这一层 / 对应 Unity-D3D 里的什么**。
> 配套：建链过程与 ash 0.38 事实见《宿主壳搭建记录》§3；生命周期分层讨论见当轮对话（§11 摘录）。

## 0. 一句话总图

创建链就是依赖链，每层回答一个独立的问题，缺一层 Vulkan 就拒绝开工：

```
Entry         "驱动 DLL 在哪？"        ——加载 vulkan-1.dll，拿到最基础的函数
  └ Instance       "我是谁，要什么"      ——向驱动总部报名+协商扩展/验证层
      └ Surface    "渲染结果去哪个窗口"   ——把 HWND 翻译成显示子系统句柄
      └ PhysicalDevice   "机器上有哪些 GPU，各自什么底子"（只读侦察）
          └ Device  "跟这块 GPU 签合同：要哪些队列/扩展/feature"
              └ Queue  "指令从这条车道提交"
          └ Swapchain "向显示子系统租 N 张可轮换画布"
```

**为什么要七层而不是一个构造函数**：Vulkan 的立场是"所有能力显式协商、所有失败当场可见"。每一层都是一个协商点——你在哪一层报错，就说明是哪一层的假设错了（loader 缺失 / 扩展不支持 / 没有符合条件的 GPU / surface 与设备不兼容……）。D3D 把这些协商藏在一个 Device 构造里，Vulkan 把它们摊开成依赖链。

### 0.1 字段速查表（详释见 §1–§10）

| 字段 | 是什么 | 为什么存在 |
|---|---|---|
| `entry` | vulkan-1.dll 加载器 | Vulkan 没有自带运行时，函数从厂商驱动 DLL 现查现用；**提前 drop 它 = 卸载 DLL**，所以它纯当生命周期锚 |
| `instance` | 与驱动总部的全局握手 | 报家门（API 1.3）+ 协商扩展/验证层。Vulkan 规矩：**能力必须提前声明，没声明的等于不存在** |
| `debug` | 验证层报警回调 | Vulkan 对"合法但蠢"的用法不崩、只错——验证层是免费审计器，messenger 是那条已注册的报警线路 |
| `surface_fns` / `swapchain_fns` | 函数指针表（`{fp, handle}`） | 符号在 `new()` 时解析好，方法调用 = 查表直调；操作 instance 的挂 instance 表，操作 device 的走 device 表 |
| `surface` | 窗口里"可送显区域"的抽象 | D3D 把 HWND 塞在 SwapChain 描述里，Vulkan 把"窗口→可显示区"拆成独立对象才能跨平台；注意它是**扩展**对象——无头渲染不需要它 |
| `physical_device` | GPU 的只读侦察句柄 | 不是你创建的，**没有 destroy**——选型筛选（同族/离散优先）就在这层做完，把它传给 create_device 即结果 |
| `device`（逻辑） | 对选中 GPU 的"启用清单合同" | 物理=硬件有什么，逻辑=你声明要什么——同卡多进程各签各的合同，驱动只背你勾的状态 |
| `queue` + 族号 | 指令提交车道 | 族=按能力分组的通道；挑 graphics+present 同族 → 单车道走天下，省掉跨族所有权移交 |
| `swapchain` | 向 DWM 租的 N 张画布轮换合同 | 显示器扫第 i 张时你画第 i+1 张，不能画正被扫描的——3 张 = 三缓冲；**images 归租约所有**，Drop 里只销 swapchain 不销 images |
| `format` / `extent` | 画布的像素格式 / 客户区尺寸 | UNORM vs SRGB 的 gamma 责任在 M2 材质时对齐；resize = extent 变 = 换租约（施工④主题） |

## 1. `entry: Entry` —— 驱动加载器（进程级）

**是什么**：vulkan-1.dll 的动态加载句柄 + 最顶层函数表（`vkCreateInstance`、`vkEnumerateInstanceLayerProperties` 这些"不依赖任何已创建对象"的函数）。

**为什么要单独一层**：Vulkan 没有自带运行时——GL 渲染上下文由系统运行时提供，Vulkan 只有规范，实现散装在各厂商 ICD（安装驱动时落地的 DLL）里。`Entry::load()` 就是 libloading 去 dlopen `vulkan-1.dll`、按规范找导出符号。它是 unsafe 的：从 DLL 里拿到的符号无任何来源担保。

**生命攸关的细节**：`Entry` 内持 libloading 的 Library 句柄。fn 指针在 `new()` 时已拷进各 loader，此后字段本身不再被读——但**提前 drop 它 = 提前卸载 vulkan-1.dll**，instance 里所有句柄瞬间悬空。所以它以 `#[expect(dead_code)]` 的身份住在结构体里，纯当生命周期锚。

**Unity/D3D 映射**：没有直接对应物——最接近的是 native plugin 里 `LoadLibrary("vulkan-1.dll")` 那一下；D3D 把这层藏进了系统运行时（d3d11.dll 自带）。

## 2. `instance: Instance` —— 与驱动总部的连接（进程级）

**是什么**：全局连接，创建时携带三样协商结果——`ApplicationInfo`（报家门：叫 ash_renderer、要 API 1.3）、instance 扩展清单（VK_KHR_surface + VK_KHR_win32_surface + debug utils）、layer 清单（验证层按可用性开启）。

**为什么扩展要在这里声明**：instance 级扩展描述的是"整机层面"的能力，WSI（窗口系统集成）是典型——它由 ICD 提供、跟 GPU 无关，所以必须在 Instance 创建前声明，"我接下来要玩窗口显示"。Vulkan 的规矩：**没提前声明的能力等于不存在**，事后不能补。

**验证层为什么也在这一层**：验证层是驱动厂商之外（Khronos）提供的"官方用法检查器"，以 layer 形式挂在 instance 上拦截所有 API 调用。本机没装 SDK 时会走裸奔分支（日志声明）。

**Unity/D3D 映射**：≈ `IDXGIFactory` 的角色（枚举适配器、创建交换链之前的"系统级"对象）。

## 3. `debug: Option<(debug_utils::Instance, DebugUtilsMessengerEXT)>` —— 报警器

messenger = 一条已注册的回调规则："WARNING/ERROR 级别的验证消息，调这个函数指针打到 stderr"。它随验证层一起生死：没装验证层就没有它（`None`）。**销毁顺序必须在 Instance 之前**（它是挂在 instance 上的回调，instance 死了它就成了悬空指针——这也是 Drop 反序拆除的一环）。

## 4. `surface_fns` / `swapchain_fns` —— 函数表（前文对话释义）

ash 0.38 的 loader 结构，内容就是两个字段：`{ fp: 函数指针表, handle: 绑定的 instance/device }`。`new()` 时用 `vkGetInstanceProcAddr`/`vkGetDeviceProcAddr` 把符号一次性解析进来，之后方法调用 = 查表直调。

- `surface_fns: surface::Instance` —— **instance 级**函数表（查 surface capabilities/formats/支持、销毁 surface）；
- `swapchain_fns: swapchain::Device` —— **device 级**函数表（create/acquire/present/destroy swapchain）；
- `win32_fns: win32_surface::Instance` —— 也是 instance 级，只干一件事 `vkCreateWin32SurfaceKHR`。

**为什么分两级**：扩展函数挂在哪一级，取决于它操作谁——操作 instance 的走 instance 表，操作 device 的走 device 表。driver 实现上 device 级入口可以绕过 instance 转发、更快。命名后缀 `_fns` 是"这是一组函数指针"的缩写（ash 社区惯用 `_loader`，层级更清晰的话是 `surface_instance_fns`/`swapchain_device_fns`）。

## 5. `surface: vk::SurfaceKHR` —— 窗口里的"出租区"（窗口级）

**是什么**：一个不透明句柄，代表"这个 OS 窗口里、可以被渲染结果占据并送显的区域"。我们的路径：从 bevy 窗口实体的 `RawHandleWrapper` 里取出 `hwnd`（+ `hinstance`，缺则 `GetModuleHandleW` 兜底），手写 `vkCreateWin32SurfaceKHR` 建出。

**为什么 WSI 要拆出独立对象**：D3D 里 HWND 是塞在 SwapChain 描述里的一个参数；Vulkan 把"窗口 → 可显示区域"单独做成 `SurfaceKHR`，才能做到同一套 Device/Swapchain 代码换平台只换 surface 创建那几行（VK_KHR_win32_surface / xcb / wayland……）。`KHR` 后缀 = 扩展对象：**显示是扩展功能不是核心功能**——无头渲染（渲染到离屏 image）就不需要它。

**生命周期的隐含约定**（侦察篇 §3）：bevy 靠"窗口实体上的 `RawHandleWrapper` 组件存在与否"表达"窗口还能不能显示"；我们的 surface 生命周期完全自管——**先 drop Device/Surface 再让 winit 销窗**，否则销毁 surface 时窗口已亡 = 悬空句柄。

**Unity/D3D 映射**：D3D11 `IDXGISwapChain` 创建时绑定 HWND 的那一步，被拆成了独立对象。

## 6. `physical_device: vk::PhysicalDevice` —— 侦察报告（不持有）

**是什么**：一块 GPU 的只读查询句柄（本机 = RTX 2060）。用它查 `Properties`（名字/类型/API 上限）、`QueueFamilyProperties`（车道配置）、surface 支持性、扩展清单——**筛选逻辑就在这层**：graphics+present 同族的族号 + 离散卡优先打分。

**它不是你创建的**：枚举出来、查询完，函数指针借 instance 的用。**不需要也没有 destroy**——它天生不属于你。选型结果体现为把它传给 `vkCreateDevice`。

**Unity/D3D 映射**：`IDXGIAdapter`。

## 7. `device: Device`（逻辑设备）—— 谈判合同（窗口级）

**是什么**：对选中物理 GPU 的一份"启用清单"合同——本项目只开了一个通用队列族 + `VK_KHR_swapchain` 扩展 + 零额外 features（清屏用不到；M6 descriptor buffer 时在这里加 feature）。

**为什么物理/逻辑分两层**：同一块 GPU 同时服务多个进程（或同进程多份合同），每份合同只背自己勾选的状态——驱动不用为"你可能用到的所有能力"维持状态。物理设备是"硬件有什么"，逻辑设备是"你声明要用什么"，**协商结果物化为合同**。

**Unity/D3D 映射**：最贴 `ID3D11Device`/Unity 的 GfxDevice——之后所有资源（buffer/image/pipeline）都从它创建，所有指令提交都经它的队列。

## 8. `queue_family_index: u32` + `queue: vk::Queue` —— 提交车道（窗口级）

**是什么**：`queue` 是提交指令的通道句柄；`queue_family_index` 记录它属于哪个族。族（family）= 按能力分组的通道集合，族内每条通道能力相同——有的族只会搬运（transfer）、有的只会算（compute）、有的全才（graphics）。

**为什么我们的筛选条件是"graphics + present 同族"**：present（把画面交给显示引擎）也是队列操作。桌面 GPU 普遍有一个 graphics/transfer/compute/present 全会的万能族，选它则一个族号走天下，省掉"两条车道间的所有权移交"（`SharingMode::EXCLUSIVE` 只在单族时合法）。真需要专用传输通道（M2/M3 大批量上传）时再拆第二条车道——这是路线图里"拷贝队列起步"的伏笔。

**Unity/D3D 映射**：D3D11 immediate context / 引擎内部"渲染命令流"的概念位。

## 9. `swapchain: vk::SwapchainKHR` —— 画布轮换租约（resize 级）

**是什么**：与显示引擎（Windows 上是 DWM 合成器）签的一份数量合同："按 BGRA8_UNORM、1600×900、至少 3 张，租我一批可轮换的画布"。

**为什么至少 min+1 张（我们拿了 3 张）**：显示器逐行扫描第 i 张画面的同时，CPU/GPU 在画第 i+1 张——不能画正在被扫描的那张。双缓冲 = 2 张（一读一画，互相等），三缓冲 = 3 张（读 N、画 N+1、还备着 N+1，等待更少、延迟略高）。`min_image_count + 1` 是"比最小要求多备一张"的惯用起手。

**为什么 present 模式选 FIFO**：FIFO（垂直同步语义）是规范**唯一保证所有平台可用**的模式；MAILBOX/IMMEDIATE 等要查了 `present_modes` 再选——施工④若要"不锁帧"在这里换。

**所有权规则**：`swapchain_images` 里的 image **归租约所有**——swapchain 销毁时它们一起没了，所以 `Drop` 里**只** destroy swapchain、绝不手动 destroy 这些 image（验证层会抓）。这个"租约含画布"的所有权模型，跟 M2 以后"自管 VkImage/缓冲"形成对照。

**Unity/D3D 映射**：`IDXGISwapChain` 的 backbuffer 数组；Unity 里"camera 输出目标在 N 个 backbuffer 间轮换"的那套机制。

## 10. `swapchain_format` / `swapchain_extent` —— 画布的像素格式与尺寸

- **format（BGRA8_UNORM 优先）**：每像素 8bit×4、字节序 B-G-R-A、UNORM = "无符号归一化"（整数 0~255 映射到 0.0~1.0）。清屏阶段无所谓，M2 起它决定"shader 输出色彩空间怎么解释"——UNORM 画布配 SRGB 线性输出会出现画面发灰/过亮的经典 gamma 坑，届时与 SRGB 变体二选一并全链对齐。
- **extent（= `current_extent`）**：窗口**客户区**实际像素（不是窗口外框尺寸，DPI 缩放后两者会差）。`current_extent == u32::MAX` 表示"驱动未定，调用方自报尺寸"——桌面 Windows 上不会出现，代码里留了显式报错。**resize 时这个值会变 → 换租约（重建 swapchain）= 施工④的主题之一**。

## 11. 还没有的字段（施工④预告）

| 缺什么 | 干什么 | 归属 |
|---|---|---|
| Command pool / command buffer | 录制 `vkCmdClearColorImage` 等指令 | 帧级（随帧轮转，不复用同一帧的录制） |
| Image view | image 的"视角"（swapchain image 建 view 后才能绑定） | resize 级（跟 swapchain 重建） |
| Fence / Semaphore ×2 | CPU↔GPU、GPU 内部的等待关系（acquire→画→present 三段衔接） | 帧级 |

这些进来后，`Context` 一个名字就装不下四种寿命了（见 §12）。

## 12. 生命周期四层（摘自 2026-09-21 对话，施工④拆分的命名依据）

| 寿命 | 字段 | 死亡时机 |
|---|---|---|
| 进程级 | entry / instance / debug | `Drop`（App 退出、World drop） |
| 窗口级 | surface / physical_device / device / queue / queue_family_index | 同上（surface 跟窗口实体约定走） |
| resize 级 | swapchain / images / format / extent | resize 整体换血（④ 引入 `Swapchain` 独立类型） |
| 帧级 | （待 ④）command buffer / 同步对象 | 每帧轮转 |

`Drop` 反序拆除（`device_wait_idle → Swapchain ← Surface ← Messenger ← Device ← Instance`）就是"依赖谁、谁后建，谁先死"的逆操作——顺序错的本质是"先死长辈再死晚辈"，验证层（装上后）会当场报 use-after-destroy。
