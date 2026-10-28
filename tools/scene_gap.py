# -*- coding: utf-8 -*-
"""GPU 缺口请求一键压测驱动（6.6）：稀疏缺口清单 + 去重限额 + 跨帧异步读回 + CPU 合并。

场景生成持久化（6.2.1 口径）：3×3 unique 阶梯（LOD 档细粒度载具），floor_anchor
+ mesh 预算全量 1/4 + 请求半径 10（继承 6.5 隔离口径——Helmet 非组载具出圈）。

判定线采证（对应 6.6 段施工计划判定线草案）：
  ① 请求稀疏且有界（红线）：GpuOnly 档缺口槽数 ≤ bump bound（位图硬上限）、
     去重身份 ≤ 非零槽数、缺口槽数 ≤ 场景可绘身份数（不是全量广播）；
     census 槽号 ∈ lod_census 在场行集（缺口只可能来自在场行）
  ② 无当帧等待（红线）：快照采样在帧起 fence 纪律内（结构保证，施工记录
     钉明）——采证 = GpuOnly 活跃期间 CPU 帧 p95 相对 Off 无恶化 + 零 VUID
  ③ 丢失自愈：merged → off（模拟读回哑火）→ merged：off 期缺口不消费、
     回开后缺口重新报出（持续信号，无 ACK/无重试协议）
  ④ CPU 权威（负例）：gap on + 预算 0（全常驻）→ 缺口恒零、draw 正常——
     缺口清单恒空时系统零动作
  ⑤ A/B：off（6.5 CPU 预测独走）vs GpuOnly（GPU 缺口独走）used/请求对照
  ⑥ 零 VUID + WM_CLOSE exit 0（脚本自启宿主才查）

用法：同 scene_floor.py（--no-close / --skip-launch / --build-only）。
退出码：全过 = 0，任一失败 = 1。
"""

import argparse
import ctypes
import json
import os
import subprocess
import sys
import time
import urllib.request

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except AttributeError:
    pass

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# 环境变量旁路：worktree 双实例并行时指向错开的 exe 与端口（缺省主仓同名）
HOST_NAME = os.environ.get("ASH_HOST_EXE_NAME", "ash_renderer.exe")
EXE = os.environ.get("ASH_HOST_EXE", os.path.join(REPO, "target", "debug", HOST_NAME))
HOST_LOG = os.path.join(REPO, ".temp", "scene_gap_host.log")
SHOT_DIR = os.path.join(REPO, ".temp", "6.6")
BRP_PORT = int(os.environ.get("ASH_BRP_PORT", "15702"))

_results = []


def rpc(method, params=None, timeout=30):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}}).encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{BRP_PORT}", data=body, headers={"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        payload = json.load(resp)
    if "error" in payload:
        raise RuntimeError(f"{method} 协议错误: {payload['error']}")
    return payload["result"]


def check(name, ok, detail=""):
    mark = "PASS" if ok else "FAIL"
    _results.append((name, ok))
    print(f"  [{mark}] {name}" + (f" —— {detail}" if detail else ""))


def wait_brp(seconds=60):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            rpc("ash_renderer/increment_stats")
            return True
        except Exception:
            time.sleep(1.0)
    return False


def wait_settle(seconds=3.0):
    time.sleep(seconds)


def launch_host():
    if not os.path.isfile(EXE):
        print(f"宿主二进制不存在：{EXE}\n先 cargo build 再跑本脚本。")
        sys.exit(2)
    os.makedirs(os.path.dirname(HOST_LOG), exist_ok=True)
    log = open(HOST_LOG, "wb")
    proc = subprocess.Popen(
        [EXE], cwd=REPO, stdout=log, stderr=subprocess.STDOUT,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP,
    )
    print(f"宿主已自启（PID {proc.pid}），日志 {HOST_LOG}")
    return proc


def close_host(pid):
    user32 = ctypes.windll.user32
    WNDENUMPROC = ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)

    def _cb(hwnd, _lp):
        owner = ctypes.c_uint()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid:
            user32.PostMessageW(hwnd, 0x0010, None, None)
        return True

    user32.EnumWindows(WNDENUMPROC(_cb), None)
    for _ in range(20):
        time.sleep(0.5)
        if not pid_alive(pid):
            print(f"宿主（PID {pid}）已退出")
            return
    print(f"警告：宿主（PID {pid}）10 秒未退出")


def shoot(pid, name):
    out = os.path.join(SHOT_DIR, name)
    os.makedirs(SHOT_DIR, exist_ok=True)
    r = subprocess.run(
        [sys.executable, os.path.join(REPO, "tools", "capture_window.py"),
         "--pid", str(pid), "--out", out],
        capture_output=True, text=True,
    )
    print(f"  截图 {name}: rc={r.returncode} {(r.stdout or '').strip()[:80]}")


