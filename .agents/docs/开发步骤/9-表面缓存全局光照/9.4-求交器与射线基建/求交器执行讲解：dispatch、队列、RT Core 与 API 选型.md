# 求交器怎么执行：CPU 录一条 dispatch，射线在 GPU 上生灭

"求交器"听起来像一个可以调用的函数：传进一条射线，拿回命中结果。9.4 不是这个形状。CPU 从来拿不到单条射线的结果，它每帧只做三件事：资产期建 BLAS、每帧写地址表、录制命令。射线本身由 compute 线程现场生成，通常只存在一个线程的几十条指令里。

核心判断：**CPU 的"调用"粒度是 dispatch，不是射线；一次 dispatch 启动几万到几百万个线程，每个线程自己打射线。**

## CPU 每帧做的三件事

BLAS 是每份网格的加速结构，TLAS 是场景实例的加速结构；结构本身怎么组织见[加速结构讲解](加速结构讲解：BLAS、TLAS 与两层 BVH.md)。这里只看 CPU 侧的动作。

| 时机 | 动作 | 频次 |
|---|---|---|
| mesh 资产 commit / replace | 查构建尺寸、建 BLAS 存储与加速结构对象、录构建命令、取设备地址写地址表 | 每个资产一次 |
| 每帧 ② 段 | 写本帧地址表增量、录 `tlas_build` compute、录 TLAS 构建命令、录消费射线的 compute | 每帧一次 |
| BRP（项目远程调试接口）请求 | 读统计缓冲、把调试图拷回 CPU 转成 PPM 图片 | 按需，不是每帧 |

没有"CPU 调一次 `intersect(ray)`"的接口。原因不是设计偷懒：单条射线的结果要回 CPU，必须等 GPU 执行完再走一次拷贝加 fence（主机侧查询 GPU 是否执行完的同步对象）等待，往返毫秒级；几百万条逐条回读，帧预算不够用。所以命中结果只留在 GPU，给调试图、统计和 9.6 的 GI 使用。

## dispatch 是启动 compute 管线的一条命令

三个概念别混：

- **管线**（pipeline）：init 期用 WGSL 入口、绑定布局、工作组尺寸编译出的设备对象，创建一次。
- **录制**：每帧把管线绑上命令缓冲（录制 Vulkan 命令的字节流），绑描述符集（着色器读取资源的登记表）、写 push 常量（随命令提交的一小段着色器参数），然后发一条 `vkCmdDispatch`。
- **dispatch**：一条命令，启动 `gx × gy × gz` 个工作组；每组线程数由 shader 的 `@workgroup_size` 决定，项目里常见 64（一维）或 8×8（二维）。

仓库把绑定和派发封成 `ComputeHandles`：`bind` 绑管线与描述符，`dispatch` 写 push、发 `vkCmdDispatch`、再落一条交接屏障（`ash_renderer/src/render/core/record/compute.rs:85-113`）。屏障的作用是让前一条命令的写对后一条命令可见。9.4 计划有两条这样的 dispatch：

- `tlas_build`：线程数 = 本帧在场行数，每线程写一条实例记录；
- `ray_debug`：线程数 = 像素数，每线程打一条主射线。

9.6 的 GI 更新也是同形状，只是射线从格子播种而不是从像素播种。

## 命令走图形队列，和渲染共用一条命令缓冲

设备只建一条主队列，队列族（同一类能力的队列集合）要求 `GRAPHICS + COMPUTE + PRESENT` 同族（`ash_renderer/src/render/core/device/context.rs:287-310`）；另有可选的 transfer 队列族只做上传。所有 compute dispatch 录在帧命令缓冲的 ② 段（`ash_renderer/src/render/frame/record.rs:151-197`），与后面的绘制在同一次提交里，天然串行，不需要跨队列信号量。

光线追踪命令没有"专属队列"这回事。本机 Vulkan SDK 的 validusage.json 写得很明确：

- `VUID-vkCmdBuildAccelerationStructuresKHR-commandBuffer-cmdpool`：命令池必须支持 `VK_QUEUE_COMPUTE_BIT`；
- `VUID-vkCmdTraceRaysKHR-commandBuffer-cmdpool`：同样要求 `VK_QUEUE_COMPUTE_BIT`。

我们的图形族带 COMPUTE，所以 BLAS/TLAS 构建命令就录在 ② 段，前后各一条屏障（compute 写 → 加速结构构建读；构建写 → compute 读）。把 compute 拆到独立队列做异步是外置拓展 #10 的事，本段不做。

## compute 能用 RT core，是因为 ray query 是 SPIR-V 指令

`VK_KHR_ray_query` 没有一条宿主 `vkCmd*` 命令，它只做两件事：加一个设备 feature（驱动可选能力，`rayQuery`），允许 shader 里出现 `OpRayQueryInitializeKHR`、`OpRayQueryProceedKHR`、`OpRayQueryGetCommittedIntersectionKHR` 等指令。这些指令可以出现在任意 shader 阶段，compute 当然可以。

