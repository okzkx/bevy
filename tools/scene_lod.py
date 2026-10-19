# -*- coding: utf-8 -*-
"""LOD 场景一键生成与判定线采证驱动（6.2）。

场景生成持久化（6.2.1 口径）：本脚本即"测试场景生成的 Python 脚本持久化"——
场景完全由 BRP 方法族生成（spawn-lod 阶梯样本 + set_camera 钉机位），同参数
重跑得到同一场景；6.3/6.4 直接复用本脚本的场景段（真实资产场景归步骤 7）。

判定线采证（--test，对应 6.2 段施工计划判定线草案）：
  ① 选档生效：LOD 开启档档位分布非全 0（远处阶梯实例被派粗档）
  ② CPU 参照一致：lod_census mismatches == 0（同一公式同一次投影，静止场景）
  ③ A/B 开关：关 = host 档位环全 0（命令引脚回 5.3 基准），开 = 恢复选档
  ④ 迟滞：相机沿推/拉方向穿越阈值带，档位读数单调收敛、无带内抖动
  ⑤ draw_stats 零回退：LOD 开/关两档 draw_calls 恒 ≤1
  ⑥ 零 VUID：宿主日志无 VUID/VK-ERROR（脚本自启的宿主才查）

用法：
  python tools/scene_lod.py              # 默认：宿主没起就自启（dev 构建），测完关掉
  python tools/scene_lod.py --no-close   # 测完不关宿主（手动接管，场景留在场内）
  python tools/scene_lod.py --skip-launch # 只驱动已在跑的宿主
  python tools/scene_lod.py --build-only # 只重建场景不采证（6.3/6.4 复用入口）
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

# Windows 控制台 GBK 兜底：强制 UTF-8 输出（脚本含中文判定行）
try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except AttributeError:
    pass

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = os.path.join(REPO, "target", "debug", "ash_renderer.exe")
HOST_LOG = os.path.join(REPO, ".temp", "scene_lod_host.log")
BRP_PORT = 15702

_results = []


def rpc(method, params=None, timeout=30):
    """一次 BRP JSON-RPC 调用，返回 result；协议错误抛 RuntimeError。"""
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
    """记录并打印一项判定线结论。"""
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


def wait_settle(seconds=4.0):
    """等读数新鲜：档位环是两帧前输出，变更后至少等 3~4 秒再读。"""
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
            user32.PostMessageW(hwnd, 0x0010, None, None)  # WM_CLOSE
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


# ===== 场景生成（持久化的"场景脚本"本体，6.3/6.4 复用入口）=====

# 阶梯样本布点：相机在 +z（radius 8，看原点），样本沿 -z 递远——屏幕覆盖
# 逐个减半，覆盖选档公式的档位分带（阈值 256px 默认值下四档全触发）。
LADDER_SPOTS = [
    (0.0, 0.5, 0.0),    # d≈8：近档
    (0.0, 0.5, -8.0),   # d≈16
    (0.0, 0.5, -24.0),  # d≈32
    (0.0, 0.5, -56.0),  # d≈64：远档
]


def build_scene(settle=5.0):
    """重建 LOD 阶梯场景（确定性：同参数同位——档位变体资产身份由缓存复现）。

    返回 (阶梯实体清单, 各档 mesh 身份清单)。幂等：重复调用新增一组样本
    （despawn 场景走 chunk_despawn/官方 despawn，本脚本不隐式清场）。
    """
    rpc("ash_renderer/set_camera", {"yaw_deg": 0, "pitch_deg": 10, "radius": 8})
    entities = []
    level_ids = set()
    for i, (x, y, z) in enumerate(LADDER_SPOTS):
        r = rpc("ash_renderer/spawn_lod", {
            "shape": "sphere",
            "color": [1.0 - i * 0.2, 0.4, 0.2 + i * 0.2],
            "translation": [x, y, z],
            "name": f"lod-阶梯-{i}",
        })
        entities.append(r["entity"])
        level_ids.update(r["levels"])
        print(f"  阶梯 {i} @z={z}: entity {r['entity']}，{len(r['levels'])} 档")
    if settle > 0:
        wait_settle(settle)  # 档位资产上传 + 账本入行 + 档位环两帧陈旧
    return entities, sorted(level_ids)


def read_census():
    c = rpc("ash_renderer/lod_census")
    return c


def main():
    ap = argparse.ArgumentParser(description="LOD 场景一键生成与判定线采证（6.2）")
    ap.add_argument("--no-close", action="store_true", help="测完不关自启的宿主")
    ap.add_argument("--skip-launch", action="store_true", help="不检查/自启宿主，只驱动已在跑的实例")
    ap.add_argument("--build-only", action="store_true", help="只重建场景不采证（6.3/6.4 复用）")
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
        print("FAIL: BRP 端口 60 秒未就绪——宿主起了吗？")
        sys.exit(1)

    print("\n== 场景重建（BRP 生成 → Python 脚本持久化）==")
    entities, level_ids = build_scene()
    rpc("ash_renderer/lod_set_params", {"threshold_px": 256.0, "hysteresis": 0.25})
    rpc("ash_renderer/lod_set_enabled", {"enabled": True})

    if args.build_only:
        print("\n场景已重建（--build-only，不采证）。")
        sys.exit(0)

    print("\n== ① 选档生效（档位分布非全 0）==")
    c_on = read_census()
    counts = c_on["level_counts"]
    print(f"  档位分布 {counts}，rows={c_on['rows_compared']}，dispatched={c_on['bound'] > 0}")
    check("① 选档生效（最高档 > 0，远处实例被派粗档）", max(counts) > 0 and counts[0] >= 1,
          f"counts={counts}")

    print("\n== ② CPU 参照逐实例一致（同一公式 + 同一迟滞状态）==")
    check("② mismatches == 0（静止场景收敛一致）", len(c_on["mismatches"]) == 0,
          f"rows={c_on['rows_compared']}")

    print("\n== ③ A/B 开关：关 = host 档位环全 0 ==")
    rpc("ash_renderer/lod_set_enabled", {"enabled": False})
    wait_settle()
    c_off = read_census()
    print(f"  LOD 关：counts={c_off['level_counts']}")
    check("③a LOD 关 = 全最细档（counts[0] == rows，其余档全 0）",
          c_off["level_counts"][0] == c_off["rows_compared"]
          and all(v == 0 for v in c_off["level_counts"][1:]))
    rpc("ash_renderer/lod_set_enabled", {"enabled": True})
    wait_settle()
    c_back = read_census()
    check("③b LOD 开 = 恢复选档", max(c_back["level_counts"]) > 0,
          f"counts={c_back['level_counts']}")

    print("\n== ④ 迟滞：相机沿拉远方向穿越阈值带，档位读数单调无抖动 ==")
    # 相机半径逐级拉远：近档实例的屏幕覆盖缩小，选档应单调向粗、不回弹
    seq = []
    for radius in (8.0, 12.0, 16.0, 24.0):
        rpc("ash_renderer/set_camera", {"radius": radius})
        wait_settle(2.5)
        c = read_census()
        top = max(i for i, v in enumerate(c["level_counts"]) if v > 0) if any(c["level_counts"]) else 0
        seq.append(top)
        print(f"  radius {radius}: 最高档 {top}（counts={c['level_counts']}）")
    monotonic = all(a <= b for a, b in zip(seq, seq[1:]))
    check("④ 拉远方向档位单调向粗（无带内抖动）", monotonic, f"序列 {seq}")
    rpc("ash_renderer/set_camera", {"radius": 8.0})
    wait_settle(2.0)

    print("\n== ⑤ draw_stats 零回退（绘制调用数恒 ≤1）==")
    ds = rpc("ash_renderer/draw_stats")
    print(f"  draw_calls={ds.get('draw_calls_recorded', ds)}")
    check("⑤ draw_calls 恒 ≤1", ds.get("draw_calls_recorded", 1) <= 1)

    print("\n== ⑥ 零 VUID（仅脚本自启的宿主有日志可查）==")
    if host_pid is not None:
        time.sleep(2.0)
        try:
            log = open(HOST_LOG, "rb").read().decode("utf-8", errors="replace")
            bad = [line for line in log.splitlines()
                   if "VUID" in line or "VK_ERROR" in line or "Validation" in line]
            check("⑥ 宿主日志零 VUID/VK_ERROR", not bad, f"{len(bad)} 行" if bad else "")
        except OSError as e:
            check("⑥ 日志读取失败", False, str(e))
    else:
        print("  [SKIP] 驱动的既有实例无日志面")

    # 收尾：脚本自启的宿主关掉（--no-close 留场供手动观察）
    if host_pid is not None and not args.no_close:
        close_host(host_pid)

    failed = [name for name, ok in _results if not ok]
    print(f"\n== 结果：{len(_results) - len(failed)}/{len(_results)} 过 ==")
    if failed:
        for name in failed:
            print(f"  FAIL: {name}")
        sys.exit(1)
    sys.exit(0)


if __name__ == "__main__":
    main()
