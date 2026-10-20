# -*- coding: utf-8 -*-
"""Sponza 场景基座一键压测驱动（7.1）。

场景来源：Startup 进场组自加载 `sponza/glTF/Sponza.gltf`（内容拍板首推场景，
入库资产，无需生成脚本铺场）——本脚本只负责等加载、采证与判定线驱动。

判定线采证（对应 7.1 段施工计划判定线草案）：
  ① 加载清单对照：账本 rows == 103（primitive 全在账）、resident_meshes == 103、
     贴图驻留 ≥ 69（文件贴图全驻留，容器口径 71 = 内置 2 + 文件 69）
  ② 静态游历零重传：中庭五站环游后 requested/cooling/ejected 增量全零、
     上传侧脏行零（全常驻基线下的稳态）
  ③ 预算负例：预算 = 全常驻实占 1/3，used ≤ budget 收敛、rows 不掉、
     命令条数 ≤ 在场行（未驻留行暂缓 = 零幽灵）
  ④ A/B 关闭沿恢复：关后 used 回全量口径
  ⑤ 零 VUID + exit 0（脚本自启宿主才查日志）
另：中庭机位截图（--shot 路径）供读图判定（画面功能视觉验证规则）。

用法：
  python tools/scene_sponza.py               # 默认：自启宿主，测完关掉
  python tools/scene_sponza.py --no-close    # 测完不关宿主
  python tools/scene_sponza.py --skip-launch
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
EXE = os.path.join(REPO, "target", "debug", "ash_renderer.exe")
HOST_LOG = os.path.join(REPO, ".temp", "scene_sponza_host.log")
BRP_PORT = 15702

# Sponza.gltf 清单（解析资产文件实测，立案期钉死）：1 mesh 拆 103 primitive、
# 25 材质、69 贴图全被引用；世界 AABB ≈ 30×12.4×18.3 米（节点 scale 0.008）。
EXPECTED_PRIMITIVES = 103
EXPECTED_FILE_IMAGES = 69

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


def wait_scene(seconds=120):
    """等 Sponza 进场展开入账：rows/mesh 到齐且上传侧排空（69 张 jpg 逐张
    异步上传，mesh 先于贴图收敛——只等 mesh 会把在途上传当稳态）。"""
    deadline = time.time() + seconds
    last = None
    while time.time() < deadline:
        try:
            st = rpc("ash_renderer/residency_stats")
            inc = rpc("ash_renderer/increment_stats")
            if (st["rows"] >= EXPECTED_PRIMITIVES
                    and st["resident_meshes"] >= EXPECTED_PRIMITIVES
                    and inc["upload"]["dirty_rows"] == 0):
                return st
            last = (st, inc)
        except Exception:
            pass
        time.sleep(1.0)
    if last:
        st, inc = last
        print(f"  [FAIL] Sponza {seconds}s 未排空：rows={st['rows']} "
              f"resident={st['resident_meshes']} dirty_rows={inc['upload']['dirty_rows']}"
              f"（看宿主日志加载错误）")
    else:
        print("  [FAIL] BRP 一直不可达")
    return None


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
        if len(row) >= 2 and row[0] == "ash_renderer.exe":
            rows.append((int(row[1]), row[0]))
    return rows


def pid_alive(pid):
    return any(p[0] == pid for p in iter_processes())


def capture(pid, out):
    os.makedirs(os.path.dirname(out), exist_ok=True)
    r = subprocess.run(
        [sys.executable, os.path.join(REPO, "tools", "capture_window.py"), "--pid", str(pid), "--out", out],
        capture_output=True,
    )
    note = r.stdout.decode("utf-8", errors="replace").strip() or r.stderr.decode("utf-8", errors="replace").strip()
    print(f"  截图 {out}\n  {note}")


def main():
    ap = argparse.ArgumentParser(description="Sponza 场景基座一键压测（7.1）")
    ap.add_argument("--no-close", action="store_true")
    ap.add_argument("--skip-launch", action="store_true")
    ap.add_argument("--shot", default=os.path.join(REPO, ".temp", "7.1", "sponza_atrium.png"))
    args = ap.parse_args()

    host_pid = None
    if not args.skip_launch:
        alive = [p[0] for p in iter_processes()]
        if alive:
            print(f"检测到宿主已在跑（PID {alive[0]}），直接驱动它（测完不关）。")
            host_pid = alive[0]
            args.no_close = True
        else:
            proc = launch_host()
            host_pid = proc.pid
    if not wait_brp():
        print("FAIL: BRP 端口 60 秒未就绪")
        sys.exit(1)

    print("\n== ① 加载清单对照（等 Sponza 进场展开）==")
    st = wait_scene()
    if st is None:
        if host_pid is not None and not args.no_close:
            close_host(host_pid)
        sys.exit(1)
    inc = rpc("ash_renderer/increment_stats")
    perf = rpc("ash_renderer/perf_stats")
    print(f"  rows={st['rows']} resident_mesh={st['resident_meshes']} "
          f"used_mesh={st['used_mesh_bytes']}B resident_images={st['resident_images']} "
          f"dirty_rows={inc['upload']['dirty_rows']}")
    check("①a primitive 全在账（rows == 103）", st["rows"] == EXPECTED_PRIMITIVES,
          f"rows={st['rows']}")
    check("①b mesh 全驻留（resident_meshes == 103）", st["resident_meshes"] == EXPECTED_PRIMITIVES,
          f"resident={st['resident_meshes']}")
    check("①c 文件贴图全驻留（resident_images ≥ 69）", st["resident_images"] >= EXPECTED_FILE_IMAGES,
          f"resident_images={st['resident_images']}（驻留计量口径只数文件贴图，期望 69；"
          f"容器口径 71 = 文件 69 + 内置 2，内置白图不进驻留账）")
    check("①d 稳态零上传（脏行 0）", inc["upload"]["dirty_rows"] == 0,
          f"dirty_rows={inc['upload']['dirty_rows']}")

    print("\n== ② 静态游历零重传（中庭五站环游；站 0 = 中庭机位截图）==")
    full = st["used_mesh_bytes"]
    ok_tour = True
    # 站位绕中庭中心（yaw 90 = Startup 同款机位；半径 5 在中庭内部，不穿拱廊）
    for i, yaw in enumerate((90, 150, 210, 270, 330)):
        rpc("ash_renderer/set_camera",
            {"target": [0.0, 3.0, 0.0], "yaw_deg": yaw, "pitch_deg": 5, "radius": 5.0})
        wait_settle(2.5)
        s = rpc("ash_renderer/residency_stats")
        print(f"  站 {i} yaw={yaw}: requested={s['requested']} cooling={s['cooling']} "
              f"ejected_total={s['ejected_total']} requested_rows_last={s['requested_rows_last']}")
        if i == 0 and host_pid is not None:
            capture(host_pid, args.shot)
    st2 = rpc("ash_renderer/residency_stats")
    inc2 = rpc("ash_renderer/increment_stats")
    check("②a 五站环游零重传（requested/cooling/ejected 增量全零）",
          st2["requested"] == 0 and st2["cooling"] == 0
          and st2["ejected_total"] == 0 and st2["requested_rows_last"] == 0,
          f"requested={st2['requested']} cooling={st2['cooling']} "
          f"ejected_total={st2['ejected_total']}")
    check("②b 环游后仍零上传（脏行 0）", inc2["upload"]["dirty_rows"] == 0,
          f"dirty_rows={inc2['upload']['dirty_rows']}")

    print(f"\n== ③ 预算负例（预算 {full // 3}B ≈ 全常驻 1/3，半径 40 全覆盖）==")
    budget = max(full // 3, 200_000)
    rpc("ash_renderer/residency_set", {
        "enabled": True, "budget_mesh_bytes": budget,
        "request_radius": 40.0, "eject_cooldown_frames": 30,
    })
    wait_settle(5.0)
    s3 = rpc("ash_renderer/residency_stats")
    ds3 = rpc("ash_renderer/draw_stats")
    inc3 = rpc("ash_renderer/increment_stats")
    print(f"  used={s3['used_mesh_bytes']}B budget={s3['budget_mesh_bytes']}B "
          f"ejected={s3['ejected_total']} over_budget={s3['over_budget']} "
          f"rows={s3['rows']} command_count={ds3.get('command_count')}")
    check("③a used ≤ budget（淘汰收敛）且 rows 不掉",
          s3["used_mesh_bytes"] <= s3["budget_mesh_bytes"] and s3["rows"] == EXPECTED_PRIMITIVES,
          f"over_budget={s3['over_budget']}（如实报口径）")
    check("③b 命令条数 ≤ 在场行（未驻留行暂缓 = 零幽灵）",
          ds3.get("command_count", 0) <= inc3["rows"],
          f"command_count={ds3.get('command_count')} rows={inc3['rows']}")

    print("\n== ④ A/B：关 = 全常驻基线恢复 ==")
    rpc("ash_renderer/residency_set", {"enabled": False, "budget_mesh_bytes": 0})
    wait_settle(5.0)
    s4 = rpc("ash_renderer/residency_stats")
    print(f"  关后 used={s4['used_mesh_bytes']}B（全量口径 {full}B）resident={s4['resident_meshes']}")
    check("④ 关后回全量常驻（used ≥ 全量、resident == 103）",
          s4["used_mesh_bytes"] >= full and s4["resident_meshes"] == EXPECTED_PRIMITIVES,
          f"used={s4['used_mesh_bytes']} resident={s4['resident_meshes']}")

    print("\n== ⑤ 零 VUID（仅脚本自启的宿主有日志可查）==")
    if host_pid is not None and not args.skip_launch:
        try:
            log = open(HOST_LOG, "rb").read().decode("utf-8", errors="replace")
            bad = [l for l in log.splitlines()
                   if "VUID" in l or "VK_ERROR" in l or "panicked" in l]
            check("⑤ 宿主日志零 VUID/VK_ERROR/panic", not bad, f"{len(bad)} 行" if bad else "")
        except OSError as e:
            check("⑤ 日志读取失败", False, str(e))
    else:
        print("  [SKIP] 驱动的既有实例无日志可查")

    if host_pid is not None and not args.no_close and not args.skip_launch:
        close_host(host_pid)

    failed = [name for name, ok in _results if not ok]
    print(f"\n== 结果：{len(_results) - len(failed)}/{len(_results)} 过 ==")
    for name in failed:
        print(f"  FAIL: {name}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
