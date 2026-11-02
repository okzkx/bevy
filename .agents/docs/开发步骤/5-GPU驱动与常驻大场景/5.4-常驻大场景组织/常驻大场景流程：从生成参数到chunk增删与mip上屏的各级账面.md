# 常驻大场景流程：从生成参数到 chunk 增删与 mip 上屏的各级账面

> 5.4 流程导读（2026-10-19 施工收官随段写）：本段引入三个新件——常驻大场景生成器、
> chunk 身份与整块增删、基础 mip 链（含 graphics 续批）。面向下一个读代码的人写
> "从入口动作到最终落点"的走读；验收结论归[施工记录](5.4.1-5.4.5-常驻大场景组织施工记录：生成器、chunk组织与mip链.md)，不在此重复。

## 0. 新文件分工表（一层一句话）

| 文件 | 职责 |
|---|---|
| `scene/mechanism/generation/scene_gen.rs` | 生成器机制层：批量克隆铺实例（扁平/chunk 化两形态）、SceneChunk 组件、chunk 盘点与整块删除——BRP 与将来 egui 面板共用的可复用实现 |
| `remote/generate.rs` | BRP 协议薄壳：generate_scene / generate_chunks / chunk_census / chunk_despawn 四方法的参数解析与 JSON 读回 |
| `vulkan/images.rs` | ImageSpec 携带全链级数（full_mip_chain）、GpuImage 按格式 blit 能力建图（缺支持降级单级）、采样端 max_lod 放开 |
| `vulkan/uploader.rs` | 上传编排扩形：transfer 批只拷 level 0；**mip 生成续批**（graphics 队列、等票据 → blit → signal 票据 +1） |
| `driver/upload.rs` | flush_uploads 排批侧：staging 只排 level 0 字节、带链计量（mip 链 N 张）进批次日志 |
| `overlay/windows/debug_window.rs` | 调试窗口 chunk 计量行（块数/成员总数） |

既有链路（账本/上传/DrawList/在场环/剔除/indirect）零改动——5.4 的全部内容叠在其上。

## 1. 生成时刻走读：一条 generate-chunks 请求的生命周期

以 `brp.py generate-chunks --chunk-grid 8 --instances-per-chunk 64 --spacing 4 --seed 42 --jitter 0.5`（4096 实例）为例：

1. **BRP handler**（`remote/generate.rs`，RemoteLast 执行）：解析参数 → 机制层
   `generate_chunks`（`scene_gen.rs`）。
2. **机制层铺实体**：逐块（cz×8+cx 编号）spawn 根实体（SceneChunk + Transform@origin
   + Name `chunk{id}`），块内小网格铺 64 个实例（柄克隆 = 共享 6 mesh/6 材质，零新
   资产），`add_child` 挂到根下；位置 = 局部网格 + splitmix64 散列抖动（纯函数，
   同参数重跑同位）。返回读回（块数/实例数/首尾根实体）。
3. **次帧账本对账**（`update_ledger`，PostUpdate 传播后）：4096 个 primitive 的
   Mesh3d 插入 Changed → 入行置脏；GlobalTransform 传播给子树（chunk 根的平移经
   父链到实例）。
4. **flush_uploads**（Last）：4096 脏行盘点 → mesh/材质全驻留命中 → **零资产上传**，
   只有 4096 条 48B 对象记录进一个批次（实测 196608B）→ 实例槽分配（bump 游标）→
   模型环待写标记。
5. **draw_frame**（Last）：DrawList 组装 4102 可绘行 → 材质环全量重写（仍 6 种）+
   在场环全量重写（4096 槽号）→ cull dispatch → build_indirect →
   **一次 `cmd_draw_indexed_indirect`**（commands=4102，draw_calls=1）。

规模无关性：10 万档（5.5）走同一条路，变化的只有参数——账本行走访与材质环全量
重写是 O(N)（5.3 钉明的口径：在账信息组装写侧，如实计量），绘制调用与 push 字节恒定。

## 2. chunk 时刻走读：整块增删的对账链

- **盘点**（`chunk_census`）：查 SceneChunk 组件 + Children 计数——members =
  子树内可渲染 primitive 数，origin/extent 原样报出（生成期账，不按当前位置重算）。
