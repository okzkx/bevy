# -*- coding: utf-8 -*-
"""Hi-Z 遮挡一键测试驱动（6.1）：起宿主 → 摆遮挡场景 → 按判定线逐项采证 → 收尾。

对应 6.1.6 验收回归的核心序列（见 .agents/docs/6-高级剔除LOD与流送/6.1-Hi-Z遮挡/）。
全程 BRP 驱动，不碰鼠标。判定口径与段施工记录一致：
  ① 遮挡生效：遮挡开启档 gpu_visible < cpu_visible（被遮挡实体被合法剔除）
  ② 探针一致：occlusion_probe 判被挡物 occluded=true（与 shader 逐位同语义）
  ③ 金字塔内容：hiz-census 指定级 max > 0（归约链活着，非全 0/全 1）
  ④ A/B 开关：关 = host 全可见（gpu==cpu），开 = 恢复剔除
  ⑤ 恢复：被挡物移出遮挡后数帧内回到可见集（不长期误剔除）
  ⑥ 零 VUID：宿主日志无 VUID/VK-ERROR（脚本自启的宿主才查）

用法：
  python tools/hiz_test.py             # 默认：宿主没起就自启（dev 构建），测完关掉自启的宿主
  python tools/hiz_test.py --no-close  # 测完不关宿主（手动接管）
  python tools/hiz_test.py --skip-launch  # 只驱动已在跑的宿主，不动进程
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
HOST_LOG = os.path.join(REPO, ".temp", "hiz_test_host.log")
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
    """轮询等 BRP 端口就绪。"""
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            rpc("ash_renderer/increment_stats")
            return True
        except Exception:
            time.sleep(1.0)
    return False


def wait_settle(seconds=4.0):
    """等读数新鲜：GPU 标记是两帧前输出，变更后至少等 3~4 秒再读。"""
    time.sleep(seconds)


def launch_host():
    """自启宿主（stdout/stderr 落 .temp 日志，收尾供零 VUID 检查）。返回进程句柄。"""
    if not os.path.isfile(EXE):
        print(f"宿主二进制不存在：{EXE}\n先 cargo build 再跑本脚本。")
        sys.exit(2)
    os.makedirs(os.path.dirname(HOST_LOG), exist_ok=True)
    log = open(HOST_LOG, "wb")
    # UseShellExecute 等价：不重定向管道则消息泵不被堵死；文件重定向安全（PowerShell 冒烟三坑同源）
    proc = subprocess.Popen(
        [EXE], cwd=REPO, stdout=log, stderr=subprocess.STDOUT,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP,
    )
    print(f"宿主已自启（PID {proc.pid}），日志 {HOST_LOG}")
    return proc


def close_host(pid):
    """EnumWindows 群发 WM_CLOSE 关宿主（单发 MainWindowHandle 有不关的坑）。"""
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
    """返回 [(pid, name)]（tasklist CSV），给存活判断用。"""
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


def main():
    ap = argparse.ArgumentParser(description="Hi-Z 遮挡一键测试（6.1 判定线采证）")
    ap.add_argument("--no-close", action="store_true", help="测完不关自启的宿主")
    ap.add_argument("--skip-launch", action="store_true", help="不检查/自启宿主，只驱动已在跑的实例")
    args = ap.parse_args()

    host_pid = None
    if not args.skip_launch:
        alive = [p[0] for p in iter_processes()]
        if alive:
            print(f"检测到宿主已在跑（PID {alive[0]}），直接驱动它（测完不关）。")
        else:
            proc = launch_host()
            host_pid = proc.pid
    if not wait_brp():
        print("FAIL: BRP 端口 60 秒未就绪——宿主起了吗？")
        sys.exit(1)

    print("\n== ① 场景布置：钉机位（遮挡样本必须沿相机→目标视线摆，相机在 +z 侧）==")
    # target 显式钉死（6.1 几何）：7.1 中庭机位改了默认环绕中心后，不钉 target 会让
    # ⑤ 恢复位 (0,0.3,5.5) 落到 45° 垂直 FOV 之外（CPU 视锥正确判不可见，测试假阴性）
    orbit = rpc("ash_renderer/set_camera", {"yaw_deg": 0, "pitch_deg": 10, "radius": 8,
                                            "target": [0.0, 0.0, 0.0]})["orbit"]
    print(f"  相机 yaw {orbit['yaw_deg']:.1f} pitch {orbit['pitch_deg']:.1f} radius {orbit['radius']:.1f} target {orbit['target']}")

    print("\n== 摆遮挡对：墙（cuboid 非均匀缩放）+ 被挡球 ==")
    wall = rpc("ash_renderer/spawn_shape", {
        "shape": "cuboid", "color": [0.3, 0.3, 0.3],
        "translation": [0.0, 0.3, 3.2], "scale": [10.0, 6.0, 0.5], "name": "遮挡墙",
    })
    ball = rpc("ash_renderer/spawn_shape", {
        "shape": "sphere", "color": [1.0, 0.0, 0.0],
        "translation": [0.0, 0.3, 0.0], "name": "被挡物",
    })
    wall_e, ball_e = wall["entity"], ball["entity"]
    print(f"  墙 {wall_e}（mesh_created={wall['mesh_created']}）/ 球 {ball_e}（mesh_created={ball['mesh_created']}）")
    wait_settle(5.0)  # 新 mesh 上传 + 账本入行 + 金字塔 warmup + 标记环两帧陈旧

    print("\n== ① 遮挡生效（遮挡开启档）==")
    cs = rpc("ash_renderer/cull_stats")
    occ = cs["occlusion"]
    print(f"  gpu_visible={cs['gpu_visible']} / cpu_visible={cs['cpu_visible']}，"
          f"occlusion dispatched={occ['dispatched']} fallback={occ['fallback']} settings={occ['settings_enabled']}")
    check("① 遮挡剔除生效（gpu_visible < cpu_visible）",
          cs["gpu_visible"] < cs["cpu_visible"] and occ["dispatched"] and occ["fallback"] is None,
          f"{cs['gpu_visible']}/{cs['cpu_visible']}，mismatches={len(cs['mismatches'])}（=被遮挡清单）")

    print("\n== ② CPU 探针对照（被挡物判定中间量）==")
    probe = rpc("ash_renderer/occlusion_probe", {"entity": ball_e})["rows"][0]
    if "occluded" in probe:
        print(f"  球：level={probe['level']} taps={[round(t,4) for t in probe['taps']]} "
              f"occluder={probe['occluder']:.4f} obj_depth_max={probe['obj_depth_max']:.4f} occluded={probe['occluded']}")
        check("② 探针判被挡物 occluded=true（obj_depth_max < occluder，reverse-Z）", bool(probe["occluded"]))
    else:
        # w≤0 早退 = 摆位在相机背后（场景布置问题，不是遮挡机制问题）
        print(f"  探针早退：{probe.get('reason', '未知')}——检查机位与墙的位置")
        check("② 探针判被挡物 occluded", False, "角点投影 w≤0（样本在相机背后）")

    print("\n== ③ 金字塔内容（归约链活性）==")
    census = rpc("ash_renderer/hiz_census", {"level": 5})["levels"][0]
    print(f"  level {census['level']} {census['size']}: min={census['min']:.4f} max={census['max']:.4f} "
          f"zero_ratio={census['zero_ratio']:.3f}")
    check("③ 金字塔内容非退化（0 < max，归约链在写）", 0.0 < census["max"] < 1.0,
          f"max={census['max']:.4f}")

    print("\n== ④ A/B 开关：关 = host 全可见 ==")
    rpc("ash_renderer/occlusion_set_enabled", {"enabled": False})
    wait_settle()
    cs_off = rpc("ash_renderer/cull_stats")
    print(f"  遮挡关：gpu_visible={cs_off['gpu_visible']} / cpu={cs_off['cpu_visible']} "
          f"mismatches={len(cs_off['mismatches'])}")
    check("④a 遮挡关 = host 全可见（gpu == cpu）",
          cs_off["gpu_visible"] == cs_off["cpu_visible"] and not cs_off["occlusion"]["dispatched"])
    rpc("ash_renderer/occlusion_set_enabled", {"enabled": True})
    wait_settle()
    cs_on = rpc("ash_renderer/cull_stats")
    check("④b 遮挡开 = 恢复剔除", cs_on["gpu_visible"] < cs_on["cpu_visible"],
          f"{cs_on['gpu_visible']}/{cs_on['cpu_visible']}")

    print("\n== ⑤ 恢复：被挡物移到墙前（z=5.5，相机侧），数帧内应回到可见集 ==")
    rpc("ash_renderer/set_transform", {"entity": ball_e, "translation": [0.0, 0.3, 5.5]})
    wait_settle()
    cs_rec = rpc("ash_renderer/cull_stats")
    still_hidden = [r["entity"] for r in cs_rec["rows"] if r["entity"] == ball_e and not r["gpu"]]
    print(f"  恢复后 gpu_visible={cs_rec['gpu_visible']}，球仍在被剔清单：{bool(still_hidden)}")
    check("⑤ 移出遮挡数帧内恢复（不长期误剔除）", not still_hidden)
    # 把球放回遮挡位（场景还原，供手动继续观察）
    rpc("ash_renderer/set_transform", {"entity": ball_e, "translation": [0.0, 0.3, 0.0]})

    print("\n== ⑥ 金字塔构建开销（gpu_hiz_ms）==")
    perf = rpc("ash_renderer/perf_stats")
    hiz = perf.get("gpu_hiz_ms", {})
    if hiz:
        print(f"  p50={hiz.get('p50', 0):.3f}ms p95={hiz.get('p95', 0):.3f}ms n={hiz.get('n', 0)}")

    print("\n== ⑦ 零 VUID（仅脚本自启的宿主有日志可查）==")
    if host_pid is not None:
        time.sleep(2.0)
        vuid = 0
        if os.path.isfile(HOST_LOG):
            with open(HOST_LOG, "r", encoding="utf-8", errors="replace") as f:
                vuid = sum(1 for line in f if "VUID" in line or "VK-ERROR" in line)
        check("⑦ 宿主日志零 VUID/VK-ERROR", vuid == 0, f"命中 {vuid} 条")
    else:
        print("  （跳过：宿主非脚本自启，无日志）")

    # —— 收尾
    passed = sum(1 for _, ok in _results if ok)
    total = len(_results)
    print(f"\n==== 结论：{passed}/{total} 过 ====")
    for name, ok in _results:
        print(f"  [{'PASS' if ok else 'FAIL'}] {name}")
    if host_pid is not None and not args.no_close:
        close_host(host_pid)
    sys.exit(0 if passed == total else 1)


if __name__ == "__main__":
    main()
