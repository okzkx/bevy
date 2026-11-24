# BLAS 与 TLAS：两层索引，几何留在自己的缓冲里

相机在 Sponza 中庭打出一条射线，它要知道自己撞上了哪根柱子、柱子的哪个三角形。场景里几百万个三角形，逐个试一遍不可行，GPU 先查一棵按空间分组的树。Vulkan 把这棵树叫**加速结构**（acceleration structure），并且分成两层：BLAS 负责一份网格内部的三角形，TLAS 负责这份网格在场景里的每次摆放。

先把结论放在这里：**加速结构是索引，不是几何副本；命中得到的只是三角形编号，顶点属性仍要回我们自己的顶点池取。**

## BLAS 的全名是 Bottom-Level Acceleration Structure

一个 BLAS 对应一份网格几何。构建时（`vkCmdBuildAccelerationStructuresKHR`）我们给它三样东西：顶点缓冲的设备地址、索引缓冲的设备地址、以及格式描述（三角形列表、顶点格式、步长、最大顶点号、索引类型）。驱动根据这些输入生成加速结构。常见实现是包围体层次（BVH，bounding volume hierarchy）：节点是包住一组三角形的轴对齐包围盒（AABB），叶子指向三角形；但规范只把它定义为不透明结构，内部布局由驱动决定。

不透明意味着应用不能读它，也不能假设它复制了顶点或没复制。能依赖的只有两件事：

- 构建时的输入格式与地址是我们给的；
- 命中后能拿到这个几何内的三角形编号（`primitive_index`），拿不到顶点。

因此 9.4 计划让构建输入直接指 `MeshPool` 的区间：32 字节交错的顶点（位置/法线/UV）和 U32 索引，不复制第二份。同一份池同时服务两个消费者——BLAS 构建时把它当构建输入读，命中重建时把它当 storage buffer（着色器可读的显存缓冲）读。

## 一条射线撞上物体，数据走的是 TLAS → 实例 → BLAS → 三角形

拿中庭柱子举例，实例槽 42、命中柱子网格的第 12345 个三角形：

1. 射线在世界空间，先过 TLAS。TLAS 的包围盒包住的是"实例"，测试时用实例记录里的 3×4 变换。
2. 命中实例后，ray query 给出 `instance_custom_data = 42`（我们写进去的槽号）和 `object_to_world`（这条实例的变换）。
3. 用 `accelerationStructureReference` 找到该实例引用的 BLAS，在模型局部空间遍历柱子网格的树。
4. 叶子给出 `primitive_index = 12345`、`geometry_index = 0`（9.4 一网格一几何）；`barycentrics` 是该三角形内的重心坐标，第三个分量是 `1 - x - y`。
5. 读对象记录 42 得 `first_index`、`vertex_base`、`material_index`。索引池取 `[first_index + 3 × primitive + k]` 得三个网格局部顶点号，每个都加 `vertex_base` 得池内顶点号。
6. 读三个 32 字节顶点，用重心坐标插值位置/法线/UV；再用 `object_to_world` 变到世界空间。材质索引直接取记录字段。

这条链里没有一步需要读 BLAS 内部。BLAS 只回答"第几个三角形"，属性全在池里。9.4 的施工计划把这条链叫命中重建，只在本段打通并由调试图取证；真正消费它的"命中点取光"是 9.6。

## TLAS 的叶子是实例，每条实例 64 字节

`VkAccelerationStructureInstanceKHR` 的字段：

| 字段 | 位数 / 类型 | 含义 |
|---|---|---|
| `transform` | 12 × f32 | 3×4 行主序，模型 → 世界 |
| `instanceCustomIndex` | 24 | 我们自己的实例槽号 |
| `mask` | 8 | 与射线的 cull mask 按位与 |
| `instanceShaderBindingTableRecordOffset` | 24 | 命中着色器在 SBT（shader binding table，命中着色器分发表）里的偏移；inline ray query 不用 |
| `flags` | 8 | 朝向剔除、不透明等标志 |
| `accelerationStructureReference` | u64 | BLAS 的设备地址；允许写 0 |

三个要点：

