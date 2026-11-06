# Timeline 信号量：值语义与上传票据

timeline 信号量把 binary 的"亮/不亮"一位升级成 **64 位单调递增的计数器**：signal 时指定值（必须大于当前值），wait 时等值（到达即通过）。它不是"为了 CPU 回读"设计的——CPU 可读只是值语义的免费副产品；真正的卖点是**一根信号量上用值序列表达进度**，让"等一个值"等于"等一个前缀"。闸门选型与三件套分工见《[同步模型](同步模型.md)》与《[信号量设计缘由](信号量设计缘由.md)》，本篇讲 timeline 这一档的机制、必要性论证与工程票据的完整生命周期。

## 0. 判定线

- **值由 CPU 分配、随提交参数下发，GPU 只兑现不递增。**每批提交前 CPU 取号，把号显式写进 `signal_semaphore_values`；GPU 收到提交后把计数器推到该声明值，不做任何自动 +1。
- **timeline 管的是"等的人什么时候被放行"，不管"干的人什么时候开工"。**同队列批次按提交顺序被队列消费（批间无隐含同步、内部工作可重叠），signal 是进度报告不是发令枪；被值拦住的只有跨队列的等待方。
- **等值 N ⇔ 前 N 批全部完成。**单调值的前缀语义是 binary 无论多少只都表达不了的断言——这是 timeline 不可替代性的来源。
- **递接两端有一方要 CPU 观测，才从信号量升到 timeline。**两端都在 GPU 就用 binary；只等 CPU 就用 fence；都要，且要区分"第几批"，用 timeline。

## 1. 机制：binary 信号量加一个单调计数器

- **创建带初值**：`SemaphoreTypeCreateInfo { TIMELINE, initial_value: 0 }`；
- **signal 必须带值**，且严格大于当前值（驱动执法，违者 VUID）；
- **wait 也带值**："等到计数器 ≥ v 为止"，到达 v 隐含所有 signal 值 ≤ v 的提交都已执行完；
- **CPU 三个姿势**：`vkGetSemaphoreCounterValue` 读值（读的是驱动维护的计数器，不冲刷流水线、不拷数据、纳秒级——不是显存回读）、`vkWaitSemaphores` 阻塞等值、`vkSignalSemaphore` host 发令。

心智模型：binary 是作废即焚的传票，timeline 是**取号机 + 叫号屏**——每个批次取一个新号，提交时声明"完成后叫我的号"；等待方拿着号等叫，CPU 还能随时看屏。

## 2. 必要性：反事实两方案

把 timeline 拿掉，用 binary 或 fence 重写上传链，各缺一块：

**每批一只 binary**：图形帧要等"备料前所有批次"，等待三元组变成逐帧不同长度的 `Vec<vk::Semaphore>`；binary 不可复用（signal 后必须 wait 掉），两帧在飞 × 动态批数要建信号量池按帧槽回收；CPU 想查"批 3 完成没有"无门，槽位复用还得每批再配一只 fence——对象 2N，账本两套。

**改用 fence**：fence 挂不进 `queue_submit` 的 wait（信号量的专利），图形帧想等上传只能 CPU 先 `waitForFence` 阻塞——上传与渲染串行化，多帧在飞报废，transfer 队列失去存在意义。

timeline 的两笔账：

```text
// 等待列表恒定 1 项，"等最新进度"是一个数字
semaphores: [ticket_semaphore], values: [snapshot]   // snapshot = 备料时刻的最高号
```

对象账：1 根信号量走完全程，批次数任意；语义账：等值 = 等前缀。**批次数运行期动态、完成通知同时被 GPU 侧与 CPU 侧消费、前缀语义**三个条件全中，才非 timeline 不可。

与 barrier 的作用域边界另立一条：barrier 活在**一条 command buffer 内部**（不是"一次 submit 内"——一次 submit 可含多个 cb，cb 之间只有提交序、无隐含依赖，屏障插不进去）。跨提交/跨队列只有信号量能表达；跨队列所有权屏障（release/acquire）也只是登记换手，真正让对面提交等住的仍是信号量。