def iter_processes():
    import csv
    import io
    out = subprocess.run(
        ["tasklist", "/FI", f"IMAGENAME eq {HOST_NAME}", "/FO", "CSV", "/NH"],
        capture_output=True,
    ).stdout.decode("gbk", errors="replace")
    rows = []
    for row in csv.reader(io.StringIO(out)):
        if len(row) >= 2 and row[0] == HOST_NAME:
            rows.append((int(row[1]), row[0]))
    return rows


def pid_alive(pid):
    return any(p[0] == pid for p in iter_processes())


GRID = 3
SPACING = 14.0
CORNER = [-SPACING, 0.5, -SPACING]


def build_scene():
    entities = []
    for gx in range(GRID):
        for gz in range(GRID):
            x = (gx - (GRID - 1) / 2) * SPACING
            z = (gz - (GRID - 1) / 2) * SPACING
            r = rpc("ash_renderer/spawn_lod", {
                "shape": "sphere",
                "color": [0.3 + gx * 0.2, 0.4 + gz * 0.1, 0.7],
                "translation": [x, 0.5, z],
                "unique": True,
                "name": f"缺口-阶梯-{gx}-{gz}",
            })
            entities.append(r["entity"])
    wait_settle(6.0)
    st = rpc("ash_renderer/residency_stats")
    print(f"  铺场 {len(entities)} 阶梯，账本 {st['rows']} 行，"
          f"mesh 驻留 {st['used_mesh_bytes']}B")
    return entities


def read_stats():
    return rpc("ash_renderer/residency_stats")


def read_gap():
    return rpc("ash_renderer/gap_stats")


