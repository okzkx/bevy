# -*- coding: utf-8 -*-
"""zero_day 规模压测一键驱动（7.2）。

资产：zero_day_measure_one.glb（500.1MB，CC BY 4.0，bevy_asset_files release
zero-day-1.0）——期望值与站位从 glb JSON chunk 运行时解析（primitive/材质/
贴图清单、场景 AABB），不写死数字：换 measure 场景脚本自适应。

前置：BRP load_world 方法族（7.2.1）——整场景动态进场，Sponza Startup 默认
不动。判定线采证（对应 7.2 段施工计划判定线草案）：
  ① 加载清单对照：rows == 解析 primitive 数、resident_meshes 同、贴图全驻留
  ② 规模账面成组记录：mesh/贴图驻留字节 + 加载耗时 + GPU 读数（对照 Sponza
     基线 9.3MB/362.8MB，只记录不预设达标）
  ③ 预算流送：预算 = 全常驻实占 1/4（下限 5MB），走廊五站推进全程
     used ≤ budget、rows 不掉、命令 ≤ 在场行（零幽灵）；关闭沿回全量
  ④ 6.5/6.6 真实资产复验：floor_anchor 开（无变体资产 = 保底即基准，读数
     不回退）；gap_mode=Merged（缺口回流、无当帧等待）
  ⑤ 零 VUID + exit 0（脚本自启宿主才查日志）

用法：
  python tools/scene_zero_day.py               # 默认：自启宿主，测完关掉
  python tools/scene_zero_day.py --no-close
  python tools/scene_zero_day.py --skip-launch
退出码：全过 = 0，任一失败 = 1。
"""

import argparse
import ctypes
import json
import os
import struct
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
GLB = os.path.join(REPO, "assets", "zero_day", "zero_day_measure_one.glb")
HOST_LOG = os.path.join(REPO, ".temp", "scene_zero_day_host.log")
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


def parse_glb(path):
    """解析 glb JSON chunk：清单计数 + 场景 AABB（accessor min/max × 节点矩阵）。"""
    with open(path, "rb") as f:
        magic, version, length = struct.unpack("<III", f.read(12))
        assert magic == 0x46546C67, "不是 glb"
        chunk_len, chunk_type = struct.unpack("<II", f.read(8))
        gltf = json.loads(f.read(chunk_len))

    prims = sum(len(m.get("primitives", [])) for m in gltf.get("meshes", []))
    mats = len(gltf.get("materials", []))
    imgs = len(gltf.get("images", []))

    import math

    def xform(m, p):
        x, y, z = p
        return (
            m[0] * x + m[4] * y + m[8] * z + m[12],
            m[1] * x + m[5] * y + m[9] * z + m[13],
            m[2] * x + m[6] * y + m[10] * z + m[14],
        )

    ident = [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1]
    mins, maxs = [1e30] * 3, [-1e30] * 3
    for node in gltf.get("nodes", []):
        if "mesh" not in node:
            continue
        m = node.get("matrix", ident)
        mesh = gltf["meshes"][node["mesh"]]
        for prim in mesh.get("primitives", []):
            acc = gltf["accessors"][prim["attributes"]["POSITION"]]
            for corner in [
                (acc["min"][0], acc["min"][1], acc["min"][2]),
                (acc["max"][0], acc["min"][1], acc["min"][2]),
                (acc["min"][0], acc["max"][1], acc["min"][2]),
                (acc["min"][0], acc["min"][1], acc["max"][2]),
                (acc["max"][0], acc["max"][1], acc["min"][2]),
                (acc["max"][0], acc["min"][1], acc["max"][2]),
                (acc["min"][0], acc["max"][1], acc["max"][2]),
                (acc["max"][0], acc["max"][1], acc["max"][2]),
            ]:
                w = xform(m, corner)
                for i in range(3):
                    mins[i] = min(mins[i], w[i])
                    maxs[i] = max(maxs[i], w[i])
    diag = math.dist(mins, maxs)
    return {
        "primitives": prims, "materials": mats, "images": imgs,
        "mins": mins, "maxs": maxs, "diag": diag,
    }