## 3. 工程落点：上传票据的完整生命周期

全工程一根 `uploader.ticket_semaphore`（timeline 型，初值 0），四处落点：

| 环节 | 代码 | 动作 |
|---|---|---|
| 创建 | `uploader.rs` | `TIMELINE` + `initial_value: 0` |
| 取号+发出 | `uploader.rs` `submit_batch` | CPU 取 `next_ticket`，写 staging、录拷贝，transfer 队列 signal 该值，不挂 fence；提交成功才登记 `slot.in_flight` |
| 消费①图形帧 | `frame_draw.rs` + `frame.rs` | 备料点 `UploadWait::snapshot` 记 `last_issued_ticket`，帧尾提交 `ALL_COMMANDS` 阶段等该值，CPU 不阻塞 |
| 消费②CPU | `uploader.rs` `wait_until` | 先 `get_semaphore_counter_value` 查值（快路径零开销），未完成才 `vkWaitSemaphores` 阻塞（5s 超时） |
| 消费③mip 续批 | `uploader/image.rs` | 图形队列 wait T → signal T+1，同一根线链式接力 |

一帧的票据流转：

```text
CPU:  submit_batch(批A) ── submit_batch(批B) ── snapshot(=B) ── 组帧 ── queue_submit(帧)
                                                        │                  │
transfer队列:  [拷贝A] signal 1                          │                  │
                       [拷贝B] signal 2                  │                  │
                                                              ▼            ▼ ALL_COMMANDS 等 2
graphics队列:            [mip blit] wait 2→signal 3（若有链式图像）    [整帧渲染] ← image_available

CPU(择机):   completed_ticket() 查值 → staging 槽可复用判定
```

关键在 `snapshot(=B)`：它把"图形帧该等多远"从每帧流转的动态事实压缩成一个 64 位数字，随 DrawInput 走到帧尾提交才真正挂上等待——**等待条件在备料时刻定格**，之后再有新批次发出也不影响本帧。

## 4. staging 双槽：一根信号量管多个槽

信号量的只数与 staging 槽数无关。`Uploader` 全工程只有一份（`init.rs` 构造，槽数写死 2、每槽 staging 初始 1MB 按需扩容不回缩）；每个 `RingSlot` 自带 staging、transfer 命令缓冲、blit 命令缓冲和 `in_flight`（该槽最后一次提交拿到的号）。槽按 `cursor` 轮换，与帧槽的"2"互不相干——上传槽跟批次走，帧槽跟帧走。

**复用只等本槽上一批。**`submit_batch` 选中槽后，若 `in_flight` 号大于已完成值，就 `wait_until` 该号——因为 CPU 马上要覆写这块映射内存，上一批的拷贝可能还在读。号 N 的批次要等的是号 N−2（同槽上一次），不是 N−1：

| 批次 | 号 | 槽 | 提交前 |
|---|---|---|---|
| A | 1 | 0 | 不等（槽空） |
| B | 2 | 1 | 不等（槽空），A 可能仍在跑 |
| C | 3 | 0 | 等号 1；B 在不在跑无关 |

所以 transfer 队列上最多两批同时在飞，第三批才可能卡 CPU。加载期日志 `staging 槽位 … 复用:等上批票据` 频繁出现，就是槽数不够的信号；调大槽数不用动信号量。每槽记自己的号、共用一个计数器比较，正是 §0 前缀语义的用法。

**同队列：后提交的批领不到先完成的功劳。**两槽提交到同一 transfer 队列，signal 的第一同步作用域包含同队列提交序在前的全部命令。设槽 0 号 1、槽 1 号 2：拷贝可以重叠甚至槽 1 先跑完，但"推到 2"要等号 1 那批也完成——计数器不可能停在"2 已到、1 未完"。代价是复用槽 1 时可能连带多等槽 0；语义正是前缀，安全。

