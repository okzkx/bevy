# 10.1 帧耗时先导（任务面板）

> 2026-11-27 开工、同日收官。施工依据：[施工计划：帧耗时先导](施工计划：帧耗时先导.md)（2026-11-27 立案）。
> 调试图计量口径同步：[9.4.7 判定线与计量](../../9-表面缓存全局光照/9.4-求交器与射线基建/9.4.7-判定线与计量：ray_tracing_test十五项与五脚本回归.md)。
> 本目录记录“实际做了什么”；计划文档只流转状态，不回写实现细节。

**一句话**：给每帧无条件跑的光线求交调试图加显式 `debug` 门控（默认 `rays == 0`，按需打开供 `ray_tracing_stats` / `ray_tracing_dump` 取证），并把 CPU 帧准备 `rings` 段按现有执行边界切成四段 stopwatch 读数，为 10.2/10.3 的 A/B 建立 CPU/GPU 分段基线；本段不做 rings 削减、不改渲染结果与画质档位、不碰 SCGI 采样与缓存（9.11）。

**状态：✅ 收官（2026-11-27）**——`ray_tracing_test.py` 20/20、clippy 全净、三 GI 脚本回归通过、零 VUID；调试图默认 `rays=0`，开启后 `rays=1,440,000`；Emerald Square 300 样本 `rings_ms` p50 4.45185（改前 4.26905，+4.28%，落在同噪声带），四段和与 `rings_ms` 最大偏差 5.2e-7 ms。

## 任务与施工记录

| 任务 | 内容 | 记录 |
|---|---|---|
| 10.1.1 | 调试图显式门控：`RayTracingSettings.debug`、`prepare_ray_tracing` 门控、`ray_tracing_set/stats/dump` 契约 | [10.1.1-调试图门控与BRP](10.1.1-调试图门控与BRP.md) |
| 10.1.2 | `rings` 四段打点：stopwatch 四个字段、三个中间收口、`FrameSample` 与 `perf_stats` / `perf_samples` 四键 | [10.1.2-rings四段打点](10.1.2-rings四段打点.md) |
| 10.1.3 | 取证与判定：20 项脚本、manual 曝光 A/B、300 样本、GI 回归、9.4.7 口径同步 | [10.1.3-取证与判定](10.1.3-取证与判定.md) |

## 判定线结果

| 组 | 判定 | 结果 | 证据 |
|---|---|---|---|
| 4.1 | 默认 `debug=false`、`rays==0`；不自动跨帧打开；显式开后 `rays==width*height`、dump 成功；关回 0 | ✅ | `.temp/10.1/ray_tracing_test_final.log`：新进程默认 0，开后 1,440,000，关后 0；两段 dump 拒绝 |
| 4.2 | 四段非负且和≈`rings_ms`（≤5%）；`rings_ms`/`cpu_total` 不增；可归因 | ✅ | 300 样本 p50：4.45185 vs 4.26905（+4.28%）；四段和最大偏差 5.2e-7 ms；`rings_shadow_ms` 83.25% |
| 4.3 | 零 VUID/VK-ERROR、clippy 净、无新依赖、三 GI 脚本全过 | ✅ | `clippy.log` EXIT=0；`gi_test_2.log` 25/25（首跑 24/25 时序抖动，见记录）；estimation 13/13；gather 12/12 |
| 4.4 | `enabled` 默认值不变、`debug` 默认 false 且重启复位、BRP 方法名不变、旧客户端不带 `debug` 行为不变 | ✅ | `ray_tracing_set` 仍为原方法名，`debug` 为 `Option<bool>` 缺省保留；测试 20/20 |

## 代码验证线

- `cargo clippy -p ash_renderer --all-targets -- -D warnings`：EXIT=0（`.temp/10.1/clippy.log`）。
- `python tools/ray_tracing_test.py`：20/20，EXIT=0（`.temp/10.1/ray_tracing_test_final.log`）。
- `gi_test.py`：第二次 25/25（`.temp/10.1/gi_test_2.log`）；第一次 24/25，唯一失败为 ④ 的时序读数，未改代码路径。
- `gi_estimation_test.py` 13/13、`gi_gather_test.py` 12/12。
- 未引入新依赖；未提交 `ash_renderer/assets/bcn_cache` 等运行期缓存。

## 材料与现场

- 段计划：[施工计划：帧耗时先导](施工计划：帧耗时先导.md)。
- 9.4.7 口径同步：[9.4.7 判定线与计量](../../9-表面缓存全局光照/9.4-求交器与射线基建/9.4.7-判定线与计量：ray_tracing_test十五项与五脚本回归.md)。
- 原始证据（不入库，仓库根 `.temp/10.1/`）：
  - `ray_tracing_test_final.log`、`ray_tracing_test_host.log`、`frames/` 三张 PPM；
  - `emerald_perf_samples_300.json`、`emerald_base_perf_samples_300.json`、`src.patch`、`ash_renderer_base.exe`；
  - `clippy.log`、`gi_test.log`、`gi_test_2.log`、`gi_estimation_test.log`、`gi_gather_test.log`；
  - `probe_manual.py`、`probe_debug.py`、`probe_toggle.py`。

## 遗留与评审项

- **R6 归因**：`rings_shadow_ms` 占 83.25%（3.70615 / 4.45185 ms），已满足“单段 >60%”的评审条件；本段不削减、不追加 10.1.4。该段含装配日志、光源 gather、IBL、AABB 与 shadow UBO，不能把 83% 全部算到阴影头上；是否另立削减段由用户拍板。
- **1-LSB 曝光噪声**：auto 眼适应下跨状态稳定帧仍有 1 LSB 差（改前基线即如此）；10.1.3 的 A/B 在 `exposure_set {"mode":"manual"}` 下取证，收尾恢复原模式。这是取证口径补强，不是放宽判定线。
- **首跑抖动**：`gi_test.py` 首跑 ④ 出现 `active 3614→3343`，同二进制重跑 25/25；按测试时序抖动记录，未改代码。
- **读数窗口**：`perf_stats.sections_mean` 是窗口均值，`perf_samples` 是逐帧；判定与归因统一用 300 样本 p50，不用单次 `sections_mean` 代替。