def wait_brp(seconds=60):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            rpc("ash_renderer/increment_stats")
            return True
        except Exception:
            time.sleep(1.0)
    return False


def brp(method, params=None, timeout=30):
    """rpc 的容错壳：BRP 错误（如多相机期 residency_stats 拒答）返回 None。"""
    try:
        return rpc(method, params, timeout)
    except RuntimeError:
        return None


def camera_ids():
    r = rpc("world.query", {"data": {"components": []},
                            "filter": {"with": ["bevy_camera::camera::Camera"]}})
    return [e["entity"] for e in r]


def cleanup_extra_cameras(keep):
    """清掉进场场景内嵌相机（glTF camera 节点）：宿主唯一相机语义——多相机下
    residency_stats/draw 链的 single() 全崩。实抓首钉，机制卫兵归 7.4；本段
    压测脚本先按"保留最早实体"清理解阻塞。"""
    gone = []
    for e in camera_ids():
        if e != keep:
            rpc("world.despawn_entity", {"entity": e})
            gone.append(e)
    return gone


def wait_loaded(baseline_rows, expect_prims, keep_camera, seconds=900):
    """load_world 受理后等进场展开 + 上传排空（500MB 级，数分钟量级）。

    行数口径 = 基线差值（Sponza Startup 场景仍在账，rows 含它）；轮询中每次
    residency_stats 拒答（多相机）都先清内嵌相机再重试。"""
    deadline = time.time() + seconds
    last = None
    while time.time() < deadline:
        st = brp("ash_renderer/residency_stats")
        if st is None:
            cleanup_extra_cameras(keep_camera)
            time.sleep(2.0)
            continue
        inc = rpc("ash_renderer/increment_stats")
        if (st["rows"] >= baseline_rows + expect_prims
                and st["resident_meshes"] >= baseline_rows + expect_prims
                and inc["upload"]["dirty_rows"] == 0):
            return st
        last = (st, inc)
        time.sleep(2.0)
    if last:
        st, inc = last
        print(f"  [FAIL] zero_day {seconds}s 未排空：rows={st['rows']} "
              f"resident={st['resident_meshes']} dirty_rows={inc['upload']['dirty_rows']}")
    else:
        print("  [FAIL] BRP 一直不可达")
    return None


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


def capture(pid, out):
    os.makedirs(os.path.dirname(out), exist_ok=True)
    r = subprocess.run(
        [sys.executable, os.path.join(REPO, "tools", "capture_window.py"), "--pid", str(pid), "--out", out],
        capture_output=True,
    )
    note = r.stdout.decode("utf-8", errors="replace").strip() or r.stderr.decode("utf-8", errors="replace").strip()
    print(f"  截图 {out}\n  {note}")


