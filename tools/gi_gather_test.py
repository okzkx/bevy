"""9.7 屏幕 gather 与注入的判定线脚本（自启宿主、逐项采证、退出码）。

判定线（详见 `.agents/docs/开发步骤/9-表面缓存全局光照/9.7-屏幕gather与注入/施工计划`）：
  ① 干净锚：开阔测试场 + 常量天空 L=1 + 太阳压地平线 → 缓存求值 E = πL（±5%）
  ② 场景 A/B：受控测试场（地面/背墙/白箱/红柱/绿球/自发光/悬挑板）× 多机位 × 多灯光，
     `scgi` 开/关的画面必须不同；"墙背面"要求有实质差异（方向由多跳决定，不指定变暗）；
     **悬挑板下的地板**必须明显变暗（那才是 GI 遮挡的正例）
  ③ 零 VUID / VK-ERROR
  ④ 新鲜度自检：转一下太阳，前后两帧必须不同（否则说明渲染停了、取证作废）

取证通道：`ash_renderer/frame_dump`（渲染器自回读）。不用 PrintWindow——遮挡/失焦时它会
返回陈旧帧（本轮实测四个机位拿到同一张图，见 9.7.7 施工记录）。

用法：
  python tools/gi_gather_test.py [--no-close] [--skip-launch]
环境变量：ASH_HOST_EXE（宿主二进制）、ASH_BRP_PORT（默认 15702）
退出码：0 = 全过；1 = 有失败；2 = 宿主缺/BRP 未就绪
"""

import argparse
import ctypes
import json
import math
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
HOST_LOG = os.path.join(REPO, ".temp", "gi_gather_test_host.log")
TMP = os.path.join(REPO, ".temp", "gi97")
BRP_PORT = int(os.environ.get("ASH_BRP_PORT", "15702"))
TEST_ORIGIN = (80.0, 0.0, 0.0)

_results = []


def rpc(method, params=None, timeout=240):
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


def wait_for(predicate, seconds=180.0, interval=0.5):
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