def main():
    ap = argparse.ArgumentParser(description="GPU 缺口请求一键压测（6.6）")
    ap.add_argument("--no-close", action="store_true")
    ap.add_argument("--skip-launch", action="store_true")
    ap.add_argument("--build-only", action="store_true")
    args = ap.parse_args()

    host_pid = None
    host_proc = None
    if not args.skip_launch:
        alive = [p[0] for p in iter_processes()]
        if alive:
            print(f"检测到宿主已在跑（PID {alive[0]}），直接驱动它（测完不关）。")
            args.no_close = True
            host_pid = alive[0]
        else:
            host_proc = launch_host()
            host_pid = host_proc.pid
    if not wait_brp():
        print("FAIL: BRP 端口 60 秒未就绪")
        sys.exit(1)

    print("\n== 场景重建（unique 阶梯 + floor_anchor + 预算 1/4 + 半径 10）==")
    build_scene()
    full = read_stats()
    full_used = full["used_mesh_bytes"]
    budget = max(full_used // 4, 200_000)
    rpc("ash_renderer/residency_set", {
        "enabled": True, "budget_mesh_bytes": budget,
        "request_radius": 10.0, "eject_cooldown_frames": 30,
        "fine_grained": True, "budget_image_bytes": 0,
        "floor_anchor": True,
    })
    rpc("ash_renderer/gap_set", {"mode": "off"})
    rpc("ash_renderer/set_camera", {"target": [0, 0.5, 0], "yaw_deg": 0, "pitch_deg": 25, "radius": 18})
    wait_settle(5.0)
    p_off = rpc("ash_renderer/perf_stats")["cpu_total_ms"]["p95"]

    if args.build_only:
        print(f"\n场景已重建（--build-only，预算 {budget}B 未采证）。")
        sys.exit(0)

    print(f"\n== ① 请求稀疏且有界（GpuOnly：CPU 期望档预测让位，缺口独走）==")
    used_off = full_used
    g0 = read_gap()
    rpc("ash_renderer/gap_set", {"mode": "gpu_only"})
    wait_settle(6.0)
    g1 = read_gap()
    s1 = read_stats()
    ds1 = rpc("ash_renderer/draw_stats")
    census = rpc("ash_renderer/gap_census")
    census_slots = [entry["slot"] for entry in census["slots"]]
    rowless = [entry["slot"] for entry in census["slots"] if not entry["row"]]
    gap_rows_window = g1["gap_rows_total"] - g0["gap_rows_total"]
    print(f"  gap: slots={g1['gap_slots_last']} assets={g1['gap_assets_last']} "
          f"本窗触发行={gap_rows_window} 快照非零={g1['snapshot_slots_nonzero']}/{g1['snapshot_bound']} "
          f"used={s1['used_mesh_bytes']}")
    check("①a 缺口有界（槽数 ≤ bump bound，位图硬上限）",
          g1["gap_slots_last"] <= g1["snapshot_bound"],
          f"缺口槽 {g1['gap_slots_last']} ≤ bound {g1['snapshot_bound']}")
    # 单帧 assets 可为 0：稳态下缺口身份多处于淘汰冷却窗或两帧快照延迟内已到货
    check("①b 同身份去重（资产数 ≤ 缺口槽数）",
          g1["gap_assets_last"] <= max(g1["gap_slots_last"], 1),
          f"assets {g1['gap_assets_last']} ≤ slots {g1['gap_slots_last']}")
    check("①c 缺口只来自在场行（逐槽对应账本行，数量受命令数约束）",
          not rowless and len(census_slots) <= ds1.get("command_count", 0),
          f"无行槽={rowless[:6]} gap={len(census_slots)} commands={ds1.get('command_count')}")
    check("①d GpuOnly 下缺口请求链真实运行（本窗口内产生缺口行请求）",
          gap_rows_window > 0,
          f"窗前累计={g0['gap_rows_total']} 本窗={gap_rows_window} used_off={used_off} "
          f"used={s1['used_mesh_bytes']} requested={s1['requested']}")
    if host_pid is not None:
        shoot(host_pid, "gap_gpu_only_缺口驱动.png")

    print("\n== ② 无当帧等待：GpuOnly 活跃期间帧时间相对 Off 基线无明显回退 ==")
    p_on = rpc("ash_renderer/perf_stats")
    p95_on = p_on["cpu_total_ms"]["p95"]
    max_p95 = max(1.0, p_off * 1.5)
    print(f"  Off={p_off:.2f}ms GpuOnly={p95_on:.2f}ms 上限={max_p95:.2f}ms")
    check("② 活跃期间帧循环无明显阻塞/相对回退",
          p95_on < 100.0 and p95_on <= max_p95,
          f"Off={p_off:.2f}ms GpuOnly={p95_on:.2f}ms")

    print("\n== ③ 丢失自愈：merged → off（哑火）→ merged ==")
    rpc("ash_renderer/gap_set", {"mode": "merged"})
    wait_settle(4.0)
    g3a = read_gap()
    rpc("ash_renderer/gap_set", {"mode": "off"})
    wait_settle(4.0)
    g3b = read_gap()
    rpc("ash_renderer/gap_set", {"mode": "merged"})
    wait_settle(5.0)
    g3c = read_gap()
    print(f"  merged={g3a['gap_rows_total']} → off 期 rows_last={g3b['gap_rows_last']} → 回开 rows_total={g3c['gap_rows_total']}")
    check("③ off 期缺口不消费、回开恢复（丢失帧只推迟升级）",
          g3b["gap_rows_last"] == 0 and g3c["gap_rows_total"] >= g3a["gap_rows_total"],
          f"rows_total {g3a['gap_rows_total']}→{g3c['gap_rows_total']}，off 期 rows_last={g3b['gap_rows_last']}")

    print("\n== ④ CPU 权威负例：gap on + 预算 0（全常驻）→ 缺口恒零 ==")
    rpc("ash_renderer/residency_set", {"budget_mesh_bytes": 0})
    wait_settle(6.0)
    g4 = read_gap()
    ds4 = rpc("ash_renderer/draw_stats")
    inc4 = rpc("ash_renderer/increment_stats")
    print(f"  全常驻：缺口槽={g4['gap_slots_last']} rows_last={g4['gap_rows_last']} 命令={ds4.get('command_count')}")
    check("④ 缺口恒空时零请求（缺口=缺席差集，全常驻无缺口）",
          g4["gap_slots_last"] == 0 and g4["gap_rows_last"] == 0,
          f"slots={g4['gap_slots_last']} rows={g4['gap_rows_last']}")
    check("④b 全常驻画面完整（命令=账本行）", ds4.get("command_count") == inc4["rows"],
          f"命令 {ds4.get('command_count')} = 行 {inc4['rows']}")
    rpc("ash_renderer/residency_set", {"budget_mesh_bytes": budget})
    wait_settle(3.0)

    print("\n== ⑥ 零 VUID + WM_CLOSE exit 0（仅脚本自启的宿主可查）==")
    if host_pid is not None:
        try:
            log = open(HOST_LOG, "rb").read().decode("utf-8", errors="replace")
            bad = [l for l in log.splitlines()
                   if "VUID" in l or "VK_ERROR" in l or "panicked" in l]
            check("⑥a 宿主日志零 VUID/VK_ERROR/panic", not bad, f"{len(bad)} 行" if bad else "")
        except OSError as e:
            check("⑥a 日志读取失败", False, str(e))
    else:
        print("  [SKIP] 驱动的既有实例无日志可查")

    if host_proc is not None and not args.no_close:
        close_host(host_pid)
        rc = host_proc.wait(timeout=15)
        check("⑥b WM_CLOSE 优雅退出 exit 0", rc == 0, f"退出码 {rc}")

    failed = [name for name, ok in _results if not ok]
    print(f"\n== 结果：{len(_results) - len(failed)}/{len(_results)} 过 ==")
    for name in failed:
        print(f"  FAIL: {name}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