- **整删**（`chunk_despawn --id N`）：按 id 找根 → `try_despawn` 递归删整棵子树 →
  逐 primitive 的 Mesh3d 移除各发一次 Removed → `update_ledger` ①号入口逐行清账
  （实例槽交接 freed_slots）→ flush 消费进对象表停尸房 → 双条件排空归 free list。
  实测：chunk 27（64 成员）→ removed_rows_total 精确 +64、上传字节零增量。
- **重生**：同参数 `generate-chunks` 再铺 → 行重新入账 → 驻留命中 → 又只剩 48B/条
  的记录批次（实测 3072B/64 条）。停尸房里换下的旧槽经排空后由新行复用（槽位闭环）。

## 3. mip 时刻走读：从资产字节到全链可采样

1. **规格映射**（`image_spec`）：核对维度/格式，算全链级数
   `floor(log2(max(w,h)))+1`（512×512 → 10 级）；带链描述符不再拒绝。
2. **建图**（`GpuImage::create`）：查格式 optimal features——三用途之外加查
   `BLIT_SRC|BLIT_DST|SAMPLED_IMAGE_FILTER_LINEAR`，全含则按全链建 mip_levels，
   缺任一降级单级（日志留判定）；view 全链层级。
3. **transfer 批**（transfer 队列）：staging 只排 level 0（w×h×4）→
   `cmd_copy_buffer_to_image` level 0 → 迁出为 blit 源（TRANSFER_DST→TRANSFER_SRC，
   **dstAccess=TRANSFER_READ**——transition 是一次写，要对跨提交读者可见）→ signal
   票据 T。
4. **mip 生成续批**（graphics 队列，`record_blit_continuation`）：等票据 T →
   高层级 UNDEFINED→TRANSFER_DST → 级 i 从级 i-1 blit（LINEAR，2× 平均）→ 逐级
   迁回 TRANSFER_SRC（含末级，链内布局归一）→ 全链收口 SHADER_READ_ONLY
  （dstAccess=SHADER_READ + dstStage=FRAGMENT_SHADER）→ **signal 票据 T+1**。
5. **消费**：发布票据 = T+1，图形提交等它（与单级图同一条执法点，值不同）——
   "链可用"的完成点由续批定义，等待语义不变。

失败路径：transfer 成功后续批失败 → 票据 T 已发但 T+1 永不发布，链停在中间布局
——上传链按 Tier② 致命退出（无重试路径），teardown 排空兜底。稳态：已驻留身份
零上传零续批（票据只在有新资产的批次存在）。

## 4. 与步骤 6 的接口

chunk 身份现在是纯 CPU 侧组织（生成期账），GPU 不知道 chunk——步骤 6 的流送/
Hi-Z 若需要 GPU 侧块身份，从 SceneChunk 组件出发扩对象表/标记环字段即可（5.4
待决 8 的预留口）；生成器规模无关，10 万档压测只换参数。

---

## 附：代码改动简要解析（`git show --stat` 实数，分支 goal/5.4 七笔 feat/chore）

| 提交 | 内容 | 规模 |
|---|---|---|
| 99ae1b77f | 5.4.1 生成器（scene_gen.rs 新建 + BRP generate_scene + brp.py generate） | 7 文件 +246 |
| 5514e86f6 | 5.4.2 chunk 身份与整块增删（SceneChunk/generate_chunks/census/despawn + brp.py 三命令函数） | 7 文件 +341 −8 |
| 52727625f | 5.4.3 mip 链初版（images/uploader/upload 四处联动 + 库测试） | 7 文件 +275 −48 |
| f91f3b1a9 | 5.4.4 egui chunk 计量行 | 2 文件 +25 −2 |
| 5e09639b9 | 5.4.3 施工订正：blit 续批 graphics 队列 + 屏障访问掩码（uploader 重构 + 探针补参） | 5 文件 +318 −154 |
| f51872db5 | 5.4 取景配套（轨道限位/远平面放宽）+ 生成器空参文案订正 | 3 文件 +15 −5 |
| 21157047b | brp.py 三子命令 parser 注册补漏 | 1 文件 +15 |

取舍要点：①生成器放机制层不放业务半边——"铺多少、怎么铺"是参数，场景内容（哪个
primitive 当种子）才是业务；②chunk 用根实体+子树而不是生成器账面——despawn 树
语义免费拿整块清账，账本零改动；③mip 生成续批而不是换整个上传队列——保留
transfer 专用族的架构位置，只在"blit 必须 graphics"这个硬约束处加一条链式提交。
