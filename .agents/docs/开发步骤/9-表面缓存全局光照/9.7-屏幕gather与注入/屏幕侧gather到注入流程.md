# 屏幕侧 gather 到注入：从一次相机变化到屏幕上的间接光

> 本段（9.7）引入了新的数据流：patch 表 → 屏幕 1/4 目标 → 全分辨率辐照度 → 合成注入。
> 这篇走读面向下一个读代码的人：一次改动/一帧里这些数据各在哪、谁写谁读、出错会怎样。
> 验收数字与判定线归[9.7.7 施工记录](9.7.7-场景验收与调档.md)，这里只讲结构。
> 生成日期 2026-11-24。

## 一、全景：四级账面

| 级 | 内容 | 谁写 | 谁读 |
|---|---|---|---|
| **世界缓存**（9.5/9.6） | `patch_irradiance0/1`（RGBL1，物理单位）、`patch_statistics`（rank/update_count/variance）、`cell_patch`（cell→patch）、`filtered_irradiance0/1`（9.7.2/9.7.3 的滤波产物） | estimation（9.6）、空间滤波（9.7.2 写 A）、时域滤波（9.7.3 写 B） | 屏幕查表（9.7.4）只读 |
| **1/4 屏幕目标**（9.7.4/9.7.5） | `scgi_l0`（RGBA16F：L0 + 覆盖率 a）、`scgi_l1`（RGBA8：三轴方向比值），各双缓冲（P0/P1） | 查表写 P0；去噪第 k 遍读一组写另一组 | 去噪、上采样 |
| **全分辨率辐照度**（9.7.5） | `scgi_irradiance`（RGBA16F：E(n) + 覆盖率 a） | 双边上采样 | 合成 pass（9.7.6） |
| **开关与参数** | 帧 UBO `scgi.x`（注入开关）、GI UBO（Volume/级联/偏移）、`GiSettings`（滤波/去噪/样本数/半径） | 宿主每帧写 | 各 shader |

## 二、一帧的走读（④.5 段与 ⑤ 段）

1. **帧首 `ensure`**：`ScgiTargets::ensure` 比较 1/4 尺寸，变了就重建两张 1/4 图与全分辨率图，
   并把它们一次性从 `UNDEFINED` 迁到 `GENERAL`（新图内容未定义，但布局合法——合成 pass 每帧都绑
   全分辨率那张，连 GI 关掉的帧也要合法）。**布局与描述符池只建一次**，resize 只换附件。
2. **GI 主链**（`settings.enabled` 门控）：滚动 → 淘汰 → 整理 → 播种 → 自发光表 → 光源清单 →
   estimation → bounce 分配。产物是本帧的 patch 辐照度（奇偶决定写 `patch_irradiance0` 还是 `1`）。
3. **屏幕链入口**（`screen_chain = estimation 可用 && settings.scgi`）：为 false 时**一条都不录**，
   于是"`scgi = 0` = 基线且零新增命令"成立。
4. **空间滤波**（9.7.2）：每活跃 patch 在切平面投 4 个抖动样本，查邻居 patch 本帧的辐照度，
   权重 `1 − |jitter|/√2` 加自身权重 1，写 `filtered_irradiance0`。查不到邻居就原样拷贝，
   所以"关开关"与"开但无邻居"给出同一份 A。
5. **时域滤波**（9.7.3）：`B = mix(A, B_prev, w)`，`w = lerp(0.9, 0.95, smoothing)`；
   `update_count ≤ 1` 的 patch 直取 A（那个槽里是上一条 patch 的残留）。
6. **1/4 查表**（9.7.4）：每线程一个 1/4 像素 = 全分辨率 4×4 块；按 9.7.1 的网格口径在该块内取
   三个样本点（本帧播种点 / 对角 / 块心），各自反投影 + 取法线 + `lookup_cell_key` 定位 →
   `cell_patch` → `filtered_irradiance1`。中心样本权重 0.01 且不查 rank（只回答"这里有没有缓存"），
   其余只收 rank 0。加权合成后写 `scgi_l0`（L0 + a）与 `scgi_l1`（三轴比值）。
7. **去噪**（9.7.5a）：0～N 遍 3×3，步长 `{1,2,2,3}[k%4]`；权重 = 高斯 × 法线点积 8 次方 ×
   平面距离衰减，跨边不混。每遍换一份描述符集（`set` / `set_reverse`）实现 A↔B ping-pong，
   同一 dispatch 永不边写边读。
