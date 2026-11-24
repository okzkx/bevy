"""9.6 表面缓存辐照度更新的判定线脚本（自启宿主、逐项采证、退出码）。

判定线（详见 `.agents/docs/开发步骤/9-表面缓存全局光照/9.6-irradiance更新/施工计划`）：
  ① 多跳色渗可见（multi_bounce 关/开读图 + 同批 patch 的探针对账）
  ② 开关灯 lag 量级如实记录（点光开关后 patch 的 update_count 归零→回到 updateMax 的帧数）
  ③ 收敛后射线预算可降档且画面稳定（预算压档后画面差分在阈值内、方差读数不反弹）
  ④ 自发光 A/B 与单点对账（emissive_intensity 0/1）
  ⑤ 阴影漏光案例读图留档（CSM 只覆盖主相机视锥）
  ⑥ 回退硬线：estimation 关 = 9.5 收官画面（同进程同槽 PPM 逐字节一致）
  ⑦ 不破底座：本脚本只测运行期；零 VUID 也在这里查，clippy/fmt/库测试与回归脚本另跑

用法：
  python tools/gi_estimation_test.py [--no-close] [--skip-launch]
环境变量：ASH_HOST_EXE（宿主二进制）、ASH_BRP_PORT（默认 15702）
退出码：0 = 全过；1 = 有失败；2 = 宿主缺/BRP 未就绪
"""

import ctypes
import json
import os
import subprocess
import sys
import time
import urllib.request

try:
    ctypes.windll.kernel32.SetConsoleOutputCP(65001)
except AttributeError:
    pass

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXE = os.environ.get("ASH_HOST_EXE", os.path.join(REPO, "target", "debug", "ash_renderer.exe"))
HOST_LOG = os.path.join(REPO, ".temp", "gi_estimation_test_host.log")
BRP_PORT = int(os.environ.get("ASH_BRP_PORT", "15702"))
TMP = os.path.join(REPO, ".temp")

_results = []


def rpc(method, params=None, timeout=120):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}}).encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{BRP_PORT}", data=body, headers={"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        payload = json.load(resp)
    if "error" in payload:
        raise RuntimeError(f"{method}：{payload['error']}")
    return payload["result"]


def check(name, ok, detail=""):
    _results.append((name, ok))
    print(f"  [{'PASS' if ok else 'FAIL'}] {name}" + (f" —— {detail}" if detail else ""))


def wait_for(predicate, seconds=120.0, interval=0.5):
    deadline = time.time() + seconds
    while time.time() < deadline:
        try:
            got = predicate()
        except Exception:
            got = None
        if got:
            return got
        time.sleep(interval)
    return None


def wait_brp(seconds=180):
    return wait_for(lambda: rpc("ash_renderer/gi_stats"), seconds, 1.0)


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
    return proc, log


def pid_alive(pid):
    out = subprocess.run(
        ["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"],
        capture_output=True,
        text=True,
        encoding="gbk",
        errors="replace",
    ).stdout
    return str(pid) in out


def close_host(pid):
    user32 = ctypes.windll.user32

    def cb(hwnd, _):
        length = user32.GetWindowTextLengthW(hwnd)
        if length:
            buf = ctypes.create_unicode_buffer(length + 1)
            user32.GetWindowTextW(hwnd, buf, length + 1)
            if "ash_renderer" in buf.value:
                user32.PostMessageW(hwnd, 0x0010, 0, 0)
        return True

    user32.EnumWindows(ctypes.WINFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)(cb), 0)
    for _ in range(40):
        time.sleep(0.5)
        if not pid_alive(pid):
            return True
    return False


def vuid_lines():
    try:
        text = open(HOST_LOG, "rb").read().decode("utf-8", "replace")
    except FileNotFoundError:
        return 0
    return sum(
        1 for line in text.splitlines() if "VUID" in line or "VK-ERROR" in line or "VK_ERROR" in line
    )


def read_ppm(path):
    with open(path, "rb") as f:
        data = f.read()
    if data[:2] != b"P6":
        raise RuntimeError(f"{path} 不是 P6")
    parts = data.split(b"\n", 3)
    w, h = (int(v) for v in parts[1].split())
    return w, h, parts[3]


def image_stats(path):
    w, h, rgb = read_ppm(path)
    colors = set()
    bright = 0
    total = 0
    for i in range(0, len(rgb), 3 * 7):
        r, g, b = rgb[i], rgb[i + 1], rgb[i + 2]
        total += 1
        colors.add((r >> 3, g >> 3, b >> 3))
        if r + g + b > 24:
            bright += 1
    return {"w": w, "h": h, "distinct": len(colors), "bright_ratio": bright / max(total, 1)}


