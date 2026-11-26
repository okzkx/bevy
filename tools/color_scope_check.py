"""颜色校验（9.9）BRP 复核：读 `ash_renderer/color_scope_get` 的 bins 快照，
按施工计划判定线做自洽校验，并可导出逐 bin CSV。

用法:
  python tools/color_scope_check.py                     # 读最近一次样本并校验
  python tools/color_scope_check.py --once              # 先请求单帧重采再读
  python tools/color_scope_check.py --csv .temp/scope.csv       # 导出 luma+parade 逐 bin
  python tools/color_scope_check.py --cbcr-csv .temp/cbcr.csv   # 另导 CbCr 64×64
  python tools/color_scope_check.py --json .temp/scope.json     # 存整份快照

校验项（与计划 §2 判定线同源）:
  1. magic/stride/bins 长度与常量一致（7199 u32、stride 2）；
  2. 每域 `luma 直方图和 + below + above + zero == samples`（计数不漏样本）；
  3. `samples == ceil(w/2) * ceil(h/2)`（stride 2 的采样点总数）；
  4. B 域加权均值与 `avg_log2 + log2(scale)` 差 ≤ 0.15 EV（B 已乘曝光倍数，
     头里的 avg_log2 是乘法之前的测光值，直接比会固定偏 log2(scale)）；
  5. 低于轴比例 > 5% 时告警（计划待决项 2：轴可能不够宽）；
  6. C 域触顶比例只报告，判定线⑬按场景口径另判。

退出码：0 全过；1 有校验失败；2 宿主未起/连接失败。
宿主须先起（cargo run -p ash_renderer），默认 127.0.0.1:15702。
"""

import argparse
import csv
import json
import math
import sys
import time
import urllib.error
import urllib.request

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 15702
EXPECTED_BINS_U32 = 7199
EXPECTED_STRIDE = 2
BINS = 256
AXIS_MIN_EV = -16.0
AXIS_SPAN_EV = 24.0
B_MEAN_TOLERANCE_EV = 0.15
BELOW_WARN_RATIO = 0.05