8. **上采样**（9.7.5b）：全分辨率逐像素，4 邻居双边加权（双线性 × 法线 × 平面距离 + 0.01 兜底），
   解码 L1 三轴比值乘回 L0 颜色，按**全分辨率着色法线**求 `E(n)` 写 `scgi_irradiance`，
   覆盖率取四邻居最大值。
9. **合成**（9.7.6）：`MODE_SHADING` 路径里，`frame.scgi.x != 0 && ibl_enabled != 0` 时读
   `scgi_irradiance`（第 6 集恒绑，关态不读），按 `a` 与 IBL 漫反射项 `mix`；高光不参与。
   层叠顺序 `sky → ibl → scgi`：IBL 关掉时整条注入不生效。

## 三、新文件分工（一层一句话）

| 文件 | 职责 |
|---|---|
| `shaders/common/gi_lookup.wgsl` | 只读查表链（世界位置+法线 → cell）+ 1/4 网格口径，9.7.1 抽出 |
| `shaders/common/gi_compression.wgsl` | L1 三轴比值的编解码 + `scgi_evaluate` |
| `shaders/scene/gi/spatial_filter.wgsl` | patch 级切平面滤波（写 A） |
| `shaders/scene/gi/temporal_filter.wgsl` | patch 级时域混合（写 B） |
| `shaders/scene/gi/screen_gather.wgsl` | 1/4 屏幕查表（写 `scgi_l0/l1`） |
| `shaders/scene/gi/denoise.wgsl` | 1/4 目标 3×3 去噪（跨边不混） |
| `shaders/scene/gi/upsample.wgsl` | 全分辨率双边插值 + SH 求值（写 `scgi_irradiance`） |
| `src/render/passes/gi/screen_gather.rs` | 四张目标的所有权、两个方向集 + 采样集、读回通道 |
| `src/render/passes/gi/filtering.rs` | 两条 patch 级滤波的录制束（push 打包） |

## 四、失败与稳态路径

- **缓存没数据 / Volume 外 / 无 rank 0 命中**：gather 写 `a = 0`，合成端 `mix` 逐像素回 IBL 基线；
  不是黑，也不是半亮。
- **`gi.enabled = 0`**：`GiFrameDraw` 为 None，④.5 整段不录，但合成仍绑第 6 集（内容未定义从不被读，
  因为 `scgi.x` 也是 0）——这条是"集恒绑、开关只决定读不读"的直接后果。
- **`ibl_enabled = 0`**：`frame.ibl_enabled` 为 0，注入点不生效（计划 §6 的层叠），画面回步骤 8 的
  `albedo × ambient`。
- **resize**：`ensure` 重建四张图 + 一次布局迁移，描述符集重写、布局不动；实测三次 resize 零 VUID。
- **关态回退**：`scgi` 开→关后画面与"从未开过"的帧**逐字节一致**（`tools/gi_gather_test.py` ⑤B）。

## 五、代码改动简要解析（按提交顺序）

按 `git show --stat` 的实数，本段新增 8 个文件（5 个着色器 + 3 个 Rust）、改动约 20 个文件：

- **新增着色器**：`gi_lookup` / `gi_compression` / `spatial_filter` / `temporal_filter` /
  `screen_gather` / `denoise` / `upsample`（7 个，其中前两个是公共片段）；
- **新增 Rust**：`screen_gather.rs`（目标与三张描述符集 + 读回）、`filtering.rs`（两条滤波的录制束）；
- **扩展**：`surface_cache.rs`（GI 集加 b14/b15 + 两张 filtered 表）、`frame_uniform_table.rs`
  （UBO 336→352 与 `scgi` 槽）、`composite_pipeline.rs`（第 6 集）、`record.rs`（合成第 6 集）、
  `render_features.rs`（`scgi_set`）、`layout_registry.rs`（三条管线 + 两个集）、`init.rs`
  （目标资源与布局登记）、`host/frame/gi.rs`（屏幕链装配与门控）、`gi/mod.rs`（设置与管线）。

## 六、这一段的取舍一句话

- 查表只在 1/4 做，全分辨率只付"一次采样 + 一次 SH 求值"（§0.1 的口径订正）；
- 不做 Chebyshev 软可见性，靠 rank 0 过滤 + 法线/平面距离权重（§0.2）；
- L1 压成三轴标量比值、颜色跟随 L0（RGBA8 装不下三轴×三色）——代价是间接光色向性被抹平；
- 屏幕链整体由 `scgi` 一个开关把关，关态零新增命令。