执行时分工：

- shader（通用计算核）负责生成射线、控制 `Proceed` 循环、读命中结果、插值和着色；
- RT core（固定功能单元）负责 BVH 遍历和 ray-box / ray-triangle 测试，由 shader 发出的 ray query 指令驱动。

2060（Turing）有 RT core，这些指令走硬件。没有 RT 硬件、或设备没启用 feature 时，含 `RayQueryKHR` 的 shader 根本建不了管线——这也是 9.4 不把 ray query 塞进 `gbuffer_composite` 的原因：那会让"RT 关 = 与 9.3 逐位一致"的降级路径失效。

naga 30.0.1 的 WGSL 前端已支持这套语法（`enable wgpu_ray_query;`），SPIR-V 后端会发 `SPV_KHR_ray_query`。9.4.4 要在 build.rs 的校验能力里补 `Capabilities::RAY_QUERY`，否则模块会校验失败。

## 那为什么不直接用 VK_KHR_ray_tracing_pipeline

Vulkan 把"光线追踪"拆成了三个扩展，名字像不等于职责相同：

| 扩展 | 管什么 | 9.4 用不用 |
|---|---|---|
| `VK_KHR_acceleration_structure` | BLAS/TLAS 的创建、构建、设备地址 | 用（数据侧） |
| `VK_KHR_ray_query` | 在任意 shader 里内联打射线 | 用（发射侧） |
| `VK_KHR_ray_tracing_pipeline` | 专用 RT 管线：RayGen/ClosestHit/Miss/AnyHit/Intersection 阶段、`TraceRay`、SBT、`vkCmdTraceRaysKHR` | 不用 |

RT 管线是给"整条光线追踪渲染器"设计的：命中后靠 SBT（shader binding table，命中着色器分发表）把实例、几何、SBT 偏移映射到某段命中着色器，常见做法是按材质或几何分 hit group，还可以递归 `TraceRay`。它确实有批量发射的 API——`vkCmdTraceRaysKHR(width, height, depth)`，一次发 `width × height × depth` 条射线。

对 9.4 不合适的理由有四条：

1. **SBT 解决的是"命中后跳哪段 shader"。** 我们的着色是 bindless 表驱动：把大量资源放进一张大表、shader 用索引取；全场一个 shader，命中后用槽号查对象记录和材质环，自己分发数据。上 SBT 只多一层间接和一套要维护的缓冲。
2. **命中属性仍要回顶点池读。** RT 管线不多给任何取数能力。
3. **管线形态不划算。** GI 的射线本来就是 compute 线程顺手打的二次线；inline 让现有 compute 录制器、屏障、描述符布局全部复用。RT 管线要新增管线类型、SBT 缓冲、shader group handle、命中着色器入口，等于复刻一套材质分发。
4. **使用面不匹配。** Unity SCGI 的 Hardware 后端就是 compute 里的 inline ray query；RT 管线的典型用户是硬件路径追踪器。

将来若要做每材质 hit shader、递归 traceRay、SBT 调度的完整 RT 渲染器，再启用第三个扩展；9.4 的边界里明确不做。

## 比 CPU 快多少：数量级，以及为什么不能只看倍数

先给数量级，都是估算，实测留给 9.4 的 `rt_stats`：

| 实现 | 不相干射线的吞吐量级 |
|---|---|
| CPU 单线程 BVH（Embree 级） | 百万条/秒 |
| CPU 8 线程 | 千万条/秒 |
| 2060 硬件 ray query（incoherent 二次线） | 亿到十亿条/秒 |

粗算一到两个数量级。但对动态场景，真正的差距不是倍数而是可行性：

- 几何和 BVH 常驻显存，不需要每帧把场景重新送进 CPU 缓存；
- 几百万个线程并行，RT core 做固定功能遍历；
- 射线在 GPU 上直接消费，没有"结果回 CPU、CPU 再上传指令"的往返。

2060 是 Turing 第一代 RT core，吞吐不算高；我们的用法又是稀疏 GI 线而不是全屏路径追踪，所以"能打多少条"最终由帧预算反推，而不是先定一个射线数。9.4 的验收会给 `rt_stats` 的射线计数和命中率，9.8 再按实测压预算。

## 相关

- [加速结构讲解：BLAS、TLAS 与两层 BVH](加速结构讲解：BLAS、TLAS 与两层 BVH.md)——dispatch 之前，这些结构是什么。
- [求交器数据讲解：几何复用、射线生成与射线量预算](求交器数据讲解：几何复用、射线生成与射线量预算.md)——一次 dispatch 的线程数怎么定、射线从哪来。
- [求交器模块讲解：接口两面、落点与后端可替换](求交器模块讲解：接口两面、落点与后端可替换.md)——这些命令由哪个模块录制。
- [施工计划：求交器与射线基建](施工计划：求交器与射线基建.md)——§9.4.1/§9.4.5 的设备门与帧内插入点。