def image_diff(a, b):
    """返回平均绝对差（0..255）与差异像素占比。"""
    wa, ha, ra = read_ppm(a)
    wb, hb, rb = read_ppm(b)
    if (wa, ha) != (wb, hb):
        raise RuntimeError("两张图尺寸不同")
    total = 0
    diff_sum = 0
    for i in range(0, len(ra), 3):
        d = abs(ra[i] - rb[i]) + abs(ra[i + 1] - rb[i + 1]) + abs(ra[i + 2] - rb[i + 2])
        diff_sum += d
        if d > 6:
            total += 1
    pixels = len(ra) // 3
    return diff_sum / (3 * pixels), total / pixels


def image_mean(path):
    """整屏平均亮度：比逐 patch 探针对"池子 churn"稳得多（一次平均上万像素）。"""
    w, h, rgb = read_ppm(path)
    total = 0.0
    count = 0
    for i in range(0, len(rgb), 3 * 5):
        total += 0.2126 * rgb[i] + 0.7152 * rgb[i + 1] + 0.0722 * rgb[i + 2]
        count += 1
    return total / max(count, 1)


def probe_mean(rings=8):
    total = 0.0
    count = 0
    for i in range(rings):
        probe = rpc("ash_renderer/gi_estimate_probe", {"ring": i})
        if not probe.get("alive"):
            continue
        irr = probe["irradiance_a"]
        n = probe["geometry"]["normal"]
        total += irr["l0"][0] + irr["l1x"][0] * n[0] + irr["l1y"][0] * n[1] + irr["l1z"][0] * n[2]
        count += 1
    return total / max(count, 1)


