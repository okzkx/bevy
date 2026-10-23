# -*- coding: utf-8 -*-
"""低保真常驻保底一键压测驱动（6.5）：可绘锚迁移 + 需求档按需上传。

场景生成持久化（6.2.1 口径）：复用 BRP 方法族——3×3 unique 阶梯网格
（LOD 档细粒度载具），mesh 预算压到全量 1/4 迫使高档/基准被淘汰。

判定线采证（对应 6.5 段施工计划判定线草案）：
  ① 缺高档仍有画面（红线）：floor_anchor 开 + 预算压紧 → 保底档全驻留
     （floor_groups_resident = 组数）而基准被淘汰（ejected > 0），全部行
     照常绘制（command_count = 账本行数，远景不消失）
  ② A/B 对照：floor_anchor 关 = 6.4 现状——基准缺席行整行暂缓
     （command_count < 行数，远景消失）；回开恢复全行可画
  ③ 高档按需回流：相机接近角落 → 需求位图点名基准（level 0）→ 基准
     重新驻留（高档按需，不上传非需求档）
  ④ 回退零悬空：保底档引脚/采样全链合法——零 VUID、命令组装正常、
     关键两态截图读图归施工记录（A/B 对照取证）
  ⑤ 关闭沿恢复：管理器关 + 预算 0 + 锚关 → 全行置脏恢复全常驻基线
  ⑥ 零 VUID + WM_CLOSE exit 0（脚本自启宿主才查）

用法：同 scene_fine.py（--no-close / --skip-launch / --build-only）。
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
HOST_LOG = os.path.join(REPO, ".temp", "scene_floor_host.log")
SHOT_DIR = os.path.join(REPO, ".temp", "6.5")
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
    """PrintWindow 截图取证（.temp/6.5/，判定线 ④ 读图输入）。"""
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


GRID = 3        # 3×3 unique 阶梯（9 组 × 4 档 = 36 mesh 身份）
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
                "name": f"保底-阶梯-{gx}-{gz}",
            })
            entities.append(r["entity"])
    wait_settle(6.0)
    st = rpc("ash_renderer/residency_stats")
    print(f"  铺场 {len(entities)} 阶梯，账本 {st['rows']} 行，"
          f"mesh 驻留 {st['used_mesh_bytes']}B / 档位驻留 {st['fine']['lod_levels_resident']}")
    return entities


def read_stats():
    return rpc("ash_renderer/residency_stats")


def draw_count():
    ds = rpc("ash_renderer/draw_stats")
    return ds.get("command_count", -1), ds.get("draw_calls_recorded", -1)


def main():
    ap = argparse.ArgumentParser(description="低保真常驻保底一键压测（6.5）")
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

    print("\n== 场景重建（unique 阶梯网格；Helmet 场景保留=无阶梯载具）==")
    build_scene()
    full = read_stats()
    budget = max(full["used_mesh_bytes"] // 4, 200_000)  # 压到全量 1/4：基准/高档必被淘汰
    rpc("ash_renderer/residency_set", {
        "enabled": True, "budget_mesh_bytes": budget,
        # 半径 10 = 角落阶梯内、Helmet 载具（~15 单位）外——隔离非组资产噪声，
        # ③ 高档回流采证不被 Helmet 的 coarse 请求/淘汰循环污染（诊断实抓）
        "request_radius": 10.0, "eject_cooldown_frames": 30,
        "fine_grained": True, "budget_image_bytes": 0,
        "floor_anchor": True,
    })
    rpc("ash_renderer/set_camera", {"target": [0, 0.5, 0], "yaw_deg": 0, "pitch_deg": 25, "radius": 18})
    wait_settle(5.0)

    if args.build_only:
        print(f"\n场景已重建（--build-only，预算 {budget}B 未采证）。")
        sys.exit(0)

    print(f"\n== ① 缺高档仍有画面（floor_anchor 开，预算 {budget}B ≈ 全量 1/4）==")
    s1 = read_stats()
    f1 = s1["fine"]
    cc1, dc1 = draw_count()
    ejected_total = s1["ejected_total"] + f1["lod_levels_ejected_total"]
    print(f"  used={s1['used_mesh_bytes']} 保底驻留组={f1['floor_groups_resident']} "
          f"淘汰累计={ejected_total} 命令={cc1} draw_calls={dc1}")
    check("①a 保底档全组驻留（远景有落点）", f1["floor_groups_resident"] == GRID * GRID,
          f"floor_groups_resident={f1['floor_groups_resident']}/{GRID * GRID}")
    check("①b 高档/基准确被淘汰（按需语义生效）", ejected_total > 0,
          f"ejected={s1['ejected_total']} lod_ejected={f1['lod_levels_ejected_total']}")
    check("①c 组行全照常绘制（远景不因高档缺席消失；Helmet 非组行无保底，"
          "按 6.4 语义可暂缓——6.5 范围钉明）", cc1 >= GRID * GRID and dc1 <= 1,
          f"命令 {cc1} ≥ 组行 {GRID * GRID}（账本 {s1['rows']} 行含非组载具），draw_calls={dc1}")
    if host_pid is not None:
        shoot(host_pid, "floor_anchor_on_远景保底.png")

    print("\n== ② A/B：floor_anchor 关 = 6.4 现状（基准缺席行整行暂缓）==")
    rpc("ash_renderer/residency_set", {"floor_anchor": False})
    wait_settle(6.0)
    s2 = read_stats()
    cc2, dc2 = draw_count()
    print(f"  关：命令={cc2}（行 {s2['rows']}）保底驻留组={s2['fine']['floor_groups_resident']}")
    check("② 基准缺席行消失（对照面：锚在基准 = 远景蒸发）", cc2 < s2["rows"],
          f"命令 {cc2} < 行 {s2['rows']}")
    if host_pid is not None:
        shoot(host_pid, "floor_anchor_off_远景消失.png")
    rpc("ash_renderer/residency_set", {"floor_anchor": True})
    wait_settle(6.0)
    cc2b, _ = draw_count()
    check("②b 回开恢复组行全可画（锚切换沿重收敛；非组行仍按基准锚）", cc2b >= GRID * GRID,
          f"命令 {cc2b} ≥ 组行 {GRID * GRID}")

    print("\n== ③ 高档按需回流：相机接近角落 → 需求位图点名基准 → 重新驻留 ==")
    far_base_absent = False
    st_mid = read_stats()
    for a in st_mid["assets"]:
        if not a["resident"] and a["min_dist"] > 10.0:
            far_base_absent = True
            break
    else:
        far_base_absent = False
    rpc("ash_renderer/set_camera", {"target": CORNER, "yaw_deg": 40, "pitch_deg": 20, "radius": 5})
    wait_settle(6.0)
    s3 = read_stats()
    near = [a for a in s3["assets"] if a["min_dist"] <= 8.0]
    near_resident = sum(1 for a in near if a["resident"])
    print(f"  接近角落：近处身份 {near_resident}/{len(near)} 驻留，used={s3['used_mesh_bytes']}")
    check("③ 近处高档按需回流（需求点名 → 基准重驻留）",
          len(near) > 0 and near_resident == len(near),
          f"近处 {near_resident}/{len(near)}，远处基准缺席={far_base_absent}")

    print("\n== ④ 回退零悬空：保底/回退链路全程合法（读图归施工记录）==")
    cc4, dc4 = draw_count()
    check("④ 命令组装正常（表内回退 + rec 保底引脚链活着）", dc4 <= 1 and 0 < cc4 <= s3["rows"],
          f"命令={cc4} draw_calls={dc4}")

    print("\n== ⑤ 关闭沿恢复：管理器关 + 预算 0 + 锚关 → 全常驻基线 ==")
    rpc("ash_renderer/residency_set", {"enabled": False, "budget_mesh_bytes": 0, "floor_anchor": False})
    wait_settle(6.0)
    s5 = read_stats()
    cc5, _ = draw_count()
    check("⑤ 全行恢复可画（下降沿全行置脏消化）", cc5 == s5["rows"],
          f"命令 {cc5} = 行 {s5['rows']}")

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
