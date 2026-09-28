# 选型记录：着色器源语言 WGSL，HLSL 为备用路线

日期：2026-09-24。性质：选型决策落账，不构成任何施工验收证据。

对应[施工计划](../施工计划：Bindless起步五段拆解.md) §编译选型与[本段面板](README.md)前置闸门中的 naga 小样例条目。

## 起因

此前 frenderer 时期的习惯是 HLSL → SPIR-V，本段开工前重提一次：WGSL 是否必要、是否值得换回 HLSL。

## 结论

**维持 WGSL → SPIR-V（naga 30.0.1，features `wgsl-in` / `spv-out`）；HLSL/DXC 降为备用路线。**

WGSL 本身不是必要条件：Vulkan 只消费 SPIR-V，源语言是纯选型问题。维持 WGSL 是按本项目约束算出的收益结论，不是语言优劣裁决。

## 事实核查（2026-09-24，本机证据）

1. naga 30.0.1 **没有 HLSL 输入前端**。输入侧只有 `wgsl-in`、`glsl-in`、`spv-in`；`hlsl-out` 是输出方向。证据：本地 registry `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/naga-30.0.1/Cargo.toml` 的 feature 表。"HLSL → SPIR-V" 走不通 naga，选 HLSL 必须整条工具链换成 DXC 或 glslang。
2. Vulkan SDK 1.4.357.0 自带 `dxc.exe` 与 `glslang.exe`。证据：`C:/VulkanSDK/1.4.357.0/Bin`。备用路线的工具本机已就位，切换不产生额外安装成本。

## 维持 WGSL 的理由

- **工具链治理一致性**：选 naga 是为了纯 Rust 依赖、build.rs 内编译、锁版本锁行为。DXC 锁的是 SDK 版本，SDK 升级则 dxc 随之漂移，与"锁定 naga 30.0.1 锁行为"的既有思路相反。
- **Bevy 生态是 WGSL**：view 布局、矩阵约定、prepass/材质参考代码均为 WGSL；用 HLSL 等于每次参考 Bevy 源码都手工翻译，且升级后需再同步。
- **体量与成本**：本段 shader 仅几十行，语言熟练度收益可忽略；实际工作量在 binding 布局、capability 与 SPIR-V 字段偏移核对，这部分两种语言完全一致。HLSL 的 `register`/`binding` 概念映射到 `@group`/`@binding`，迁移成本预计半天。

## 备用路线：HLSL → DXC → SPIR-V

触发条件（满足其一才考虑切换）：

- 小样例验证中 naga 的 `enable wgpu_binding_array;` 扩展或 `var<immediate>` 地址空间支持卡住，且无可行替代写法。
- 出现 DXC 能表达而 naga 无法表达的必需特性。

切换时的治理要求：

- 选型变更落账进施工计划与本面板，注明触发证据，"选型不变"的旧冻结同时解除。
- 重跑小样例验证：数组类型、NonUniform（HLSL 写法为 `NonUniformResourceIndex()`）、所需 capability、入口与字段偏移，过 `spirv-val`；设备支持、编译成功、SPIR-V 合法、最终采样正确四个验收点不因工具链更换而豁免。

## 边界

本记录只冻结源语言选型。前置闸门中小样例验证**已于 2026-09-28 执行**（见[前置闸门施工记录](前置闸门施工记录：naga小样例与set-binding表冻结.md)）：WGSL 接口全链能表达能编译，但 naga 原生 runtime 数组输出缺 `RuntimeDescriptorArray` capability（VUID-04680），capability 补丁后过 spirv-val——**DXC 切换条件未命中**（naga 未卡住，补丁器即"可行替代写法"）；本记录的源语言冻结继续有效。**路线已定 runtime+补丁器（2026-09-28 用户拍板，见施工记录 §4）**。
