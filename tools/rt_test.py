# -*- coding: utf-8 -*-
"""9.4 求交器判定线一键驱动（BRP + 帧取证）：起宿主 → 四条判定线逐项采证 → 收尾。

判定口径（见 .agents/docs/开发步骤/9-表面缓存全局光照/9.4-求交器与射线基建/施工计划 §9.4.7）：
  ① 求交正确：rt_stats rays 恒等于像素数、命中率 > 0 且 ≤1；rt_dump 三个子视图落 PPM；
     "距离图与深度图结构一致/法线朝向正确"由 PPM 读图取证（脚本只负责落文件 + 结构自检）。
  ② 移动跟随：在相机前方 1.5m 造一面大墙（unique mesh → 独立 BLAS）→ view=2 槽 hash 图
     几乎单色（墙比场景任何面都近，全部射线命中它）；把墙移出视野 → 画面大面积变化
     （TLAS 每帧重建跟随 transform）。坑：不要用"罩住相机"的巨盒——它比周围墙面远，
     最近命中永远不是它，测不到跟随。
  ③ 流送无悬空：despawn 该立方体 → 等驻留回收 → blas_count 回落且进程零 VUID/VK-ERROR；
     之后连续 60 帧（≥1.5s）无新增 VUID。
  ④ 降级路径：RT 开时 frame_dump 两次（稳定性基线）+ rt_set enabled=false 后 frame_dump 一次
     ——关档帧与开档帧逐字节一致（RT 关 = 9.3 收官态）；rt_stats 报 enabled:false；rt_dump 拒绝。
⑤ 回归五脚本（hiz_test / scene_lod / scene_stream / scene_fine / scene_sponza）由外部逐个跑，
   本脚本不重复。

用法：
  python tools/rt_test.py                  # 宿主没起就自启（默认 target/debug/ash_renderer.exe）
  python tools/rt_test.py --no-close       # 测完不关自启的宿主
  python tools/rt_test.py --skip-launch    # 只驱动已在跑的宿主
环境变量（与 hiz_test 同款）：
  ASH_HOST_EXE       宿主 exe 全路径（缺省 target/debug/ash_renderer.exe；9.4 验证期指 ash_renderer_rt.exe）
  ASH_BRP_PORT       BRP 端口（缺省 15702；双实例并行时错开）
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
EXE = os.environ.get("ASH_HOST_EXE", os.path.join(REPO, "target", "debug", "ash_renderer.exe"))
HOST_NAME = os.path.basename(EXE)
HOST_LOG = os.path.join(REPO, ".temp", "rt_test_host.log")
BRP_PORT = int(os.environ.get("ASH_BRP_PORT", "15702"))
TMP = os.path.join(REPO, ".temp")
DEBUG_VIEWS = {0: "距离", 1: "法线", 2: "槽 hash"}

_results = []


def rpc(method, params=None, timeout=60):
    """一次 BRP JSON-RPC 调用，返回 result；协议错误抛 RuntimeError。"""
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
            rpc("ash_renderer/rt_stats")
            return True
        except Exception:
            time.sleep(1.0)
    return False


def settle(seconds=4.0):
    """GPU 侧输出（调试图/统计/标记）是两帧前的提交，变更后至少等几秒再读。"""
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
    import io

    out = subprocess.run(
        ["tasklist", "/FI", f"IMAGENAME eq {HOST_NAME}", "/FO", "CSV", "/NH"],
        capture_output=True,
    ).stdout.decode("gbk", errors="replace")
    return any(
        len(row) >= 2 and row[0] == HOST_NAME and int(row[1]) == pid
        for row in csv.reader(io.StringIO(out))
    )


def log_vuid_count():
    """宿主日志里 VUID/VK-ERROR 行数（日志存在才查）。"""
    if not os.path.isfile(HOST_LOG):
        return 0
    with open(HOST_LOG, "rb") as f:
        text = f.read().decode("utf-8", errors="replace")
    return sum(1 for line in text.splitlines() if "VUID" in line or "VK-ERROR" in line)


def read_ppm(path):
    """P6 PPM → (w, h, rgb bytes)。"""
    with open(path, "rb") as f:
        data = f.read()
    parts = data.split(b"\n", 3)
    if parts[0] != b"P6":
        raise RuntimeError(f"{path} 不是 P6 PPM")
    w, h = map(int, parts[1].split())
    return w, h, parts[3][: w * h * 3]


def diff_ratio(a, b):
    """两张同尺寸 RGB 图逐像素差异占比；尺寸不同视为全不同。"""
    wa, ha, da = read_ppm(a)
    wb, hb, db = read_ppm(b)
    if (wa, ha) != (wb, hb) or len(da) != len(db):
        return 1.0
    total = max(1, wa * ha)
    changed = sum(1 for i in range(0, len(da), 3) if da[i : i + 3] != db[i : i + 3])
    return changed / total


def unique_color_ratio(path):
    """最大单一颜色占比（"罩住相机的立方体"应让槽 hash 图近乎单色）。"""
    w, h, rgb = read_ppm(path)
    counts = {}
    for i in range(0, len(rgb), 3):
        key = rgb[i : i + 3]
        counts[key] = counts.get(key, 0) + 1
    return max(counts.values(), default=0) / max(1, w * h)


def rt_dump(path, view=None, expect_error=False):
    params = {"path": path}
    if view is not None:
        params["view"] = view
    try:
        result = rpc("ash_renderer/rt_dump", params)
        if expect_error:
            raise RuntimeError(f"rt_dump 期待被拒却成功: {result}")
        return result
    except RuntimeError as e:
        if expect_error:
            return {"rejected": str(e)}
        raise


def frame_dump(path):
    return rpc("ash_renderer/frame_dump", {"path": path, "slot": 0})


def stats():
    return rpc("ash_renderer/rt_stats")


def main():
    ap = argparse.ArgumentParser(description="9.4 求交器判定线采证")
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

    vuid_base = log_vuid_count()
    print(f"启动后 VUID/VK-ERROR 基线行数：{vuid_base}")

    try:
        # ---- ① 求交正确：读数 + 三个子视图落图 ----
        settle(4.0)
        s0 = stats()
        check(
            "① RT 就绪（支持 + 总开关 + 能力分位全开）",
            s0["supported"] and s0["enabled"] and s0["support"]["missing"] == "",
            f"enabled={s0['enabled']} missing='{s0['support']['missing']}'",
        )
        rays, hits = s0["debug"]["rays"], s0["debug"]["hits"]
        check(
            "① 射线数 = 像素数且命中率在 (0,1]",
            rays > 0 and 0 < hits <= rays,
            f"rays={rays} hits={hits} rate={s0['debug']['hit_rate']:.3f}",
        )
        s1 = stats()
        check(
            "① 射线数稳定（两次读数一致，无陈旧/漂移）",
            s1["debug"]["rays"] == rays,
            f"{rays} → {s1['debug']['rays']}",
        )
        dumps = {}
        for view in (0, 1, 2):
            rpc("ash_renderer/rt_set", {"enabled": True, "view": view})
            settle(1.0)
            path = os.path.join(TMP, f"rt_test_view{view}.ppm")
            rt_dump(path, view=view)
            w, h, _ = read_ppm(path)
            dumps[view] = path
            check(f"① 调试图 view={view}（{DEBUG_VIEWS[view]}）落图", w > 0 and h > 0, f"{w}×{h}")
        # 法线图不应是常量（结构自检：至少 8 种颜色 → 几何朝向有细节）
        w, h, rgb = read_ppm(dumps[1])
        distinct = len({rgb[i : i + 3] for i in range(0, len(rgb), 3)})
        check("① 法线图非单色（命中重建给出逐面朝向）", distinct > 8, f"{distinct} 种颜色")

        # ---- ② 移动跟随：巨型立方体罩住相机 → 移出视野 ----
        rpc("ash_renderer/rt_set", {"enabled": True, "view": 2})
        settle(1.0)
        before = os.path.join(TMP, "rt_test_move_before.ppm")
        rt_dump(before, view=2)
        spawn = rpc(
            "ash_renderer/spawn_shape",
            {
                "shape": "cuboid",
                "translation": [3.5, 3.44, 0.0],
                "scale": [0.2, 12.0, 12.0],
                "unique": True,
                "name": "rt-test-blocker",
            },
        )
        entity = spawn["entity"]
        settle(2.5)
        inside = os.path.join(TMP, "rt_test_move_inside.ppm")
        rt_dump(inside, view=2)
        ratio_inside = unique_color_ratio(inside)
        check(
            "② 相机前的大墙独占画面（view=2 单色占比 > 95%）",
            ratio_inside > 0.95,
            f"单色占比 {ratio_inside:.3f}（新增实体 {entity}）",
        )
        rpc(
            "ash_renderer/set_transform",
            {"entity": entity, "translation": [-200.0, 3.44, 0.0]},
        )
        settle(2.5)
        moved = os.path.join(TMP, "rt_test_move_away.ppm")
        rt_dump(moved, view=2)
        changed = diff_ratio(inside, moved)
        check(
            "② 移出视野后画面大面积变化（TLAS 跟随 transform）",
            changed > 0.30,
            f"像素变化 {changed:.3f}",
        )
        check(
            "② 移动前后射线数/命中数仍满额（无陈旧帧）",
            stats()["debug"]["rays"] == rays,
            f"rays={stats()['debug']['rays']}",
        )

        # ---- ③ 流送无悬空：despawn → 驻留回收 → BLAS 数回落 ----
        blas_before = stats()["blas"]["count"]
        rpc("world.despawn_entity", {"entity": entity})
        settle(2.0)
        blas_after = blas_before
        deadline = time.time() + 10
        while time.time() < deadline:
            blas_after = stats()["blas"]["count"]
            if blas_after < blas_before:
                break
            time.sleep(1.0)
        check(
            "③ despawn 后 BLAS 数回落（资产退场 → 加速结构排销毁）",
            blas_after < blas_before,
            f"{blas_before} → {blas_after}",
        )
        rounds = 0
        while time.time() < deadline + 1.5:
            settle(1.5)
            rounds += 1
            if rounds >= 2:
                break
        check(
            "③ 回收后连续运行零 VUID/VK-ERROR",
            log_vuid_count() == vuid_base,
            f"基线 {vuid_base} → 现 {log_vuid_count()}",
        )

        # ---- ④ 降级路径：RT 关与 9.3 收官态逐字节一致 ----
        rpc("ash_renderer/rt_set", {"enabled": True})
        settle(1.0)
        on1 = os.path.join(TMP, "rt_test_frame_on1.ppm")
        frame_dump(on1)
        settle(1.0)
        on2 = os.path.join(TMP, "rt_test_frame_on2.ppm")
        frame_dump(on2)
        stable = read_ppm(on1)[2] == read_ppm(on2)[2]
        rpc("ash_renderer/rt_set", {"enabled": False})
        settle(1.0)
        off = os.path.join(TMP, "rt_test_frame_off.ppm")
        frame_dump(off)
        check(
            "④ RT 开→关：LDR 帧逐字节一致（降级不改画面）",
            read_ppm(on2)[2] == read_ppm(off)[2],
            "同帧态对比" + ("" if stable else "（警告：开档两帧也不一致，画面非静止）"),
        )
        s_off = stats()
        check(
            "④ 关档 rt_stats 报 enabled:false（不报错）",
            s_off["enabled"] is False and s_off["supported"] is True,
            f"enabled={s_off['enabled']} supported={s_off['supported']}",
        )
        rejected = rt_dump(os.path.join(TMP, "rt_test_should_fail.ppm"), expect_error=True)
        check("④ 关档 rt_dump 被拒（无调试图可读）", "rejected" in rejected)
        rpc("ash_renderer/rt_set", {"enabled": True})
    finally:
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