def main():
    ap = argparse.ArgumentParser(description="zero_day 规模压测一键驱动（7.2）")
    ap.add_argument("--no-close", action="store_true")
    ap.add_argument("--skip-launch", action="store_true")
    args = ap.parse_args()

    if not os.path.isfile(GLB):
        print(f"资产不存在：{GLB}\n先下载（bevy_asset_files release zero-day-1.0，500MB）。")
        sys.exit(2)
    scene = parse_glb(GLB)
    print(f"glb 清单：primitive {scene['primitives']} / 材质 {scene['materials']} / "
          f"贴图 {scene['images']}；AABB "
          f"({scene['mins'][0]:.1f},{scene['mins'][1]:.1f},{scene['mins'][2]:.1f})~"
          f"({scene['maxs'][0]:.1f},{scene['maxs'][1]:.1f},{scene['maxs'][2]:.1f})，"
          f"对角线 {scene['diag']:.1f}")

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

    print("\n== ① load_world 进场 + 加载清单对照 ==")
    base = rpc("ash_renderer/residency_stats")
    base_rows, base_imgs = base["rows"], base["resident_images"]
    cameras = camera_ids()
    check("①pre 宿主唯一相机（Sponza 基线）", len(cameras) == 1,
          f"cameras={cameras}（多相机即清外）")
    keep_camera = cameras[0] if cameras else None
    t0 = time.time()
    r = rpc("ash_renderer/load_world", {"path": "zero_day/zero_day_measure_one.glb"})
    print(f"  load_world 受理：{r}")
    st = wait_loaded(base_rows, scene["primitives"], keep_camera)
    if st is None:
        if host_pid is not None and not args.no_close:
            close_host(host_pid)
        sys.exit(1)
    load_secs = time.time() - t0
    inc = rpc("ash_renderer/increment_stats")
    d_rows = st["rows"] - base_rows
    print(f"  rows {base_rows}->{st['rows']}（Δ{d_rows}）resident={st['resident_meshes']} "
          f"used_mesh={st['used_mesh_bytes']/1e6:.1f}MB images={st['resident_images']} "
          f"（Δ{st['resident_images'] - base_imgs}）耗时 {load_secs:.0f}s")
    check("①a primitive 全在账（Δrows == 解析 primitive 数）",
          d_rows == scene["primitives"], f"Δ={d_rows} 期望 {scene['primitives']}")
    check("①b mesh 全驻留", st["resident_meshes"] == st["rows"],
          f"resident={st['resident_meshes']} rows={st['rows']}")
    check("①c 文件贴图全驻留（实载可超 JSON 计数：KHR_texture_transform 复用展开）",
          st["resident_images"] - base_imgs >= scene["images"],
          f"Δ={st['resident_images'] - base_imgs} 期望 ≥ {scene['images']}")
    check("①d 稳态零上传", inc["upload"]["dirty_rows"] == 0,
          f"dirty_rows={inc['upload']['dirty_rows']}")

    print("\n== ② 规模账面成组记录（对照 Sponza 基线 mesh 9.3MB/贴图 362.8MB）==")
    perf = rpc("ash_renderer/perf_stats")
    print(f"  GPU 全帧 p50 {perf['gpu_total_ms']['p50']:.2f}ms · p95 {perf['gpu_total_ms']['p95']:.2f}ms；"
          f"GPU 光照 p50 {perf['gpu_lighting_ms']['p50']:.2f}ms；CPU p50 {perf['cpu_total_ms']['p50']:.2f}ms；"
          f"实例行 {perf['scale']['instance_rows']}；显存贴图 "
          f"{perf['memory']['image_resident_bytes']/1e9:.2f}GB/{perf['memory']['image_resident_count']} 张")
    check("② 规模读数可测（全常驻 used_mesh > 0 且 GPU 计时开着）",
          st["used_mesh_bytes"] > 0 and perf.get("gpu_timer_enabled", False),
          f"used_mesh={st['used_mesh_bytes']/1e6:.1f}MB")

    print("\n== ③ 预算流送：走廊五站推进（预算 = 全常驻 1/4）==")
    full = st["used_mesh_bytes"]
    budget = max(full // 4, 5_000_000)
    rpc("ash_renderer/residency_set", {
        "enabled": True, "budget_mesh_bytes": budget,
        "request_radius": max(scene["diag"] / 3, 30.0),
        "eject_cooldown_frames": 30,
    })
    wait_settle = time.sleep
    wait_settle(3.0)
    mn, mx = scene["mins"], scene["maxs"]
    cx, cz = (mn[0] + mx[0]) / 2, (mn[2] + mx[2]) / 2
    ok_tour = True
    for i, t in enumerate((0.0, 0.25, 0.5, 0.75, 1.0)):
        x = mn[0] + (mx[0] - mn[0]) * t
        z = mn[2] + (mx[2] - mn[2]) * t
        rpc("ash_renderer/set_camera",
            {"target": [x, (mn[1] + mx[1]) / 2, z], "yaw_deg": 90 + i * 30, "pitch_deg": 5,
             "radius": max(scene["diag"] / 8, 6.0)})
        time.sleep(4.0)
        s = rpc("ash_renderer/residency_stats")
        ds = rpc("ash_renderer/draw_stats")
        print(f"  站 {i} t={t}: used={s['used_mesh_bytes']/1e6:.1f}MB "
              f"ejected={s['ejected_total']} requested={s['requested']} cooling={s['cooling']}")
        if s["used_mesh_bytes"] > s["budget_mesh_bytes"]:
            ok_tour = False
        if ds.get("draw_calls_recorded", 1) > 1:
            ok_tour = False
    s3 = rpc("ash_renderer/residency_stats")
    inc3 = rpc("ash_renderer/increment_stats")
    ds3 = rpc("ash_renderer/draw_stats")
    check("③a 游历全程 used ≤ budget 且 draw_calls 恒 ≤1", ok_tour,
          f"末站 used={s3['used_mesh_bytes']/1e6:.1f}MB budget={s3['budget_mesh_bytes']/1e6:.1f}MB")
    check("③b rows 不掉", s3["rows"] == base_rows + scene["primitives"], f"rows={s3['rows']}")
    check("③c 命令条数 ≤ 在场行（未驻留行暂缓 = 零幽灵）",
          ds3.get("command_count", 0) <= inc3["rows"],
          f"command_count={ds3.get('command_count')} rows={inc3['rows']}")

    print("\n== ④ 6.5/6.6 真实资产复验 ==")
    rpc("ash_renderer/residency_set", {"floor_anchor": True})
    rpc("ash_renderer/gap_set", {"mode": "merged"})
    time.sleep(4.0)
    g = rpc("ash_renderer/gap_stats")
    s4 = rpc("ash_renderer/residency_stats")
    print(f"  floor_anchor=on + gap=merged: rows={s4['rows']} requested={s4['requested']} "
          f"gap_stats={g}")
    check("④a floor_anchor 开（无变体资产=保底即基准）rows 不掉",
          s4["rows"] == base_rows + scene["primitives"], f"rows={s4['rows']}")
    check("④b gap_mode=Merged 请求有界（requested ≤ 行数）",
          s4["requested"] <= s4["rows"], f"requested={s4['requested']}")

    print("\n== ⑤ A/B：关 = 全常驻基线恢复 ==")
    rpc("ash_renderer/residency_set", {"enabled": False, "budget_mesh_bytes": 0,
                                       "floor_anchor": False})
    rpc("ash_renderer/gap_set", {"mode": "off"})
    time.sleep(5.0)
    s5 = rpc("ash_renderer/residency_stats")
    print(f"  关后 used={s5['used_mesh_bytes']/1e6:.1f}MB（全量 {full/1e6:.1f}MB）resident={s5['resident_meshes']}")
    check("⑤ 关后回全量常驻", s5["used_mesh_bytes"] >= full * 0.99
          and s5["resident_meshes"] == s5["rows"],
          f"used={s5['used_mesh_bytes']} resident={s5['resident_meshes']}")

    print("\n== ⑥ 零 VUID（仅脚本自启的宿主有日志可查）==")
    if host_pid is not None and not args.skip_launch:
        try:
            log = open(HOST_LOG, "rb").read().decode("utf-8", errors="replace")
            bad = [l for l in log.splitlines()
                   if "VUID" in l or "VK_ERROR" in l or "panicked" in l]
            check("⑥ 宿主日志零 VUID/VK_ERROR/panic", not bad, f"{len(bad)} 行" if bad else "")
        except OSError as e:
            check("⑥ 日志读取失败", False, str(e))
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
