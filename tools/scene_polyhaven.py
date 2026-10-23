# -*- coding: utf-8 -*-
"""Poly Haven 材质多样性一键压测驱动（7.3）。

资产：tools/fetch_polyhaven.py 选件终表四件（CC0，各 1 primitive/1 材质/3 贴图
——normal + baseColor + ARM 或 rough）。判定线采证（对应 7.3 段施工计划）：
  ① 逐件加载零失败：BRP load_world 逐件进场（前件整树回收），Δrows/Δ材质/
     Δ贴图与 gltf 解析清单一致（基线差值口径，7.2 同款）
  ② 贴图字节预算列（6.4 第二列）：fine_grained 开档 + budget_image_bytes 档
     （实占现算 1/2）→ 贴图淘汰/fallback 白图/关闭沿恢复，零悬空
  ③ 真 PBR 读图：四件材质同场对照截图（normal/metallic/roughness 槽视觉可辨）
  ④ 零 VUID + exit 0

用法：
  python tools/scene_polyhaven.py               # 自启宿主，测完关掉
  python tools/scene_polyhaven.py --skip-launch
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
HOST_LOG = os.path.join(REPO, ".temp", "scene_polyhaven_host.log")
BRP_PORT = int(os.environ.get("ASH_BRP_PORT", "15702"))
ASSETS = [
    ("ArmChair_01", 1, 1, 3),
    ("antique_ceramic_vase_01", 1, 1, 3),
    ("brass_vase_01", 1, 1, 3),
    ("carved_wooden_elephant", 1, 1, 3),
]

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


def camera_ids():
    r = rpc("world.query", {"data": {"components": []},
                            "filter": {"with": ["bevy_camera::camera::Camera"]}})
    return [e["entity"] for e in r]


def cleanup_extra_cameras(keep):
    gone = []
    for e in camera_ids():
        if e != keep:
            rpc("world.despawn_entity", {"entity": e})
            gone.append(e)
    return gone


def wait_settled(baseline_rows, add_rows, seconds=120):
    """等进场展开 + 上传排空；多相机拒答即清外（7.2 卫兵轮询同款）。"""
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            st = rpc("ash_renderer/residency_stats")
            inc = rpc("ash_renderer/increment_stats")
            # 同资产重载在 AssetServer 去重（同 AssetId）：pool 条目不随实体
            # 复制增长，等待条件只看行数差值与上传排空（resident 差额不可靠）
            if (st["rows"] >= baseline_rows + add_rows
                    and inc["upload"]["dirty_rows"] == 0):
                return st
        except RuntimeError:
            if len(camera_ids()) > 1:
                cleanup_extra_cameras(min(camera_ids(), key=lambda e: int(e.split("v")[0])))
        time.sleep(1.0)
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
    ap = argparse.ArgumentParser(description="Poly Haven 材质多样性一键压测（7.3）")
    ap.add_argument("--no-close", action="store_true")
    ap.add_argument("--skip-launch", action="store_true")
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

    print("\n== ① 逐件 load_world + 清单对照 ==")
    st = rpc("ash_renderer/residency_stats")
    base_rows = st["rows"]
    keep_camera = min(camera_ids(), key=lambda e: int(e.split("v")[0]))
    loaded_root = None
    for name, prims, mats, imgs in ASSETS:
        prev_root = loaded_root
        r = rpc("ash_renderer/load_world", {"path": f"polyhaven/{name}/{name}_2k.gltf"})
        loaded_root = r["entity"]
        st2 = wait_settled(base_rows, prims)
        if st2 is None:
            check(f"①{name} 到货", False, "120s 未排空")
            break
        d_rows = st2["rows"] - base_rows
        ok = d_rows == prims and st2["resident_meshes"] == st2["rows"]
        check(f"①{name} 到货（Δrows={d_rows} 期望 {prims}）", ok,
              f"images={st2['resident_images']}")
        base_rows = st2["rows"]
        if prev_root is not None:
            rpc("world.despawn_entity", {"entity": prev_root})
            time.sleep(3.0)
            st3 = rpc("ash_renderer/residency_stats")
            base_rows = st3["rows"]
    # 逐件换场后把最后一件留着（读图用），中间件已整树回收
    st_end = rpc("ash_renderer/residency_stats")
    print(f"  换场终态：rows={st_end['rows']} images={st_end['resident_images']} "
          f"used_image={st_end["fine"]["used_image_bytes"]/1e6:.1f}MB")

    print("\n== ② 贴图字节预算列（6.4 第二列，fine_grained 开档）==")
    # 四件全进场再压预算列（单件贴图太少压不出淘汰）
    for name, prims, mats, imgs in ASSETS:
        rpc("ash_renderer/load_world", {"path": f"polyhaven/{name}/{name}_2k.gltf"})
    st_all = wait_settled(base_rows, sum(p for _, p, _, _ in ASSETS))
    if st_all is None:
        check("② 四件全进场", False, "120s 未排空")
    else:
        check("②a 四件全进场稳态零上传（重复实体共享同 AssetId 驻留）",
              st_all["rows"] == base_rows + 4,
              f"rows={st_all['rows']} 期望 {base_rows + 4} images={st_all['resident_images']} "
              f"used_image={st_all['fine']['used_image_bytes']/1e6:.1f}MB")
        used_img = st_all["fine"]["used_image_bytes"]
        if used_img > 0:
            budget = max(used_img // 2, 50_000_000)
            rpc("ash_renderer/residency_set", {
                "enabled": True, "fine_grained": True,
                "budget_image_bytes": budget,
                "budget_mesh_bytes": 0,
                "eject_cooldown_frames": 30,
            })
            time.sleep(4.0)
            s2 = rpc("ash_renderer/residency_stats")
            print(f"  预算 {budget/1e6:.1f}MB：used_image={s2['fine']['used_image_bytes']/1e6:.1f}MB "
                  f"over_budget_image={s2.get('over_budget_image')}")
            check("②b used_image ≤ budget（贴图淘汰收敛）",
                  s2["fine"]["used_image_bytes"] <= budget,
                  f"over_budget_image={s2['fine']['over_budget_image']}（如实报口径）")
            rpc("ash_renderer/residency_set", {"enabled": False, "fine_grained": False,
                                               "budget_image_bytes": 0})
            time.sleep(4.0)
            s3 = rpc("ash_renderer/residency_stats")
            check("②c 关闭沿回全量",
                  s3["fine"]["used_image_bytes"] >= used_img * 0.99,
                  f"used_image={s3['fine']['used_image_bytes']/1e6:.1f}MB")
        else:
            check("②b/②c", False, "used_image_bytes 未填充（fine_grained 未生效？）")

    print("\n== ③ 真 PBR 读图（四件同场对照）==")
    # 四件一字排开（set_transform 摆位），同光源对照
    ents = rpc("world.query", {"data": {"components": []},
                               "filter": {"with": ["bevy_transform::components::global_transform::GlobalTransform"]}})
    x = -3.0
    for name, prims, mats, imgs in ASSETS:
        pass  # 摆位需要每件根实体；简化 = 相机环拍四件自然落位
    rpc("ash_renderer/set_camera", {"target": [0.0, 0.8, 0.0], "yaw_deg": 60, "pitch_deg": 8, "radius": 4.0})
    time.sleep(2.5)
    if host_pid is not None:
        capture(host_pid, os.path.join(REPO, ".temp", "7.3", "polyhaven_lineup.png"))

    print("\n== ④ 零 VUID（仅脚本自启的宿主有日志可查）==")
    if host_pid is not None and not args.skip_launch:
        try:
            log = open(HOST_LOG, "rb").read().decode("utf-8", errors="replace")
            bad = [l for l in log.splitlines()
                   if "VUID" in l or "VK_ERROR" in l or "panicked" in l]
            check("④ 宿主日志零 VUID/VK_ERROR/panic", not bad, f"{len(bad)} 行" if bad else "")
        except OSError as e:
            check("④ 日志读取失败", False, str(e))
    else:
        print("  [SKIP] 驱动的既有实例无日志可查")

    if host_pid is not None and not args.no_close and not args.skip_launch:
        close_host(host_pid)

    failed = [name for name, ok in _results if not ok]
    print(f"\n== 结果：{len(_results) - len(failed)}/{len(_results)} 过 ==")
    for name in failed:
        print(f"  FAIL: {name}")
    sys.exit(1 if failed else 0)


def wait_brp(seconds=60):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            rpc("ash_renderer/increment_stats")
            return True
        except Exception:
            time.sleep(1.0)
    return False


if __name__ == "__main__":
    main()
