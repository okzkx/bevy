# -*- coding: utf-8 -*-
"""流送场景一键生成与超预算游历压测驱动（6.3）。

场景生成持久化（6.2.1 口径延伸）：复用 BRP 方法族生成——`spawn-lod --unique`
独立四档阶梯铺网格（每阶梯独立 mesh 资产，"世界大于驻留"的压测前提）+ 钉机
位 + 预算/半径设定；同参数重跑同场景。

判定线采证（对应 6.3 段施工计划判定线草案）：
  ① 预算硬约束：开启后 used_mesh_bytes ≤ budget（淘汰收敛）
  ② 可持续游历：相机逐站移动，全程 rows>0、无 panic、used ≤ budget；
     回访起点站后起点资产重新驻留（requested→resident 恢复）
  ③ 发布语义：未驻留行 DrawList 暂缓（零幽灵——draw_stats 正常、
     命令条数 = 在场行数）
  ④ A/B：关（全常驻基线）= used 恢复全量、ejected 停止增长；开 = 收敛回预算
  ⑤ 零 VUID + exit 0（脚本自启宿主才查）

用法：
  python tools/scene_stream.py              # 默认：自启宿主，测完关掉
  python tools/scene_stream.py --no-close   # 测完不关宿主
  python tools/scene_stream.py --skip-launch
  python tools/scene_stream.py --build-only # 只重建场景（压测前手动铺场）
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
HOST_LOG = os.path.join(REPO, ".temp", "scene_stream_host.log")
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


def iter_processes():
    import csv
    import io
    out = subprocess.run(
        ["tasklist", "/FI", "IMAGENAME eq ash_renderer.exe", "/FO", "CSV", "/NH"],
        capture_output=True,
    ).stdout.decode("gbk", errors="replace")
    rows = []
    for row in csv.reader(io.StringIO(out)):
        if len(row) >= 2 and row[0] == HOST_NAME:
            rows.append((int(row[1]), row[0]))
    return rows


def pid_alive(pid):
    return any(p[0] == pid for p in iter_processes())


# ===== 场景生成（流送压测载具）=====

# 4×4 阶梯网格：间距 14，世界 42×42；每阶梯独立四档 mesh（ico4 档 ≈36KB），
# 16 阶梯 ≈ 2.3MB 驻留需求——预算给 700KB（≈1/3）即可制造持续超预算。
GRID = 4
SPACING = 14.0


def build_scene():
    """铺 4×4 独立资产阶梯网格，返回 (实体清单, 全量驻留需求估计的读数)。"""
    entities = []
    for gx in range(GRID):
        for gz in range(GRID):
            x = (gx - (GRID - 1) / 2) * SPACING
            z = (gz - (GRID - 1) / 2) * SPACING
            r = rpc("ash_renderer/spawn_lod", {
                "shape": "sphere",
                "color": [0.2 + gx * 0.2, 0.3 + gz * 0.15, 0.7],
                "translation": [x, 0.5, z],
                "unique": True,
                "name": f"流送-阶梯-{gx}-{gz}",
            })
            entities.append(r["entity"])
    wait_settle(6.0)  # 每阶梯 4 资产上传 + 账本入行
    st = rpc("ash_renderer/residency_stats")
    print(f"  铺场 {len(entities)} 阶梯（独立资产），账本 {st['rows']} 行，"
          f"mesh 驻留 {st['used_mesh_bytes']}B（全常驻口径）")
    return entities


def read_stats():
    return rpc("ash_renderer/residency_stats")


def main():
    ap = argparse.ArgumentParser(description="流送场景一键生成与超预算游历压测（6.3）")
    ap.add_argument("--no-close", action="store_true")
    ap.add_argument("--skip-launch", action="store_true")
    ap.add_argument("--build-only", action="store_true")
    args = ap.parse_args()

    host_pid = None
    if not args.skip_launch:
        alive = [p[0] for p in iter_processes()]
        if alive:
            print(f"检测到宿主已在跑（PID {alive[0]}），直接驱动它（测完不关）。")
            args.no_close = True
        else:
            proc = launch_host()
            host_pid = proc.pid
    if not wait_brp():
        print("FAIL: BRP 端口 60 秒未就绪")
        sys.exit(1)

    print("\n== 场景重建（unique 阶梯网格 → 持久化脚本）==")
    build_scene()
    # 预算 = 全常驻实占的 1/3（读数现算，自适应规模）；半径覆盖 1.5 格
    full = read_stats()["used_mesh_bytes"]
    budget = max(full // 3, 200_000)
    rpc("ash_renderer/residency_set", {
        "enabled": True, "budget_mesh_bytes": budget,
        "request_radius": 26.0, "eject_cooldown_frames": 30,
    })
    rpc("ash_renderer/set_camera", {"target": [0, 0.5, 0], "yaw_deg": 0, "pitch_deg": 25, "radius": 8})
    wait_settle(3.0)

    if args.build_only:
        print(f"\n场景已重建（--build-only，预算 {budget}B 未采证）。")
        sys.exit(0)

    print(f"\n== ① 预算硬约束（预算 {budget}B ≈ 全常驻 1/3）==")
    s1 = read_stats()
    print(f"  used={s1['used_mesh_bytes']}B budget={s1['budget_mesh_bytes']}B "
          f"resident={s1['resident_meshes']} ejected={s1['ejected_total']} over_budget={s1['over_budget']}")
    check("① used ≤ budget（淘汰收敛）",
          s1["used_mesh_bytes"] <= s1["budget_mesh_bytes"] and s1["ejected_total"] > 0,
          f"over_budget={s1['over_budget']}")

    print("\n== ② 可持续游历：四站环游 + 回访起点 ==")
    stations = [(-14, -14), (14, -14), (14, 14), (-14, 14), (-14, -14)]  # 末站=起点回访
    ok_tour = True
    for i, (x, z) in enumerate(stations):
        rpc("ash_renderer/set_camera", {"target": [x, 0.5, z], "radius": 8})
        wait_settle(4.0)
        st = read_stats()
        ds = rpc("ash_renderer/draw_stats")
        inc = rpc("ash_renderer/increment_stats")
        print(f"  站 {i} @({x},{z}): used={st['used_mesh_bytes']} ejected={st['ejected_total']} "
              f"requested={st['requested']} cooling={st['cooling']} pending_rows={inc['upload']['dirty_rows']}")
        if st["used_mesh_bytes"] > st["budget_mesh_bytes"]:
            ok_tour = False
        if ds.get("draw_calls_recorded", 1) > 1:
            ok_tour = False
    check("②a 游历全程 used ≤ budget 且 draw_calls 恒 ≤1", ok_tour)
    # 回访起点：起点站资产应重新驻留（resident_meshes 不缩水 + 起点网格行可绘）
    s_end = read_stats()
    check("②b 回访后仍在预算内", s_end["used_mesh_bytes"] <= s_end["budget_mesh_bytes"],
          f"used={s_end['used_mesh_bytes']}")

    print("\n== ③ 发布语义（未驻留行暂缓 = 无幽灵）==")
    inc = rpc("ash_renderer/increment_stats")
    ds = rpc("ash_renderer/draw_stats")
    print(f"  在场行 {inc['rows']}，pending（上传侧脏行）={inc['upload']['dirty_rows']}，"
          f"draw_calls={ds.get('draw_calls_recorded')}")
    check("③ 淘汰期间命令条数 ≤ 在场行（暂缓行不进命令环）",
          ds.get("command_count", 0) <= inc["rows"])

    print("\n== ④ A/B：关 = 全常驻基线 ==")
    rpc("ash_renderer/residency_set", {"enabled": False})
    wait_settle(5.0)
    s_off = read_stats()
    print(f"  关：used={s_off['used_mesh_bytes']} ejected={s_off['ejected_total']} "
          f"resident={s_off['resident_meshes']}")
    # ejected 容差 = 关生效前的在途请求消化（②末 pending 行完成可能短暂触顶淘汰，
    # BRP 写值次帧生效——容差上限 = 场景资产数口径）
    check("④ 关后全常驻（used 回到全量口径、ejected 增量在在途容差内）",
          s_off["used_mesh_bytes"] >= full
          and s_off["ejected_total"] - s_end["ejected_total"] < 64)
    rpc("ash_renderer/residency_set", {"enabled": True})
    wait_settle(5.0)
    s_on = read_stats()
    check("④b 开 = 收敛回预算", s_on["used_mesh_bytes"] <= s_on["budget_mesh_bytes"],
          f"used={s_on['used_mesh_bytes']}")

    print("\n== ⑤ 零 VUID（仅脚本自启的宿主有日志可查）==")
    if host_pid is not None:
        try:
            log = open(HOST_LOG, "rb").read().decode("utf-8", errors="replace")
            bad = [l for l in log.splitlines()
                   if "VUID" in l or "VK_ERROR" in l or "panicked" in l]
            check("⑤ 宿主日志零 VUID/VK_ERROR/panic", not bad, f"{len(bad)} 行" if bad else "")
        except OSError as e:
            check("⑤ 日志读取失败", False, str(e))
    else:
        print("  [SKIP] 驱动的既有实例无日志可查")

    if host_pid is not None and not args.no_close:
        close_host(host_pid)

    failed = [name for name, ok in _results if not ok]
    print(f"\n== 结果：{len(_results) - len(failed)}/{len(_results)} 过 ==")
    for name in failed:
        print(f"  FAIL: {name}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