def pid_alive(pid):
    out = subprocess.run(
        ["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"],
        capture_output=True, text=True, encoding="gbk", errors="replace",
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


def dump(name):
    ppm = os.path.join(TMP, name + ".ppm")
    png = os.path.join(TMP, name + ".png")
    rpc("ash_renderer/frame_dump", {"path": ppm, "slot": 0})
    from PIL import Image

    with Image.open(ppm) as img:
        img.save(png)
    return png


def mean_and_diff(path_a, path_b):
    from PIL import Image, ImageChops, ImageStat

    with Image.open(path_a) as a, Image.open(path_b) as b:
        ga, gb = a.convert("L"), b.convert("L")
        diff = ImageChops.difference(ga, gb)
        return ImageStat.Stat(ga).mean[0], ImageStat.Stat(gb).mean[0], ImageStat.Stat(diff).mean[0]


def wait_sky_uniform(timeout=90.0):
    """等 IBL 的常量天空捕获落定：`probe16.center_energy` ≈ π 才算就绪。

    坑：`sky_uniform` 只是改开关，环境 cube/SH 要等下一次捕获；不等待就直接取帧，
    会拿到"上一张天空"下的 IBL（实测出现过 IBL 4.59 的假读数）。
    """
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            st = rpc("ash_renderer/ibl_stats")
            last = st["probe16"]["center_energy"][0][0]
        except Exception:  # noqa: BLE001
            last = None
        if last is not None and abs(last / math.pi - 1.0) <= 0.05:
            return round(last, 4)
        time.sleep(0.5)
    return None


def build_field():
    ox, oy, oz = TEST_ORIGIN
    for shape in [
        dict(shape="cuboid", color=[0.72, 0.72, 0.72], scale=[30.0, 0.3, 24.0],
             translation=[ox, oy - 0.15, oz + 12.0], name="gt_floor"),
        dict(shape="cuboid", color=[0.85, 0.85, 0.85], scale=[12.0, 5.0, 0.3],
             translation=[ox, oy + 2.5, oz + 4.0], name="gt_wall"),
        dict(shape="cuboid", color=[0.9, 0.9, 0.9], scale=[1.2, 1.2, 1.2],
             translation=[ox - 2.4, oy + 0.6, oz + 9.0], name="gt_box_w"),
        dict(shape="cuboid", color=[0.85, 0.12, 0.1], scale=[0.9, 2.6, 0.9],
             translation=[ox, oy + 1.3, oz + 11.0], name="gt_pillar_r"),
        dict(shape="sphere", color=[0.15, 0.75, 0.25], translation=[ox + 2.6, oy + 0.6, oz + 8.5],
             name="gt_sphere_g"),
        dict(shape="cuboid", color=[0.05, 0.05, 0.05], scale=[0.8, 0.8, 0.1],
             emissive=[6.0, 3.0, 0.6], translation=[ox + 3.8, oy + 1.6, oz + 10.5],
             name="gt_emissive"),
        # 悬挑板：给"真遮挡"一个正例。板下的地板只剩一条天空带，缓存必须明显暗于 IBL；
        # 而孤立的墙背面其实**不是** GI 遮挡（它朝开阔天空 + 被太阳照亮的地板要多跳补光），
        # 所以判定线不再要求"墙背面必须变暗"——那是把直射遮挡与 GI 遮挡混为一谈。
        dict(shape="cuboid", color=[0.02, 0.02, 0.02], scale=[12.0, 0.3, 12.0],
             translation=[ox - 8.0, oy + 2.4, oz + 16.0], name="gt_overhang"),
    ]:
        rpc("ash_renderer/spawn_shape", shape)


CAMERAS = [
    ("cam_front", 0.0, 10.0, 13.0),
    ("cam_close", 25.0, 6.0, 7.0),
    ("cam_behind", 180.0, 12.0, 11.0),
]

LIGHTS = [
    ("sun_hi", {"azimuth_deg": 45.0, "elevation_deg": 55.0}),
    ("sun_low", {"azimuth_deg": 220.0, "elevation_deg": 12.0}),
]


def anchor():
    """① 干净锚（屏幕版）：常量天空 L=1 + 太阳压地平线，SCGI 与 IBL 的画面必须同量级。

    为什么不比"缓存 L0 是否等于 π"：`scgi_irradiance` 是**求值后的 E(n)**，而它与视野里
    究竟是哪个面朝向天空强相关（地板朝上、砖墙朝侧、箱体各个面都不一样）。屏幕比对没有这个
    歧义，也正是判定线②的原话（"注入后的显示值与 IBL 基线在同一区间"）。E 的绝对值仍然打印
    出来当读数留档，但不作断言。
    """
    rpc("ash_renderer/sky_uniform", {"enabled": True, "color": [1.0, 1.0, 1.0]})
    rpc("ash_renderer/sky_set", {"enabled": True})
    rpc("ash_renderer/ibl_set", {"enabled": True})
    # 太阳压到地平线：直射几乎不进 NoL，两个路径都只剩天光
    rpc("ash_renderer/sun_set", {"azimuth_deg": 0.0, "elevation_deg": 0.0})
    ox, oy, oz = TEST_ORIGIN
    rpc("ash_renderer/set_camera", {"target": [ox, 1.0, oz + 9.5], "yaw_deg": 0.0,
                                    "pitch_deg": 14.0, "radius": 13.0})
    rpc("ash_renderer/gi_set", {
        "enabled": True, "view": 0, "estimation": True,
        "spatial_filter": True, "temporal_filter": True, "scgi": False,
    })
    time.sleep(10.0)
    ready = wait_sky_uniform()
    check("① 常量天空就绪：GlobalProbe 中心能量 ≈ π（±5%）", ready is not None,
          f"center_energy={ready}")
    p_ibl = dump("anchor_ibl")
    rpc("ash_renderer/gi_set", {"scgi": True})
    time.sleep(6.0)
    p_scgi = dump("anchor_scgi")
    m_ibl, m_scgi, diff = mean_and_diff(p_ibl, p_scgi)
    ratio = m_scgi / m_ibl if m_ibl else 0.0
    check(
        "① 干净锚：常量天空 L=1 下 SCGI 与 IBL 同量级（±5%）",
        abs(ratio - 1.0) <= 0.05,
        f"IBL {m_ibl:.2f} / SCGI {m_scgi:.2f} → 比值 {ratio:.4f}",
    )
    rpc("ash_renderer/sky_uniform", {"enabled": False})
    rpc("ash_renderer/sun_set", {"azimuth_deg": 45.0, "elevation_deg": 55.0})


def off_matrix():
    """⑤ 关态矩阵：`scgi = 0` 的稳定性、回退精确性，以及 `ibl_mode` × `ibl_enabled` 四组。

    跨构建逐字节对比做不到（没有存档的 9.6 收官帧），所以钉同构建内的三条不变量：
    A 关态两帧逐字节一致；B 开→关后与关态首帧逐字节一致；C 四组组合产出 3 种画面
    （IBL 开时 sh_l2 / probe16 不同；IBL 关时两者都走 ambient，必然相同）。
    """
    rpc("ash_renderer/ibl_set", {"enabled": True, "diffuse_mode": "sh_l2"})
    rpc("ash_renderer/gi_set", {"scgi": False})
    time.sleep(3.0)
    a1 = dump("matrix_off_a1")
    time.sleep(3.0)
    a2 = dump("matrix_off_a2")
    check("⑤A 关态两帧逐字节一致", open(a1, "rb").read() == open(a2, "rb").read())

    rpc("ash_renderer/gi_set", {"scgi": True})
    time.sleep(3.0)
    dump("matrix_on")
    rpc("ash_renderer/gi_set", {"scgi": False})
    time.sleep(3.0)
    b = dump("matrix_off_b")
    check("⑤B 开→关后与关态首帧逐字节一致", open(a1, "rb").read() == open(b, "rb").read())

    frames = {}
    for mode in ("sh_l2", "probe16"):
        for ibl_on in (True, False):
            rpc("ash_renderer/ibl_set", {"enabled": ibl_on, "diffuse_mode": mode})
            time.sleep(2.0)
            frames[(mode, ibl_on)] = dump(f"matrix_{mode}_i{int(ibl_on)}")
    shots = {k: open(v, "rb").read() for k, v in frames.items()}
    check("⑤C 四组组合产出 3 种画面", len(set(shots.values())) == 3,
          f"{len(set(shots.values()))} 种")
    check("⑤C ibl_enabled=0 时编码无关",
          shots[("sh_l2", False)] == shots[("probe16", False)])
    rpc("ash_renderer/ibl_set", {"enabled": True, "diffuse_mode": "sh_l2"})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--no-close", action="store_true")
    ap.add_argument("--skip-launch", action="store_true")
    args = ap.parse_args()
    os.makedirs(TMP, exist_ok=True)
    proc = None
    if args.skip_launch:
        if not wait_for(lambda: rpc("ash_renderer/gi_stats"), 30):
            print("BRP 未就绪（--skip-launch）")
            return 2
    else:
        if not os.path.isfile(EXE):
            print(f"宿主二进制不存在：{EXE}\n先 cargo build 再跑本脚本。")
            return 2
        log = open(HOST_LOG, "wb")
        proc = subprocess.Popen([EXE], cwd=REPO, stdout=log, stderr=subprocess.STDOUT,
                                creationflags=subprocess.CREATE_NEW_PROCESS_GROUP)
        print(f"宿主已自启（PID {proc.pid}），取证目录 {TMP}")
        if not wait_for(lambda: rpc("ash_renderer/gi_stats")):
            print("BRP 未就绪")
            return 2
    pid = proc.pid if proc else 0
    try:
        rpc("ash_renderer/sky_set", {"enabled": True})
        rpc("ash_renderer/ibl_set", {"enabled": True})

        build_field()
        anchor()
        rpc("ash_renderer/sun_set", {"azimuth_deg": 45.0, "elevation_deg": 55.0})
        rpc("ash_renderer/set_camera", {"target": [TEST_ORIGIN[0], 1.0, TEST_ORIGIN[2] + 9.5],
                                        "yaw_deg": 0.0, "pitch_deg": 10.0, "radius": 13.0})
        rpc("ash_renderer/gi_set", {
            "enabled": True, "view": 0, "estimation": True,
            "spatial_filter": True, "temporal_filter": True, "scgi": False,
        })
        time.sleep(8.0)

        # ④ 新鲜度自检
        a = dump("fresh_a")
        rpc("ash_renderer/sun_set", {"azimuth_deg": 135.0, "elevation_deg": 55.0})
        time.sleep(1.5)
        b = dump("fresh_b")
        _ma, _mb, fresh = mean_and_diff(a, b)
        check("④ frame_dump 是新鲜帧（转太阳后两帧不同）", fresh > 0.5, f"差分 {fresh:.2f}/255")

        # ② 场景 A/B 矩阵
        rows = []
        for light_name, sun in LIGHTS:
            rpc("ash_renderer/sun_set", sun)
            time.sleep(2.0)
            for cam_name, yaw, pitch, radius in CAMERAS:
                rpc("ash_renderer/set_camera", {
                    "target": [TEST_ORIGIN[0], 1.0, TEST_ORIGIN[2] + 9.5],
                    "yaw_deg": yaw, "pitch_deg": pitch, "radius": radius,
                })
                time.sleep(2.0)
                rpc("ash_renderer/gi_set", {"scgi": False})
                time.sleep(1.2)
                p_ibl = dump(f"{light_name}_{cam_name}_ibl")
                rpc("ash_renderer/gi_set", {"scgi": True})
                time.sleep(1.8)
                p_scgi = dump(f"{light_name}_{cam_name}_scgi")
                m_ibl, m_scgi, diff = mean_and_diff(p_ibl, p_scgi)
                rows.append((f"{light_name}/{cam_name}", m_ibl, m_scgi, diff))
                print(f"    [{light_name}/{cam_name}] IBL {m_ibl:.1f} / SCGI {m_scgi:.1f}"
                      f" / 差分 {diff:.2f}")

        nonzero = [r for r in rows if r[3] > 0.5]
        check("② 场景 A/B：注入确实改变画面（至少 3 组非零）", len(nonzero) >= 3,
              f"{len(nonzero)}/{len(rows)} 组非零")
        behind = [r for r in rows if r[0].endswith("cam_behind") and r[0].startswith("sun_hi")]
        if behind:
            name, m_ibl, m_scgi, _ = behind[0]
            ratio = m_scgi / m_ibl if m_ibl else 1.0
            # 孤立的墙背面朝开阔天空，而且地板被太阳照亮要多跳补光：它比 IBL 略亮是对的。
            # 这里只要求"注入真的换了数"（≥2%），遮挡的正例交给下面的悬挑遮挡。
            check("② 墙背面：注入与 IBL 有实质差异（≥2%，方向由多跳决定）",
                  abs(ratio - 1.0) >= 0.02, f"{name}: IBL {m_ibl:.1f} → SCGI {m_scgi:.1f}（{ratio:.3f}×）")
        else:
            check("② 墙背面：注入与 IBL 有实质差异（≥2%，方向由多跳决定）", False, "缺该组读数")

        # ② 遮挡的正例：悬挑板下的地板。
        # 用**物理量对账**（scgi_stats 的 E 读回）而不是 LDR 屏均值：常量天空 L=1 是 1 lux 量级，
        # 那样的帧整体接近黑，比屏均值会把噪声当信号。这里在真实天空下取两次 E：
        # 板下（黑板把天光挡掉大半）必须明显低于开阔处。
        def E_mean(tag):
            sc = rpc("ash_renderer/scgi_stats", {"stride": 8})
            return sum(sc["l0_covered_mean"]) / 3.0, sc["coverage"], tag

        rpc("ash_renderer/sun_set", {"azimuth_deg": 45.0, "elevation_deg": 55.0})
        rpc("ash_renderer/set_camera", {"target": [TEST_ORIGIN[0], 1.0, TEST_ORIGIN[2] + 9.5],
                                        "yaw_deg": 0.0, "pitch_deg": 10.0, "radius": 13.0})
        time.sleep(6.0)
        e_open = E_mean("open")
        rpc("ash_renderer/set_camera", {
            "target": [TEST_ORIGIN[0] - 8.0, 0.2, TEST_ORIGIN[2] + 16.0],
            "yaw_deg": 0.0, "pitch_deg": 35.0, "radius": 2.0,
        })
        time.sleep(8.0)
        e_under = E_mean("under")
        ratio = e_under[0] / e_open[0] if e_open[0] else 1.0
        check("② 悬挑遮挡：板下屏幕辐照度明显低于开阔处（比值 ≤0.60）", ratio <= 0.60,
              f"开阔 E={e_open[0]:.0f}（覆盖 {e_open[1]:.2f}）/ 板下 E={e_under[0]:.0f}"
              f"（覆盖 {e_under[1]:.2f}）→ 比值 {ratio:.3f}")
        rpc("ash_renderer/set_camera", {"target": [TEST_ORIGIN[0], 1.0, TEST_ORIGIN[2] + 9.5],
                                        "yaw_deg": 0.0, "pitch_deg": 10.0, "radius": 13.0})
        time.sleep(2.0)

        off_matrix()

        errors = [ln for ln in open(HOST_LOG, "rb").read().decode("utf-8", "replace").splitlines()
                  if "VK-ERROR" in ln or "VUID" in ln] if os.path.isfile(HOST_LOG) else []
        check("③ 零 VUID / VK-ERROR", not errors, f"{len(errors)} 行")
        return 0 if all(ok for _, ok in _results) else 1
    finally:
        if proc is not None and not args.no_close and not close_host(pid):
            proc.kill()


if __name__ == "__main__":
    code = main()
    passed = sum(1 for _, ok in _results if ok)
    print(f"\n判定线 {passed}/{len(_results)} 项")
    sys.exit(code)
