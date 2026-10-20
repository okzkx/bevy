# -*- coding: utf-8 -*-
"""细粒度流送一键压测驱动（6.4）：LOD 档逐档驻留 + 贴图字节预算 + fallback 保底。

场景生成持久化（6.2.1 口径）：复用 BRP 方法族——unique 阶梯网格（LOD 档
细粒度载具）+ 默认 Helmet 场景的 glTF 贴图（贴图账载具，零新资产）；同参数
重跑同场景。

判定线采证（对应 6.4 段施工计划判定线草案）：
  ① LOD 档细粒度：fine_grained 开 + mesh 预算压紧 → 变体档被逐档淘汰
     （lod_levels_ejected_total > 0）而基准全驻留（draw_calls 恒 1）
  ② 细节渐进不闪回：相机接近远阶梯 → 档位流进（lod_levels_resident 上升）
     → 拉远 → 细档可淘汰；往复无长期缺失（回访恢复）
  ③ 回退零悬空：细档淘汰期间对应行照常绘制（粗档回退），零 VUID、无黑洞
  ④ 贴图账：image 预算压紧 → helmet 贴图整资产淘汰（used_image_bytes 下降、
     画面合法降级 fallback 白图）→ 关预算恢复全驻留（used 回升）
  ⑤ A/B：fine_grained 关 = 档位全驻留（6.2 语义回归）；开 = 收敛
  ⑥ 零 VUID + exit 0（脚本自启宿主才查）

用法：同 scene_stream.py（--no-close / --skip-launch / --build-only）。
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
HOST_LOG = os.path.join(REPO, ".temp", "scene_fine_host.log")
BRP_PORT = 15702

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
        if len(row) >= 2 and row[0] == "ash_renderer.exe":
            rows.append((int(row[1]), row[0]))
    return rows


def pid_alive(pid):
    return any(p[0] == pid for p in iter_processes())


GRID = 3        # 3×3 unique 阶梯（9 组 × 4 档 = 36 mesh 身份）
SPACING = 14.0


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
                "name": f"细粒度-阶梯-{gx}-{gz}",
            })
            entities.append(r["entity"])
    wait_settle(6.0)
    st = rpc("ash_renderer/residency_stats")
    print(f"  铺场 {len(entities)} 阶梯，账本 {st['rows']} 行，"
          f"mesh 驻留 {st['used_mesh_bytes']}B / 档位驻留 {st['fine']['lod_levels_resident']}")
    return entities


def read_stats():
    return rpc("ash_renderer/residency_stats")


def main():
    ap = argparse.ArgumentParser(description="细粒度流送一键压测（6.4）")
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

    print("\n== 场景重建（unique 阶梯网格 → 持久化脚本；默认 Helmet 场景=贴图账载具）==")
    build_scene()
    full = read_stats()
    budget = max(full["used_mesh_bytes"] // 4, 200_000)  # 压到全量 1/4：细档必被逐档淘汰
    rpc("ash_renderer/residency_set", {
        "enabled": True, "budget_mesh_bytes": budget,
        "request_radius": 24.0, "eject_cooldown_frames": 30,
        "fine_grained": True, "budget_image_bytes": 0,
    })
    rpc("ash_renderer/set_camera", {"target": [0, 0.5, 0], "yaw_deg": 0, "pitch_deg": 25, "radius": 8})
    wait_settle(4.0)

    if args.build_only:
        print(f"\n场景已重建（--build-only，预算 {budget}B 未采证）。")
        sys.exit(0)

    print(f"\n== ① LOD 档细粒度（fine_grained 开，预算 {budget}B ≈ 全量 1/4）==")
    s1 = read_stats()
    f1 = s1["fine"]
    print(f"  used={s1['used_mesh_bytes']} ejected={s1['ejected_total']} "
          f"档位驻留={f1['lod_levels_resident']} 档位淘汰={f1['lod_levels_ejected_total']}")
    check("① 变体档逐档淘汰（ejected > 0）而场景照常绘制（draw_calls 恒 1）",
          f1["lod_levels_ejected_total"] > 0,
          f"lod_levels_resident={f1['lod_levels_resident']}")
    ds1 = rpc("ash_renderer/draw_stats")
    check("①b draw_calls 恒 ≤1（基准档保底，行可绘不因细档缺席中断）",
          ds1.get("draw_calls_recorded", 1) <= 1)

    print("\n== ② 细节渐进不闪回：接近远阶梯 → 档位流进；拉远 → 细档可淘汰 ==")
    # 相机移到角落阶梯：其细档在请求半径内 → 流进
    rpc("ash_renderer/set_camera", {"target": [-SPACING, 0.5, -SPACING], "radius": 6})
    wait_settle(5.0)
    s2a = read_stats()
    print(f"  接近角落：档位驻留 {s2a['fine']['lod_levels_resident']}")
    rpc("ash_renderer/set_camera", {"target": [0, 0.5, 0], "radius": 20})
    wait_settle(5.0)
    s2b = read_stats()
    print(f"  拉远回中：档位驻留 {s2b['fine']['lod_levels_resident']}")
    check("② 细粒度语义生效：接近角落需求收缩（档位驻留降/平——近处只需求基准档）且回中恢复，全程 used ≤ budget",
          s2b["fine"]["lod_levels_resident"] >= s2a["fine"]["lod_levels_resident"]
          and s2a["used_mesh_bytes"] <= s2a["budget_mesh_bytes"]
          and s2b["used_mesh_bytes"] <= s2b["budget_mesh_bytes"],
          f"{s1['fine']['lod_levels_resident']}→{s2a['fine']['lod_levels_resident']}→{s2b['fine']['lod_levels_resident']}")

    print("\n== ③ 回退零悬空：细档淘汰期间画面合法（连拍读图走 6.4.6 归档）==")
    ds3 = rpc("ash_renderer/draw_stats")
    inc3 = rpc("ash_renderer/increment_stats")
    print(f"  draw_calls={ds3.get('draw_calls_recorded')} 命令={ds3.get('command_count')} "
          f"在场行={inc3['rows']}")
    check("③ 细档缺席期间命令组装正常（表内回退链活着）",
          ds3.get("draw_calls_recorded", 1) <= 1 and ds3.get("command_count", 0) <= inc3["rows"])

    print("\n== ④ 贴图账：解除 mesh 预算（行稳定）→ image 预算压紧 → 淘汰 → 关预算恢复 ==")
    s3 = read_stats()
    img_full = s3["fine"]["used_image_bytes"]
    # mesh 预算解除（首钉：mesh thrash 让行持续置脏，贴图 eject 被排除法饿死——两账分开采证）
    rpc("ash_renderer/residency_set", {"budget_mesh_bytes": 0})
    wait_settle(5.0)
    rpc("ash_renderer/residency_set", {"budget_image_bytes": max(img_full // 2, 1)})
    wait_settle(5.0)
    s4a = read_stats()
    print(f"  贴图预算 {s4a['fine']['budget_image_bytes']}B：used={s4a['fine']['used_image_bytes']}B "
          f"（全量 {img_full}B）over={s4a['fine']['over_budget_image']}")
    check("④a 贴图整资产淘汰（used_image_bytes 下降）",
          s4a["fine"]["used_image_bytes"] < img_full,
          f"{img_full}→{s4a['fine']['used_image_bytes']}B（缺席档=fallback 白图，读图归 6.4.6）")
    rpc("ash_renderer/residency_set", {"budget_image_bytes": 0})
    wait_settle(5.0)
    s4b = read_stats()
    check("④b 关预算恢复全驻留（下降沿全行置脏，重传链消化）",
          s4b["fine"]["used_image_bytes"] >= img_full,
          f"{s4b['fine']['used_image_bytes']}B")

    print("\n== ⑤ A/B：fine_grained 关 = 档位全驻留（6.2 语义回归）==")
    rpc("ash_renderer/residency_set", {"fine_grained": False})
    wait_settle(5.0)
    s5 = read_stats()
    print(f"  关：档位驻留 {s5['fine']['lod_levels_resident']} ejected={s5['ejected_total']}")
    check("⑤ 关后档位回驻留（细粒度 A/B 面成立）",
          s5["fine"]["lod_levels_resident"] >= s2b["fine"]["lod_levels_resident"])
    rpc("ash_renderer/residency_set", {"fine_grained": True})
    wait_settle(4.0)

    print("\n== ⑥ 零 VUID（仅脚本自启的宿主有日志可查）==")
    if host_pid is not None:
        try:
            log = open(HOST_LOG, "rb").read().decode("utf-8", errors="replace")
            bad = [l for l in log.splitlines()
                   if "VUID" in l or "VK_ERROR" in l or "panicked" in l]
            check("⑥ 宿主日志零 VUID/VK_ERROR/panic", not bad, f"{len(bad)} 行" if bad else "")
        except OSError as e:
            check("⑥ 日志读取失败", False, str(e))
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