**跨队列：续批让结算顺序失守（已修）。**带 mip 链的批次两跳：transfer signal T → graphics 等 T、blit 后 signal T+1，下一批 transfer 拿 T+2。同队列结算序只在队列内成立；T+2 在 transfer、T+1 在 graphics（后面还压着整帧渲染），若下一批 transfer 不等续批，就可能 T+2 先兑现：

```text
transfer:  [批A] signal T ── [批B] signal T+2        ← 计数器先跳到 T+2
graphics:        wait T ──(排在帧后)── [blit] signal T+1   ← 比当前值小
```

两重后果：signal 值不大于当前值违反 timeline VU（未定义行为）；计数器先到 T+2，等 T+1 的一方（图形帧快照、CPU `wait_until`、graphics 回收）被提前放行，读到未生成完的 mip 或提前回收资源。只在 transfer 是独立队列族时触发；同族同队列回退时两次提交同队列按序结算。

修法（`uploader.rs`）：`Uploader` 记 `chain_ticket`（最近一次续批发布的号），下一次 transfer 提交若它未完成，就在 `TRANSFER` 阶段 wait 它；已完成或从无续批则不挂等待。号恢复按 T → T+1 → T+2 兑现。代价只落在"上一批带续批且 blit 未完成"这一种情况，且卡在 GPU、不卡 CPU。判定线：**同一根 timeline 被两条队列 signal 时，提交顺序不等于兑现顺序，必须用 wait 把两条队列的 signal 串回单调序。**

## 5. 工程钉子

- **0 号哨兵**：初值 0 + `next_ticket` 从 1 起，票 0 = 从未上传；消费侧判 0 跳过，两头一起防 VUID-03242（禁止等未 signal 过的值，等 0 要么立即通过要么非法）。
- **快照在 overlay 图集整传之后**：图集批可能多发一张号，拍早了本帧 UI 贴图不在等待值里，图形队列读到未完成的 transfer 写。
- **CPU 登记在提交成功之后**：`slot.in_flight`/`next_ticket` 的推进以 transfer `queue_submit` 成功为前提，提交失败不产生幽灵票据。
- **ALL_COMMANDS 等 + 内存可见性搭车**：transfer 写的消费方散布在各着色段，逐个挑阶段不值得；信号量内存语义隐含全量，CONCURRENT 共享下没有所有权屏障，跨队列可见性全由这一次等待兜底。
- **CPU 可读不是回读**：计数器是驱动维护的数值，读取零成本；本工程 CPU 读只在槽位复用与退出排空两处，主路径等待全在 GPU 端。

## 自测

1. 为什么"等值 N"能推出"前 N 批都完成"？若两批的 signal 值相同会怎样？
2. binary + fence 组合补齐上传链，各缺哪一块？fence 为什么救不了图形帧的 GPU 侧等待？
3. 快照若拍在图集整传之前，症状是什么？为什么票 0 不能进等待三元组？
4. barrier 为什么做不了"图形帧等 transfer 提交"？跨队列所有权屏障例外在哪？
5. 号 3 的批次复用槽 0 时等的是哪个号？为什么槽 1 先跑完，计数器也不会先到 2？
6. mip 续批不串回单调序，会出现哪两重后果？为什么同族同队列时不触发？

## 引用

- 《[同步模型](同步模型.md)》——三闸门作用域判定线的正本
- 《[信号量设计缘由](信号量设计缘由.md)》——三件套分工、全工程三只信号量清点
- 《[多帧在飞竞争](多帧在飞竞争.md)》——票据闸门在四竞争对里的位置
- `ash_renderer/src/render/core/device/transfer/uploader.rs`——取号、发出、CPU 查值/等待
- `ash_renderer/src/render/frame/frame_draw.rs`——`UploadWait` 快照与图形帧提交等待
- `ash_renderer/src/render/core/device/transfer/uploader/image.rs`——mip 续批链式接力