def rpc(host, port, method, params=None, timeout=15):
    body = {"jsonrpc": "2.0", "method": method, "id": 1}
    if params is not None:
        body["params"] = params
    req = urllib.request.Request(
        f"http://{host}:{port}",
        data=json.dumps(body).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def call(host, port, method, params=None):
    reply = rpc(host, port, method, params)
    if "error" in reply:
        err = reply["error"]
        raise RuntimeError(f"{method} 失败（{err.get('code')}）：{err.get('message')}")
    return reply.get("result")


def fetch_snapshot(host, port, once, wait_s):
    params = {"force": True} if once else {}
    snap = call(host, port, "ash_renderer/color_scope_get", params)
    if not once:
        return snap
    deadline = time.time() + wait_s
    while time.time() < deadline:
        snap = call(host, port, "ash_renderer/color_scope_get")
        if snap.get("magic_ok") and snap.get("frame_age", 99) <= 2:
            return snap
        time.sleep(0.1)
    raise RuntimeError(f"等待新样本超时（{wait_s:.1f}s）：请确认采样开关已开且后链启用")


def bin_center_ev(bin_index):
    return AXIS_MIN_EV + (bin_index + 0.5) * (AXIS_SPAN_EV / BINS)


def check(snap):
    """返回 (errors, warnings)；errors 非空即判失败。"""
    errors = []
    warnings = []

    if not snap.get("magic_ok"):
        errors.append("magic 不匹配：bins 从未被写过头，可能没采过样")
    if snap.get("stride") != EXPECTED_STRIDE:
        errors.append(
            f"stride={snap.get('stride')}，期望 {EXPECTED_STRIDE}"
        )
    if snap.get("bins_u32") != EXPECTED_BINS_U32:
        errors.append(f"bins_u32={snap.get('bins_u32')}，期望 {EXPECTED_BINS_U32}")

    width = int(snap.get("width", 0))
    height = int(snap.get("height", 0))
    expected = (width + 1) // 2 * ((height + 1) // 2)

    for name in ("a", "b", "c"):
        domain = snap["domains"][name]
        counts = domain["counts"]
        samples = counts["samples"]
        if samples == 0:
            errors.append(f"{name} 域样本数为 0（bins 没被写入？）")
            continue
        luma_sum = sum(domain["luma"])
        covered = luma_sum + counts["below"] + counts["above"] + counts["zero"]
        if covered != samples:
            errors.append(
                f"{name} 域样本账不平：luma 和 {luma_sum} + below {counts['below']}"
                f" + above {counts['above']} + zero {counts['zero']} = {covered}，"
                f"samples {samples}"
            )
        if width and height and samples != expected:
            errors.append(
                f"{name} 域采样点数 {samples}，期望 ceil({width}/2)*ceil({height}/2)"
                f" = {expected}"
            )
        below_ratio = domain["ratios"]["below"]
        if below_ratio is not None and below_ratio > BELOW_WARN_RATIO:
            warnings.append(
                f"{name} 域低于轴比例 {below_ratio:.2%} > {BELOW_WARN_RATIO:.0%}"
                "（计划待决项 2：轴下限可能不够宽）"
            )

    mean_b = snap["domains"]["b"]["mean_axis"]
    avg_log2 = snap["exposure_state"][2]
    scale = snap["exposure_state"][1]
    if mean_b is None:
        errors.append("B 域没有可用均值")
    elif scale <= 0.0:
        errors.append(f"B 域期望无法计算：曝光 scale {scale} 非正")
    else:
        expected = avg_log2 + math.log2(scale)
        delta = abs(mean_b - expected)
        if delta > B_MEAN_TOLERANCE_EV:
            errors.append(
                f"B 域均值 {mean_b:.3f} EV 与期望（avg_log2 {avg_log2:.3f}"
                f" + log2(scale {scale:.4f}) = {expected:.3f}）差 {delta:.3f} EV"
                f" > {B_MEAN_TOLERANCE_EV} EV"
            )

    return errors, warnings


def print_report(snap):
    print(
        f"样本帧 {snap['frame_index']}（age {snap['frame_age']}，"
        f"{'stale' if snap['stale'] else 'fresh'}）  "
        f"分辨率 {snap['width']}×{snap['height']}  stride {snap['stride']}  "
        f"bins {snap['bins_u32']} u32"
    )
    exp = snap["exposure_state"]
    print(
        f"曝光快照：adapted_log2 {exp[0]:.3f}  scale {exp[1]:.4f}  "
        f"avg_log2 {exp[2]:.3f}  initialized {exp[3] > 0.5}"
    )
    print(f"开关：enabled {snap['enabled']}  once {snap['once']}  slot {snap['slot']}")
    print()
    header = f"{'域':<3}{'样本':>8}{'低于轴':>9}{'高于轴':>9}{'零':>8}{'触顶':>8}{'轴均值':>11}"
    print(header)
    print("-" * len(header.encode("gbk", errors="replace")))
    for name in ("a", "b", "c"):
        d = snap["domains"][name]
        c = d["counts"]
        r = d["ratios"]
        mean = d["mean_axis"]
        unit = " EV" if d["axis"] == "log2_ev" else ""
        mean_text = "—" if mean is None else f"{mean:+.3f}{unit}"
        print(
            f"{name.upper():<3}{c['samples']:>8}{r['below']:>8.2%}{r['above']:>9.2%}"
            f"{r['zero']:>8.2%}{r['clip']:>8.2%}{mean_text:>11}"
        )


def write_csv(path, snap):
    with open(path, "w", encoding="utf-8", newline="") as fh:
        writer = csv.writer(fh)
        writer.writerow(["domain", "channel", "bin", "axis_value", "count"])
        for name in ("a", "b", "c"):
            d = snap["domains"][name]
            axis = d["axis"]
            for ch, series in enumerate(d["rgb"]):
                for i, count in enumerate(series):
                    value = bin_center_ev(i) if axis == "log2_ev" else (i + 0.5) / BINS
                    writer.writerow([name.upper(), "rgb"[ch], i, f"{value:.6f}", count])
            for i, count in enumerate(d["luma"]):
                value = bin_center_ev(i) if axis == "log2_ev" else (i + 0.5) / BINS
                writer.writerow([name.upper(), "luma", i, f"{value:.6f}", count])


def write_cbcr_csv(path, snap):
    with open(path, "w", encoding="utf-8", newline="") as fh:
        writer = csv.writer(fh)
        writer.writerow(["cb", "cr", "count"])
        for row in range(64):
            for col in range(64):
                writer.writerow([row, col, snap["cbcr"][row * 64 + col]])


def main():
    parser = argparse.ArgumentParser(description="颜色校验 bins 复核（9.9）")
    parser.add_argument("--host", default=DEFAULT_HOST)
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument("--once", action="store_true", help="先请求单帧重采再读")
    parser.add_argument("--wait", type=float, default=5.0, help="等待新样本秒数")
    parser.add_argument("--csv", help="luma/parade 逐 bin CSV 输出路径")
    parser.add_argument("--cbcr-csv", help="CbCr 64×64 CSV 输出路径")
    parser.add_argument("--json", help="整份快照 JSON 输出路径")
    parser.add_argument("--strict", action="store_true", help="把告警也当失败")
    args = parser.parse_args()

    try:
        snap = fetch_snapshot(args.host, args.port, args.once, args.wait)
    except (urllib.error.URLError, ConnectionError) as err:
        print(f"连接 BRP 失败（宿主未起？{args.host}:{args.port}）：{err}", file=sys.stderr)
        return 2
    except RuntimeError as err:
        print(err, file=sys.stderr)
        return 2

    if args.json:
        with open(args.json, "w", encoding="utf-8") as fh:
            json.dump(snap, fh, ensure_ascii=False, indent=2)
    if args.csv:
        write_csv(args.csv, snap)
        print(f"CSV 已写：{args.csv}")
    if args.cbcr_csv:
        if not snap.get("cbcr"):
            print("快照不含 cbcr（请求时 cbcr=false），跳过", file=sys.stderr)
        else:
            write_cbcr_csv(args.cbcr_csv, snap)
            print(f"CbCr CSV 已写：{args.cbcr_csv}")

    print_report(snap)
    errors, warnings = check(snap)
    print()
    for warning in warnings:
        print(f"告警：{warning}")
    if errors:
        for err in errors:
            print(f"失败：{err}", file=sys.stderr)
        return 1
    if warnings and args.strict:
        print("严格模式下告警视为失败", file=sys.stderr)
        return 1
    print("校验通过：账平、采样点数与 stride 一致、B 域均值与测光口径对齐。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
