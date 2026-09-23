# 施工 3.2：buffer 侧上传——顶点/索引进 GPU 大池

对应 [施工计划 §3](../施工计划：Bindless起步五段拆解.md)。本文件夹存放本段施工的讲解与记录文档。

**目的**：数据从 `Assets<Mesh>` 进 GPU 大池；合批上传 + 拷贝队列起步（路线图原话），超越 frenderer"每 primitive 两次 wait_idle"。

**状态：🚧 施工中（3.2.1 已完成）**

## 任务清单

- [x] **3.2.1 Context 扩展**：寻找 transfer 专用队列族（`QUEUE_TRANSFER` 且无 GRAPHICS；没有则退回 graphics 队列合批并记录取舍）；设备创建开 `Vulkan12Features.timelineSemaphore`；
- [ ] **3.2.2 新模块 `resources.rs`**：内存类型选择 helper（memory type index 查询）、顶点/索引池（DEVICE_LOCAL 大池 + bump 偏移分配）、staging 池（HOST_VISIBLE）；
- [ ] **3.2.3 顶点交错重排**：bevy SoA 分列（POSITION/NORMAL/UV_0）→ 交错 `pos+normal+uv`（32B）+ U16/U32 索引，进 pending 队列；
- [ ] **3.2.4 `flush_uploads`**：一次 flush = 一个 staging buffer + 一个提交 + timeline 信号量置位；draw_frame 渲染提交等 timeline（dst stage = `VERTEX_INPUT`）。

## 验证（本段完成标准）

1. 上传字节日志与 Assets 侧统计一致；
2. RenderDoc 确认池 buffer 就位、数据正确；
3. 连续加载全程零 `device_wait_idle`（对比 frenderer 基准——它每 primitive 两次）。

## 材料清单

- [3.2.1-Context扩展：transfer队列族与timeline信号量开关.md](3.2.1-Context扩展：transfer队列族与timeline信号量开关.md)（2026-09-23：RTX 2060 命中专用 transfer 族 1，退回取舍已记；同日 1.3 基线定案与校验复核，§3 含 timeline 机制讲解）
- timeline 校验探针：`ash_renderer/examples/timeline_probe.rs`（`cargo run --example timeline_probe` 可复跑 A/B 实测）

## 待决问题

- 池增长策略：静态场景按首 flush 需求 + 余量一次到位、不足时重建（wait_idle）——重建策略的细节施工中定，正式的增量/流送是 步骤 4+ 的事。