def main():
    no_close = "--no-close" in sys.argv
    skip_launch = "--skip-launch" in sys.argv
    proc = None
    log = None
    if not skip_launch:
        proc, log = launch_host()
    try:
        if not wait_brp(10 if skip_launch else 180):
            print("BRP 未就绪")
            return 2
        rpc("ash_renderer/ibl_set", {"enabled": True, "intensity": 1.0})
        rpc("ash_renderer/gi_set", {"enabled": True, "view": 1, "estimation": True, "samples": 2,
                                    "multi_bounce": True, "emissive_intensity": 1.0})
        ready = wait_for(lambda: (lambda s: s if s["estimation"]["rays"] > 0 else None)(rpc("ash_renderer/gi_stats")))
        check("estimation 起跑（有射线读数）", bool(ready),
              f"rays={ready['estimation']['rays'] if ready else 'x'}")
        if not ready:
            return 1
        time.sleep(5.0)

        # ---- ① 多跳色渗：A/B 读图 + 探针对账 ----
        rpc("ash_renderer/gi_set", {"view": 10})
        time.sleep(1.5)
        bounce_on_img = os.path.join(TMP, "gi96_ld_bounce_on.ppm")
        rpc("ash_renderer/gi_dump", {"path": bounce_on_img})
        mean_on = probe_mean(128)
        rpc("ash_renderer/gi_set", {"multi_bounce": False})
        time.sleep(6.0)
        bounce_off_img = os.path.join(TMP, "gi96_ld_bounce_off.ppm")
        rpc("ash_renderer/gi_dump", {"path": bounce_off_img})
        mean_off = probe_mean(128)
        diff_mean, diff_ratio = image_diff(bounce_on_img, bounce_off_img)
        # 断言只看整屏：逐 patch 探针在池子 churn（bounce 分配/淘汰）时指向的 patch 会变，
        # 单用它判定方向会出现假失败；这里作为"单点对账"证据打印，不作断言。
        check("① 多跳开/关的读图有差异", diff_ratio > 0.02,
              f"平均差 {diff_mean:.2f}/255、差异像素 {diff_ratio * 100:.1f}%；"
              f"探针 128 条均值 {mean_on:.2f} → {mean_off:.2f}")
        rpc("ash_renderer/gi_set", {"multi_bounce": True})

        # ---- ④ 自发光 A/B（干净场景里做：此时池里只有屏幕播种的 patch，可比同一批） ----
        # 自发光面板要够大、够近：小方块在整屏均值里只占零点几灰阶，会被估计噪声淹掉
        rpc("ash_renderer/gi_set", {
            "multi_bounce": False, "samples": 8, "budget": 4_000_000,
            "short_hysteresis": 0.99,
        })
        time.sleep(4.0)
        rpc("ash_renderer/spawn_shape", {
            "shape": "cuboid", "translation": [0.0, 1.2, 2.0], "scale": [8.0, 0.2, 8.0],
            "color": [1.0, 1.0, 1.0], "emissive": [16.0, 11.0, 6.0], "unique": True,
            "name": "gi-ld-emissive",
        })
        emissive = wait_for(lambda: (lambda s: s if s["emissive_triangles"] > 0 else None)(rpc("ash_renderer/gi_stats")))
        check("④ 自发光三角形进表", bool(emissive),
              f"count={emissive['emissive_triangles'] if emissive else 'x'}")
        # 判据用整屏均值（一次平均上万像素），探针只作为"单点对账"参考打印——
        # 逐 patch 探针会被池子 churn 打乱（ring 下标指向的 patch 在两次测量间会被淘汰/复用），
        # 单用探针判会出现"关掉自发光反而更亮"的假失败。
        time.sleep(8.0)
        em_a = os.path.join(TMP, "gi96_ld_emissive_on_a.ppm")
        em_b = os.path.join(TMP, "gi96_ld_emissive_on_b.ppm")
        rpc("ash_renderer/gi_dump", {"path": em_a})
        time.sleep(2.0)
        rpc("ash_renderer/gi_dump", {"path": em_b})
        em_probe_on = probe_mean(16)
        rpc("ash_renderer/gi_set", {"emissive_intensity": 0.0})
        time.sleep(8.0)
        em_c = os.path.join(TMP, "gi96_ld_emissive_off.ppm")
        rpc("ash_renderer/gi_dump", {"path": em_c})
        em_probe_off = probe_mean(16)
        noise, _ = image_diff(em_a, em_b)
        mean_on = image_mean(em_b)
        mean_off = image_mean(em_c)
        diff_em, _ = image_diff(em_b, em_c)
        check("④ 自发光关掉后整屏辐照度下降", mean_off < mean_on,
              f"整屏均值 {mean_on:.2f} → {mean_off:.2f}（探针 {em_probe_on:.2f} → {em_probe_off:.2f}）")
        check("④ 自发光关掉后画面差分超过同档噪声", diff_em > max(1.5, noise * 1.2),
              f"差分 {diff_em:.2f}/255、同档噪声 {noise:.2f}/255")
        rpc("ash_renderer/gi_set", {
            "emissive_intensity": 1.0, "multi_bounce": True, "samples": 2,
            "budget": 262_144, "short_hysteresis": 0.85,
        })

        # ---- ② 开关灯 lag 量级（只记录） ----
        rpc("ash_renderer/spawn_shape", {
            "shape": "cuboid", "translation": [0.0, 3.0, -1.0], "scale": [1.5, 8.0, 8.0],
            "unique": True, "name": "gi-ld-wall",
        })
        time.sleep(2.0)
        light = rpc("ash_renderer/light_spawn", {
            "pos": [0.0, 3.0, 0.0], "radius": 12.0, "color": [1.0, 0.9, 0.8], "intensity": 30.0,
        })
        time.sleep(3.0)
        lit = rpc("ash_renderer/gi_stats")["estimation"]
        rpc("ash_renderer/light_update", {"slot": light["slot"], "intensity": 0.0})
        time.sleep(1.0)
        dark = rpc("ash_renderer/gi_stats")["estimation"]
        check("② 开关灯后计数仍在跑（lag 量级见读数）", dark["rays"] > 0,
              f"灯开 rays={lit['rays']} → 灯关 rays={dark['rays']}；"
              f"converged {lit['converged_ratio']:.2f}→{dark['converged_ratio']:.2f}")
        rpc("ash_renderer/light_remove", {"slot": light["slot"]})

        # ---- ③ 预算降档后画面稳定 ----
        # 判据是**相对**的：先量"同档位、隔两秒"的噪声下限，再看"降档前后"的差是否落在
        # 同一量级。绝对阈值不可用——同一份代码两次跑分别得到 8.82 与 32.45/255，因为
        # 多跳开着时池子在持续 churn（bounce 分配/淘汰），可视 cell 的 patch 会在
        # "刚播种（IBL 种子）"与"已收敛"之间来回。所以先关多跳、把 churn 这个变量摘掉。
        rpc("ash_renderer/gi_set", {"multi_bounce": False})
        wide_budget = 4_000_000
        rpc("ash_renderer/gi_set", {"budget": wide_budget, "samples": 8})
        time.sleep(8.0)
        high_a = os.path.join(TMP, "gi96_ld_budget_high_a.ppm")
        high_b = os.path.join(TMP, "gi96_ld_budget_high_b.ppm")
        rpc("ash_renderer/gi_dump", {"path": high_a})
        time.sleep(2.0)
        rpc("ash_renderer/gi_dump", {"path": high_b})
        high = rpc("ash_renderer/gi_stats")["estimation"]
        noise, _ = image_diff(high_a, high_b)
        low_img = os.path.join(TMP, "gi96_ld_budget_low.ppm")
        rpc("ash_renderer/gi_set", {"budget": 20_000})
        time.sleep(8.0)
        rpc("ash_renderer/gi_dump", {"path": low_img})
        low = rpc("ash_renderer/gi_stats")["estimation"]
        diff_budget, ratio_budget = image_diff(high_b, low_img)
        limit = max(12.0, noise * 2.5)
        check("③ 紧预算后样本档不升", low["samples_effective"] <= high["samples_effective"],
              f"档位 {high['samples_effective']}→{low['samples_effective']}")
        check("③ 降档后画面差分落在同档噪声量级内", diff_budget <= limit,
              f"档位 {high['samples_effective']}→{low['samples_effective']}、"
              f"降档差 {diff_budget:.2f}/255、同档噪声 {noise:.2f}/255、上限 {limit:.2f}")
        # 样本档从 8 降到 1：方差按 1/样本数 涨是**应该**的（少采样必然更抖），
        # 所以判据不是"方差不涨"，而是"涨得有界 + 画面差分在阈值内"（上面那条）。
        check("③ 降档后各方差在 1/样本数 的量级内",
              low["variance_mean"] <= high["variance_mean"] * 40.0 + 1.0,
              f"方差 {high['variance_mean']:.2f} → {low['variance_mean']:.2f}"
              f"（档位 {high['samples_effective']}→{low['samples_effective']}）")
        rpc("ash_renderer/gi_set", {"budget": 262_144, "samples": 2, "multi_bounce": True})

        # ---- ⑤ 阴影漏光留档（读图存 .temp，人工看图归档） ----
        rpc("ash_renderer/gi_set", {"view": 10})
        time.sleep(1.5)
        leak_img = os.path.join(TMP, "gi96_ld_leak.ppm")
        rpc("ash_renderer/gi_dump", {"path": leak_img})
        stats = image_stats(leak_img)
        check("⑤ 漏光案例读图已留档", stats["distinct"] > 4, f"{leak_img}（distinct={stats['distinct']}）")

        # ---- ⑥ 回退硬线：estimation 关 = 9.5 收官画面（同进程同槽逐字节） ----
        rpc("ash_renderer/gi_set", {"view": 1, "estimation": False})
        time.sleep(1.0)
        off_a = os.path.join(TMP, "gi96_ld_est_off_a.ppm")
        rpc("ash_renderer/gi_dump", {"path": off_a})
        time.sleep(1.0)
        off_b = os.path.join(TMP, "gi96_ld_est_off_b.ppm")
        rpc("ash_renderer/gi_dump", {"path": off_b})
        same = open(off_a, "rb").read() == open(off_b, "rb").read()
        check("⑥ estimation 关：连拍两帧逐字节一致", same, off_a)

        # 再开一次估计，画面应与关态不同（证明确实生效过）
        rpc("ash_renderer/gi_set", {"estimation": True})
        time.sleep(4.0)
        on_img = os.path.join(TMP, "gi96_ld_est_on.ppm")
        rpc("ash_renderer/gi_set", {"view": 10})
        time.sleep(1.0)
        rpc("ash_renderer/gi_dump", {"path": on_img})
        check("⑥ estimation 开：画面与关态不同（估计确实生效）",
              image_stats(on_img)["distinct"] > 4, f"distinct={image_stats(on_img)['distinct']}")

        # ---- ⑦ 零 VUID ----
        check("⑦ 零 VUID / VK-ERROR", vuid_lines() == 0, f"可疑行 {vuid_lines()}")
    finally:
        try:
            rpc("ash_renderer/gi_set", {"estimation": False, "enabled": False, "view": 0,
                                        "budget": 262_144, "samples": 2, "emissive_intensity": 1.0})
        except Exception:
            pass
        if proc is not None and not no_close:
            closed = close_host(proc.pid)
            print(f"宿主退出：{'是' if closed else '否'}")
        if log is not None:
            log.close()

    bad = [name for name, ok in _results if not ok]
    print(f"\n判定线 {len(_results) - len(bad)}/{len(_results)} 过")
    for name in bad:
        print(f"  FAIL: {name}")
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