- **变换只存在于 TLAS。** BLAS 永远在模型局部空间，同一份 BLAS 可以被任意多个不同变换的实例引用。实例动了只改这条 64 字节记录，BLAS 不动。9.4 计划把实例 flags 置 `TRIANGLE_FACING_CULL_DISABLE`，让几何双面都参与 GI。
- **命中侧回槽号靠 `instanceCustomIndex`。** 对象表容量 2^17，24 位绰绰有余；WGSL 里这个成员叫 `instance_custom_data`。
- **引用写 0 是合法的**（VUID-12281）。流送缺席、BLAS 还没建好的实例写 0，射线自然不命中，不需要造一个退化 BLAS。

## 为什么非要分两层：实例复用与更新粒度

假设场景里 1000 把同款椅子。一份椅子网格的 BLAS 可能几 MB，1000 条 TLAS 实例是 64 KB——差别在"几何建一次、摆放每帧写"。

成本也不对称：BLAS 构建要读所有顶点、算整棵树，贵；TLAS 构建只读实例数组、按实例包围盒建上层树，便宜。于是动态场景的常规做法是：

- 物体变换变 → 只重建 TLAS；
- 网格内容变或驻留状态变 → 重建/销毁 BLAS。

9.4 的计划把这条规则拆成两套生命周期：

| 层 | 何时建 | 何时销毁 | 依据 |
|---|---|---|---|
| BLAS | mesh 资产 commit（LOD 组取最粗已驻留变体） | replace 换内容、retire 退场 | 施工计划 §0.2 |
| TLAS | 每帧全量重建 | 容量不足时重建 | 施工计划 §0.3 |

TLAS 每帧从在场环（记录本帧可绘实例槽号的列表）和模型环（每槽的模型矩阵）生成实例数组，实例数就是本帧 `indirect_count`，不需要 GPU→CPU 回读。TLAS 和实例数组按帧槽各一份：同一份数据按在飞帧数（已提交但还没执行完的帧）各存一套，避免一帧构建写坏另一帧正在查的结构。

## 容易混的四件事

1. **BLAS 不是顶点副本。** 规范不承诺内部存了什么；不要映射、不要读、不要假设顶点位置能从 BLAS 取。顶点属性一律回池。
2. **实例变换不在 BLAS 里。** 同一份 BLAS 被不同实例引用时，各自的世界位置由各自的 TLAS 实例记录决定。
3. **`instance_index` 不是槽号。** `instance_index` 是本帧实例数组的下标（构建顺序），`instance_custom_data` 才是我们写的槽号；9.4 里两者通常不相等。
4. **TLAS 不是把场景合成一个 mesh。** 它只有实例记录，没有三角形；几何始终在 BLAS 指向的网格缓冲区里。

## 命中重建为什么不用 position fetch

`VK_KHR_ray_tracing_position_fetch` 能让命中拿到三个顶点位置，但只限位置——法线和 UV 没有；而且 WGSL 没有指针，shader 里也没法按设备地址自己取顶点。9.4 计划统一从池读：对象记录给区间，索引池给三角形，顶点池给属性。设备地址（BDA，buffer device address）只出现在宿主侧构建 BLAS/TLAS 时，shader 里不出现。

命中属性读池这件事与 Unity 的 GeometryPool 同路：统一从一份常驻几何里取，不为每种后端准备两套副本。9.4 的实施方案见[施工计划 §0.4](施工计划：求交器与射线基建.md)。

## 相关

- [求交器执行讲解：dispatch、队列、RT Core 与 API 选型](求交器执行讲解：dispatch、队列、RT Core 与 API 选型.md)——BLAS/TLAS 由谁、在什么队列上构建。
- [求交器数据讲解：几何复用、射线生成与射线量预算](求交器数据讲解：几何复用、射线生成与射线量预算.md)——这些结构在显存里各占多少、射线从哪来。
- [求交器模块讲解：接口两面、落点与后端可替换](求交器模块讲解：接口两面、落点与后端可替换.md)——这些结构归哪个模块管。
- [施工计划：求交器与射线基建](施工计划：求交器与射线基建.md)——9.4 的裁决与任务表（§0.2/§0.3/§0.4）。
