# 4.1 对象增量——稳定槽账本、变更发现与增量写入

对应[步骤 4 README](../README.md) 与[路线图](../../学习目标实现步骤.md)。主计划=[施工计划：对象增量三线拆解](施工计划：对象增量三线拆解.md)；本 README 是段任务面板（**状态：✅ 全部收官 2026-10-10**——当日立案当日施工）。

## 任务清单（4.1.1~4.1.6，全部收官）

- [x] **4.1.1 前置闸门：pool 帧侧等待**——`FramePool::wait_all_inflight`（等全部帧槽 fence）+ `MeshPool::ensure_capacity` 回调接线（vulkan 层内 pool 不依赖 frames）；upload_probe 组 C 断言"迁移恰等一次、初建不触发"+ 读回一致 + 零 VUID。3.6.3 §3 立账清账。
- [x] **4.1.2 稳定槽账本**：`scene/mechanism/ledger.rs` 的 `InstanceLedger`（BTreeMap 行 + 脏集 + 计量）；collect.rs/CollectedScene 退役，DrawList/上传链/一次性核验全改读账本。
- [x] **4.1.3 变更发现**：`update_ledger`（PostUpdate 传播后）三入口——RemovedComponents（despawn 整树清账免费）/`Or<(Changed<Mesh3d>, Changed<MeshMaterial3d>)>`（覆盖新增+换柄）/`Changed<GlobalTransform>`（set_if_neq 保证静态零触发）；扫描数/处理数分列计量。
- [x] **4.1.4 增量写入**：flush_uploads 吃脏行（全驻留即清标、未到货保持重试），稳态零盘点零上传；首清账日志哨兵（首跑实抓 unreachable 已修）。
- [x] **4.1.5 BRP 变更方法族**：spawn/replace/increment_stats 自研（`remote_objects.rs`，操作本体 `object_ops.rs` 可复用层）；despawn/remove 复用官方 `world.despawn_entity`/`world.remove_components`；`tools/brp.py` 五子命令（合计十六）。
- [x] **4.1.6 验收回归**：五类变更判定线全过（计量+截图读图，见施工记录 §5 总账）、负例三连宿主存活（-23401×2/-23402）、三模式回归、零 VUID、WM_CLOSE exit 0 ×3、clippy 全净、增删后 resize×2 不炸。

## 判定线（计划 §0）收官口径

移动只动一行（T+1/上传 0）；替换与删除不残留（rebound/removed/树清）；新增增量入账（rows+1/零上传）；零变化零上传（稳态 proc=0/dirty=0，扫描数单列）；既有判定线零回退。逐条证据链见[施工记录](4.1.1-4.1.6-对象增量施工记录：稳定槽账本、增量写入与BRP方法族.md) §0/§5。

## 材料清单

- [4.1.1-4.1.6-对象增量施工记录：稳定槽账本、增量写入与BRP方法族](4.1.1-4.1.6-对象增量施工记录：稳定槽账本、增量写入与BRP方法族.md)——段级主产出（2026-10-10）：判定线总账、四问体例逐任务、钉子八枚（despawn→Removed 语义、set_if_neq 同值拦截、DerefMut 借用陷阱、日志哨兵可达性等）。

## 边界对账（计划 §5 兑现情况）

资产内容变化归 4.2、异步到货归 4.3、GPU 资源回收归 4.4、实例数据迁 GPU 归 5.1——本段全部未越界；BRP spawn/replace 即"实体级操作"的 CLI 面，egui 面板将来调 object_ops 同款实现。
