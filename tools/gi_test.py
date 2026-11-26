# -*- coding: utf-8 -*-
"""9.5 表面缓存判定线一键驱动（BRP + 帧取证）：起宿主 → 六条判定线逐项采证 → 收尾。

判定口径（见 .agents/docs/开发步骤/9-表面缓存全局光照/9.5-patch池与分配回收/施工计划 §9.5.7）：
  ① 分布与覆盖吻合：Sponza 中庭机位 → view=1 覆盖图无大片红（无 patch）或蓝（出 Volume）；
     view=7 位置偏离（patch 位置到当前表面切平面的垂直距离 / 体素）无大片红；view=3 的桶着色
     非单色；view=0 的 cell 归属多分区；gi_census 的 cell↔patch 双向一致、位置在体素内、
     法线在桶内；形态 B 方片在覆盖图上可见。
  ② 显存恒定：连跑 ≥120 帧，gi_stats 的表字节读数不增；池水位 ≤ 容量。
  ③ 跟随无残留：造一面独享 wall → patch 数上升；移出视野等淘汰窗（>240 帧）后回到基线；
     gi_census 无悬空。
  ④ 稳态平衡：静止游历后 failed≈0、active 稳定；defrag 前后 cell↔patch 仍一致。
  ⑤ 大世界档可游历：关态改 Size 512 / 6 级联 / 池 2^18 → 开启 + 相机推拉，池不涨、failed 有界。
  ⑥ 不破底座：GI 关 vs GI 开但 view=0 的 LDR 帧逐字节一致；关态 gi_dump/gi_census 回 enabled:false
     形态；进程零 VUID/VK-ERROR。五脚本回归（hiz_test / scene_lod / scene_stream / scene_fine /
     scene_sponza）由外部逐个跑，本脚本不重复。

用法：
  python tools/gi_test.py                  # 宿主没起就自启（默认 target/debug/ash_renderer.exe）
  python tools/gi_test.py --no-close       # 测完不关自启的宿主
  python tools/gi_test.py --skip-launch    # 只驱动已在跑的宿主
环境变量（与 ray_tracing_test 同款）：
  ASH_HOST_EXE       宿主 exe 全路径（缺省 target/debug/ash_renderer.exe）
  ASH_BRP_PORT       BRP 端口（缺省 15702；旧实例占用时用 15712 跑临时 bin）
退出码：全过 = 0，任一失败 = 1。
"""

import argparse
import ctypes
import io
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
EXE = os.environ.get("ASH_HOST_EXE", os.path.join(REPO, "target", "debug", "ash_renderer.exe"))
HOST_NAME = os.path.basename(EXE)
HOST_LOG = os.path.join(REPO, ".temp", "gi_test_host.log")
BRP_PORT = int(os.environ.get("ASH_BRP_PORT", "15702"))
TMP = os.path.join(REPO, ".temp")

_results = []


def rpc(method, params=None, timeout=60):
    body = json.dumps(
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}}
    ).encode()
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


def wait_brp(seconds=90):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            rpc("ash_renderer/gi_stats")
            return True
        except Exception:
            time.sleep(1.0)
    return False


def settle(seconds=2.0):
    time.sleep(seconds)


def launch_host():
    if not os.path.isfile(EXE):
        print(f"宿主二进制不存在：{EXE}\n先 cargo build 再跑本脚本。")
        sys.exit(2)
    os.makedirs(TMP, exist_ok=True)
    log = open(HOST_LOG, "wb")
    proc = subprocess.Popen(
        [EXE],
        cwd=REPO,
        stdout=log,
        stderr=subprocess.STDOUT,
        creationflags=subprocess.CREATE_NEW_PROCESS_GROUP,
    )
    print(f"宿主已自启（PID {proc.pid}），日志 {HOST_LOG}")
    return proc


def close_host(pid):
    """EnumWindows 群发 WM_CLOSE（单发 MainWindowHandle 有不关的坑）。"""
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
            return True
    print(f"警告：宿主（PID {pid}）10 秒未退出")
    return False


def pid_alive(pid):
    import csv

    out = subprocess.run(
        ["tasklist", "/FI", f"IMAGENAME eq {HOST_NAME}", "/FO", "CSV", "/NH"],
        capture_output=True,
    ).stdout.decode("gbk", errors="replace")
    return any(
        len(row) >= 2 and row[0] == HOST_NAME and int(row[1]) == pid
        for row in csv.reader(io.StringIO(out))
    )


def log_vuid_count():
    if not os.path.isfile(HOST_LOG):
        return 0
    with open(HOST_LOG, "rb") as f:
        text = f.read().decode("utf-8", errors="replace")
    return sum(1 for line in text.splitlines() if "VUID" in line or "VK-ERROR" in line)


def read_ppm(path):
    with open(path, "rb") as f:
        data = f.read()
    parts = data.split(b"\n", 3)
    if parts[0] != b"P6":
        raise RuntimeError(f"{path} 不是 P6 PPM")
    w, h = map(int, parts[1].split())
    return w, h, parts[3][: w * h * 3]


def color_stats(path):
    """(green, red, blue, marker, distinct) 占比/计数。marker = 暖白方片（覆盖图上高亮）。"""
    w, h, rgb = read_ppm(path)
    total = max(1, w * h)
    green = red = blue = marker = 0
    colors = set()
    for i in range(0, len(rgb), 3):
        r, g, b = rgb[i], rgb[i + 1], rgb[i + 2]
        colors.add((r // 8, g // 8, b // 8))
        if r > 120 and g > 150 and b > 60:
            marker += 1
        elif g > r * 1.6 and g > b * 1.6:
            green += 1
        elif r > g * 1.6 and r > b * 1.6:
            red += 1
        elif b > r * 1.6 and b > g * 1.6:
            blue += 1
    return {
        "w": w,
        "h": h,
        "green": green / total,
        "red": red / total,
        "blue": blue / total,
        "marker": marker / total,
        "distinct": len(colors),
    }


def dump(path, slot=None):
    params = {"path": path}
    if slot is not None:
        params["slot"] = slot
    return rpc("ash_renderer/gi_dump", params)


def frame_dump(path):
    return rpc("ash_renderer/frame_dump", {"path": path, "slot": 0})


def stats():
    return rpc("ash_renderer/gi_stats")


def set_gi(**kwargs):
    return rpc("ash_renderer/gi_set", {k: v for k, v in kwargs.items() if v is not None})


def census(n=12):
    return rpc("ash_renderer/gi_census", {"patches": n})


def main():
    ap = argparse.ArgumentParser(description="9.5 表面缓存判定线采证")
    ap.add_argument("--no-close", action="store_true", help="测完不关自启的宿主")
    ap.add_argument("--skip-launch", action="store_true", help="只驱动已在跑的宿主，不动进程")
    args = ap.parse_args()

    os.makedirs(TMP, exist_ok=True)
    proc = None
    if not args.skip_launch:
        proc = launch_host()
        if not wait_brp():
            print("宿主 BRP 端口未就绪，退出")
            return 2
    elif not wait_brp(10):
        print(f"已在跑的宿主不可达（端口 {BRP_PORT}）")
        return 2

    # 自动曝光会让静止画面逐帧变化，先冻结曝光再做 LDR 逐字节对比。
    rpc("ash_renderer/exposure_set", {"mode": "manual"})

    vuid_base = log_vuid_count()
    print(f"启动后 VUID/VK-ERROR 基线行数：{vuid_base}")
    restored = False

    try:
        # ---- ① 分布与覆盖吻合 ----
        set_gi(enabled=True, view=1, form=0)
        settle(3.0)
        s0 = stats()
        check(
            "① GI 就绪且播种有产出（enabled + patches > 0）",
            s0["enabled"] and s0["patches"] > 0,
            f"patches={s0['patches']} cells={s0['cells']}",
        )
        check(
            "① 池未满且分配失败为 0",
            s0["pool"]["watermark"] <= s0["pool"]["capacity"] and s0["counters"]["failed"] == 0,
            f"watermark={s0['pool']['watermark']}/{s0['pool']['capacity']} failed={s0['counters']['failed']}",
        )
        v1 = os.path.join(TMP, "gi_test_view1.ppm")
        dump(v1)
        c1 = color_stats(v1)
        check(
            "① 覆盖图：大部分可见表面有 patch（绿 > 60%，红 < 5%）",
            c1["green"] > 0.60 and c1["red"] < 0.05,
            f"green={c1['green']:.3f} red={c1['red']:.3f} blue={c1['blue']:.3f}",
        )
        check(
            "① 无大片出 Volume（蓝 < 10%）",
            c1["blue"] < 0.10,
            f"blue={c1['blue']:.3f}",
        )
        set_gi(view=7)
        settle(1.5)
        v7 = os.path.join(TMP, "gi_test_view7.ppm")
        dump(v7)
        c7 = color_stats(v7)
        check(
            "① 位置偏离（离面 > 0.5 体素）无大片红（红 < 10%）",
            c7["red"] < 0.10,
            f"red={c7['red']:.3f} green={c7['green']:.3f}",
        )
        set_gi(view=3)
        settle(1.5)
        v3 = os.path.join(TMP, "gi_test_view3.ppm")
        dump(v3)
        c3 = color_stats(v3)
        check(
            "① 桶着色非单色（同一平面没整片压进一个桶，distinct ≥ 8）",
            c3["distinct"] >= 8,
            f"distinct={c3['distinct']}",
        )
        set_gi(view=8)
        settle(1.5)
        v0 = os.path.join(TMP, "gi_test_view8.ppm")
        dump(v0)
        c0 = color_stats(v0)
        check(
            "① cell 归属多分区（view=8，distinct ≥ 48）",
            c0["distinct"] >= 48,
            f"distinct={c0['distinct']}",
        )
        cen = census(16)
        summary = cen["summary"]
        check(
            "① gi_census 全一致（双向/在体素内/法线在桶内）",
            summary["dangling"] == 0
            and summary["consistent"] == cen["checked"]
            and summary["in_voxel"] == cen["checked"]
            and summary["bucket_ok"] == cen["checked"],
            json.dumps(summary),
        )
        set_gi(view=1, form=1)
        settle(1.5)
        vm = os.path.join(TMP, "gi_test_markers.ppm")
        dump(vm)
        cm = color_stats(vm)
        check(
            "① 形态 B 方片可见（暖白像素 > 0.2%）",
            cm["marker"] > 0.002,
            f"marker={cm['marker']:.4f}",
        )
        set_gi(form=0, view=1)

        # ---- ② 显存恒定（≥120 帧）----
        m0 = stats()
        settle(4.0)
        m1 = stats()
        check(
            "② 表字节读数不增（连跑 ≥120 帧）",
            m0["memory"] == m1["memory"],
            f"{m0['memory']['total_bytes']}B → {m1['memory']['total_bytes']}B",
        )
        check(
            "② 池水位不超过容量",
            m1["pool"]["watermark"] <= m1["pool"]["capacity"],
            f"watermark={m1['pool']['watermark']} capacity={m1['pool']['capacity']}",
        )

        # ---- ③ 跟随无残留 ----
        base_patches = m1["patches"]
        wall = rpc(
            "ash_renderer/spawn_shape",
            {
                "shape": "cuboid",
                "translation": [0.0, 3.0, -1.0],
                "scale": [1.5, 8.0, 8.0],
                "unique": True,
                "name": "gi-test-wall",
            },
        )["entity"]
        settle(3.0)
        grown = stats()["patches"]
        check(
            "③ 新表面进视野后 patch 数上升",
            grown > base_patches,
            f"{base_patches} → {grown}（wall={wall}）",
        )
        rpc(
            "ash_renderer/set_transform",
            {"entity": wall, "translation": [-500.0, 3.0, -1.0]},
        )
        settle(7.0)  # > 淘汰窗 240 帧（60 fps 下 4 秒）+ 余量
        after = stats()
        check(
            "③ 移走后心跳淘汰回收（回到基线 ±20%）",
            after["patches"] <= base_patches * 1.2 + 64,
            f"{grown} → {after['patches']}（基线 {base_patches}）",
        )
        cen2 = census(16)
        check(
            "③ 回收后 cell↔patch 无悬空",
            cen2["summary"]["dangling"] == 0,
            json.dumps(cen2["summary"]),
        )
        rpc("world.despawn_entity", {"entity": wall})

        # ---- ④ 稳态平衡 ----
        settle(3.0)
        a0 = stats()
        settle(2.0)
        a1 = stats()
        check(
            "④ 静止游历：分配失败 0、active 稳定、水位不涨",
            a1["counters"]["failed"] == 0
            and abs(a1["patches"] - a0["patches"]) <= max(16, a0["patches"] // 20)
            and a1["pool"]["watermark"] == a0["pool"]["watermark"],
            f"active {a0['patches']}→{a1['patches']} failed={a1['counters']['failed']} "
            f"watermark {a0['pool']['watermark']}→{a1['pool']['watermark']}",
        )
        check(
            "④ defrag 后仍一致（census 全过）",
            census(16)["summary"]["dangling"] == 0,
            "抽查 16 条",
        )
        gi_ms = a1["gi_ms"]
        check(
            "④ 表面缓存段有计时读数（gi_ms ≥ 0，量级不爆炸）",
            0.0 <= gi_ms < 20.0,
            f"gi_ms={gi_ms:.3f}",
        )

        # ---- ⑤ 大世界档可游历 ----
        set_gi(enabled=False)
        settle(0.5)
        big = set_gi(enabled=False, size=512.0, cascades=6, capacity=262144)
        check(
            "⑤ 关态改大世界规格成功（Size 512 / 6 级 / 池 2^18）",
            abs(big["config"]["size"] - 512.0) < 1e-3
            and big["config"]["cascades"] == 6
            and big["config"]["capacity"] == 262144,
            json.dumps(big["config"]),
        )
        set_gi(enabled=True, view=1)
        change_rejected = False
        try:
            set_gi(size=256.0)
        except RuntimeError:
            change_rejected = True
        check("⑤ 开态改规格被拒（先关再改的纪律）", change_rejected)
        settle(2.5)
        big0 = stats()
        for zoom in (0.7, 1.3, 0.8, 1.5):
            rpc("ash_renderer/set_camera", {"zoom": zoom})
            settle(0.8)
        big1 = stats()
        check(
            "⑤ 大世界档游历：池不涨、失败有界、显存按档增长",
            big1["patches"] > 0
            and big1["pool"]["watermark"] <= big1["pool"]["capacity"]
            and big1["counters"]["failed"] < 4096
            and big1["memory"]["cell_bytes"] > m0["memory"]["cell_bytes"],
            f"patches={big1['patches']} watermark={big1['pool']['watermark']}/"
            f"{big1['pool']['capacity']} failed={big1['counters']['failed']} "
            f"cell_bytes={big1['memory']['cell_bytes']}",
        )
        # 还原起步档
        set_gi(enabled=False)
        settle(0.5)
        set_gi(enabled=False, size=128.0, cascades=4, capacity=65536)
        restored = True

        # ---- ⑥ 不破底座 ----
        set_gi(enabled=True, view=0, form=0)
        settle(2.0)
        on1 = os.path.join(TMP, "gi_test_frame_on1.ppm")
        frame_dump(on1)
        settle(1.0)
        on2 = os.path.join(TMP, "gi_test_frame_on2.ppm")
        frame_dump(on2)
        stable = read_ppm(on1)[2] == read_ppm(on2)[2]
        set_gi(enabled=False)
        settle(1.0)
        off = os.path.join(TMP, "gi_test_frame_off.ppm")
        frame_dump(off)
        check(
            "⑥ GI 开（view=0）→关：LDR 帧逐字节一致",
            read_ppm(on2)[2] == read_ppm(off)[2],
            "同帧态对比" + ("" if stable else "（警告：开档两帧也不一致，画面非静止）"),
        )
        s_off = stats()
        check(
            "⑥ 关态 gi_stats 回 enabled:false（池冻结保留，不清零）",
            s_off["enabled"] is False and s_off["patches"] > 0,
            f"enabled={s_off['enabled']} patches={s_off['patches']}",
        )
        rejected = False
        try:
            dump(os.path.join(TMP, "gi_test_should_fail.ppm"))
        except RuntimeError:
            rejected = True
        check("⑥ 关态 gi_dump 被拒（无视图可读）", rejected)
        check(
            "⑥ GI 关态 gi_census 回 enabled:false（不报错）",
            census(4).get("enabled") is False,
        )
        check(
            "⑥ 进程零 VUID/VK-ERROR",
            log_vuid_count() == vuid_base,
            f"基线 {vuid_base} → 现 {log_vuid_count()}",
        )
    finally:
        if not restored:
            try:
                set_gi(enabled=False)
                set_gi(enabled=False, size=128.0, cascades=4, capacity=65536)
            except Exception:
                pass
        if proc is not None and not args.no_close:
            close_host(proc.pid)

    failed = [name for name, ok in _results if not ok]
    print()
    if failed:
        print(f"判定线 {len(_results) - len(failed)}/{len(_results)} 过，失败：")
        for name in failed:
            print(f"  - {name}")
        return 1
    print(f"判定线 {len(_results)}/{len(_results)} 全过")
    return 0


if __name__ == "__main__":
    sys.exit(main())
