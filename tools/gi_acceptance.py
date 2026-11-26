"""9.8 acceptance harness: fixed conditions, readouts, frame capture and diffs.

The harness drives a running `ash_renderer` over BRP only. It never injects
desktop input and never depends on window focus: every picture is the renderer's
own `frame_dump` readback (see the 9.7.7 lesson).

Examples:
  python tools/gi_acceptance.py baseline --scene sponza/glTF/Sponza.gltf
  python tools/gi_acceptance.py tour
  python tools/gi_acceptance.py dynamic
  python tools/gi_acceptance.py boundary
  python tools/gi_acceptance.py probe

Artifacts go to `.temp/gi98/<command>/<run-id>/`; JSON readouts, PPM raw frames
and sRGB PNGs stay out of the docs tree.
"""

import argparse
import hashlib
import json
import os
import statistics
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

# tools/* is a Cargo workspace glob: never let Python drop module caches here.
sys.dont_write_bytecode = True

import numpy as np
from PIL import Image

from brp import rpc

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUT = ROOT / ".temp" / "gi98"
DEFAULT_EXE = ROOT / "target" / "release" / "ash_renderer.exe"
DEFAULT_SCENE = "sponza/glTF/Sponza.gltf"
BASELINE_GI = {
    "size": 128,
    "res": 32,
    "cascades": 4,
    "capacity": 65536,
    "enabled": True,
    "estimation": True,
    "samples": 2,
    "warmup": 4,
    "multi_bounce": True,
    "budget": 262144,
    "scgi": True,
    "spatial_filter": True,
    "spatial_radius": 3.0,
    "temporal_filter": True,
    "temporal_smoothing": 0.5,
    "lookup_samples": 3,
    "denoise_passes": 2,
    "upsample_bilinear": False,
    "emissive_intensity": 1.0,
    "light_samples": 128,
}
SPEC_KEYS = ("size", "res", "cascades", "capacity")
READOUTS = (
    "gi_stats",
    "scgi_stats",
    "ibl_stats",
    "perf_stats",
    "residency_stats",
    "draw_stats",
    "light_stats",
    "exposure_get",
    "sun_get",
)
TOUR_POSES = [
    # 12 orbit poses around the Sponza hall
    *[
        {"name": f"orbit_{yaw:03d}", "camera": {"target": [0, 3, 0], "yaw_deg": yaw,
                                                  "pitch_deg": 10, "radius": 8}}
        for yaw in range(0, 360, 30)
    ],
    # 4 interior poses
    {"name": "interior_low", "camera": {"target": [0, 1.5, 0], "yaw_deg": 90,
                                         "pitch_deg": 0, "radius": 3}},
    {"name": "interior_west", "camera": {"target": [-5, 2, 0], "yaw_deg": 0,
                                          "pitch_deg": 20, "radius": 5}},
    {"name": "interior_east", "camera": {"target": [5, 2, 0], "yaw_deg": 180,
                                          "pitch_deg": 20, "radius": 5}},
    {"name": "interior_top", "camera": {"target": [0, 1.0, 0], "yaw_deg": 90,
                                         "pitch_deg": 65, "radius": 5}},
    # 4 corridor / high-low poses
    {"name": "corridor_north", "camera": {"target": [0, 2, -10], "yaw_deg": 0,
                                           "pitch_deg": 5, "radius": 6}},
    {"name": "corridor_east", "camera": {"target": [10, 2, 0], "yaw_deg": 270,
                                          "pitch_deg": 10, "radius": 6}},
    {"name": "corridor_west", "camera": {"target": [-10, 2, 0], "yaw_deg": 90,
                                          "pitch_deg": 10, "radius": 6}},
    {"name": "corridor_south", "camera": {"target": [0, 1.5, 8], "yaw_deg": 180,
                                           "pitch_deg": 5, "radius": 6}},
]
FIELD_CAMERA = {"target": [80, 1.5, -2], "yaw_deg": 0, "pitch_deg": 10, "radius": 9}
BOUNDARY_CAMERA = {"target": [88, 2, 0], "yaw_deg": -90, "pitch_deg": 2, "radius": 8}
BASE_CAMERAS = [
    ("default", {"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 5, "radius": 5}),
    ("gallery", {"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 10, "radius": 9}),
    ("courtyard", {"target": [0, 1.5, 0], "yaw_deg": 90, "pitch_deg": 8, "radius": 3}),
    ("overhead", {"target": [0, 1.0, 0], "yaw_deg": 90, "pitch_deg": 65, "radius": 5}),
]


def now():
    return datetime.now().isoformat(timespec="seconds")


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def srgb8(linear_rgb8):
    """Encode a linear UNORM readback for display (same as gi_scene_regression)."""
    linear = np.asarray(linear_rgb8, dtype=np.float32) / 255.0
    encoded = np.where(
        linear <= 0.0031308,
        linear * 12.92,
        1.055 * np.power(linear, 1.0 / 2.4) - 0.055,
    )
    return np.rint(np.clip(encoded, 0.0, 1.0) * 255.0).astype(np.uint8)


def luma(rgb):
    return np.asarray(rgb, dtype=np.float32) @ np.array([0.2126, 0.7152, 0.0722])


class Brp:
    def __init__(self, port):
        self.port = port

    def call(self, method, params=None, timeout=240):
        payload = rpc("127.0.0.1", self.port, "ash_renderer/" + method,
                      params or {}, timeout=timeout)
        if "error" in payload:
            raise RuntimeError(f"{method}: {payload['error']}")
        return payload["result"]

    def try_call(self, method, params=None, timeout=60):
        try:
            return self.call(method, params, timeout=timeout)
        except Exception:
            return None

    def world_call(self, method, params=None, timeout=60):
        """Call a Bevy built-in remote method (e.g. world.despawn_entity)."""
        payload = rpc("127.0.0.1", self.port, method, params or {}, timeout=timeout)
        if "error" in payload:
            raise RuntimeError(f"{method}: {payload['error']}")
        return payload["result"]


class Host:
    """Attach to a running renderer or launch a private one on `port`."""

    def __init__(self, args, out):
        self.args = args
        self.out = out
        self.brp = Brp(args.port)
        self.proc = None
        self.attached = False
        self.launch = None

    def ensure(self):
        if self.brp.try_call("gi_stats") is not None:
            self.attached = True
            print(f"[host] attached to existing instance on :{self.args.port}")
            return
        exe = Path(self.args.exe)
        if not exe.exists():
            raise SystemExit(f"renderer not found: {exe} (build release first)")
        log = self.out / "host.log"
        env = dict(os.environ)
        env["ASH_BRP_PORT"] = str(self.args.port)
        self.launch = [str(exe), "--scene", self.args.scene]
        print(f"[host] launch {' '.join(self.launch)} (log {log})")
        with open(log, "wb") as fh:
            self.proc = subprocess.Popen(
                self.launch, cwd=str(ROOT), env=env,
                stdout=fh, stderr=subprocess.STDOUT,
            )
        deadline = time.time() + 120
        while time.time() < deadline:
            if self.proc.poll() is not None:
                tail = log.read_text(encoding="utf-8", errors="replace")[-3000:]
                raise SystemExit(f"renderer exited early ({self.proc.returncode}):\n{tail}")
            if self.brp.try_call("gi_stats") is not None:
                print(f"[host] ready pid={self.proc.pid} :{self.args.port}")
                return
            time.sleep(0.5)
        raise SystemExit("renderer did not answer BRP within 120s")

    def close(self):
        if self.proc is None or getattr(self.args, "keep", False) or self.attached:
            return
        self.proc.terminate()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=5)
        print(f"[host] stopped pid={self.proc.pid}")


class Session:
    def __init__(self, args, command):
        stamp = time.strftime("%Y%m%d-%H%M%S")
        self.out = Path(args.out) / command / stamp
        self.out.mkdir(parents=True, exist_ok=True)
        self.args = args
        self.host = Host(args, self.out)
        self.host.ensure()
        self.brp = self.host.brp

    def close(self):
        self.host.close()

    # ------------------------------------------------------------------ read
    def read(self, method, params=None):
        return self.brp.call(method, params)

    def sample(self):
        data = {"at": now(), "frame_wall": time.time()}
        for method in READOUTS:
            data[method] = self.brp.try_call(method)
        return data

    def snapshot(self, name, expected_camera=None):
        ppm = self.out / f"{name}.ppm"
        png = self.out / f"{name}.png"
        if ppm.exists():
            ppm.unlink()
        before = self.brp.call("set_camera", {"zoom": 1})["orbit"] if expected_camera else None
        self.brp.call("frame_dump", {"path": str(ppm)})
        raw = np.asarray(Image.open(ppm).convert("RGB"), dtype=np.uint8)
        display = srgb8(raw)
        Image.fromarray(display).save(png)
        luma_raw = luma(raw)
        luma_disp = luma(display)
        after = self.brp.call("set_camera", {"zoom": 1})["orbit"] if expected_camera else None
        return {
            "name": name,
            "ppm": rel(ppm),
            "png": rel(png),
            "linear_mean": float(luma_raw.mean()),
            "linear_p50": float(np.percentile(luma_raw, 50)),
            "display_mean": float(luma_disp.mean()),
            "display_p10": float(np.percentile(luma_disp, 10)),
            "display_p50": float(np.percentile(luma_disp, 50)),
            "display_p99": float(np.percentile(luma_disp, 99)),
            "display_black_fraction": float((luma_disp < 5).mean()),
            "orbit_before": before,
            "orbit_after": after,
            "pose_valid": expected_camera is None
            or (camera_matches(before, expected_camera) and camera_matches(after, expected_camera)),
        }

    # ------------------------------------------------------------------ write
    def set_camera(self, camera, settle=0.0):
        orbit = self.brp.call("set_camera", dict(camera))["orbit"]
        if not camera_matches(orbit, camera):
            raise RuntimeError(f"camera mismatch: wanted {camera}, got {orbit}")
        if settle:
            time.sleep(settle)
        return orbit

    def apply_baseline(self, size=128, res=32, cascades=4, capacity=65536,
                       samples=2, quality_extra=None):
        # Spec changes require the off state; a rebuild also resets the counters.
        self.brp.call("gi_set", {"enabled": False})
        self.brp.call("gi_set", {"size": size, "res": res, "cascades": cascades,
                                 "capacity": capacity})
        settings = dict(BASELINE_GI)
        for key in SPEC_KEYS:
            settings.pop(key, None)
        settings.update({"enabled": True, "samples": samples})
        if quality_extra:
            settings.update(quality_extra)
        # Never send the spec keys together with enabled=true in one request.
        self.brp.call("gi_set", settings)
        self.brp.call("ibl_set", {"enabled": True, "diffuse_mode": "sh_l2"})
        self.brp.call("exposure_set", {"mode": "manual"})
        self.brp.call("sun_set", {"azimuth_deg": 45.0, "elevation_deg": 55.0})
        print(f"[baseline] size={size} samples={samples} scgi={settings['scgi']}")

    def set_gi(self, params):
        if any(key in params for key in SPEC_KEYS):
            self.brp.call("gi_set", {"enabled": False})
            spec = {key: params[key] for key in SPEC_KEYS if key in params}
            self.brp.call("gi_set", spec)
            rest = {key: value for key, value in params.items() if key not in SPEC_KEYS}
            if rest:
                self.brp.call("gi_set", rest)
            self.brp.call("gi_set", {"enabled": True})
        else:
            self.brp.call("gi_set", params)

    def condition_sheet(self):
        exe = Path(self.args.exe) if not self.host.attached else None
        gpu = None
        try:
            gpu = subprocess.run(
                ["nvidia-smi", "--query-gpu=name,driver_version,memory.total",
                 "--format=csv,noheader"],
                capture_output=True, text=True, timeout=10,
            ).stdout.strip()
        except Exception as exc:  # noqa: BLE001 - condition sheet is best effort
            gpu = f"unavailable: {exc}"
        git_head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT,
                                  capture_output=True, text=True).stdout.strip()
        branch = subprocess.run(["git", "rev-parse", "--abbrev-ref", "HEAD"], cwd=ROOT,
                                capture_output=True, text=True).stdout.strip()
        stats = self.brp.call("gi_stats")
        scgi = self.brp.try_call("scgi_stats")
        return {
            "at": now(),
            "command": self.args.command if hasattr(self.args, "command") else None,
            "git_head": git_head,
            "branch": branch,
            "port": self.args.port,
            "attached": self.host.attached,
            "launch": self.host.launch,
            "exe": str(exe) if exe else None,
            "exe_mtime": datetime.fromtimestamp(exe.stat().st_mtime).isoformat() if exe else None,
            "exe_sha256": sha256(exe) if exe else None,
            "gpu": gpu,
            "scene": self.args.scene,
            "extent": (scgi or {}).get("extent"),
            "gi_settings": stats.get("settings"),
            "budget_ms": 16.7,
            "python": sys.version,
            "numpy": np.__version__,
        }

    def settle(self, seconds, reason=""):
        print(f"[settle] {seconds:.1f}s {reason}")
        time.sleep(seconds)

    def save(self, name, payload):
        path = self.out / name
        tmp = path.with_suffix(path.suffix + ".tmp")
        tmp.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
                       encoding="utf-8")
        tmp.replace(path)
        print(f"[save] {rel(path)}")
        return path


def rel(path):
    try:
        return str(Path(path).resolve().relative_to(ROOT))
    except ValueError:
        return str(path)


def camera_matches(actual, expected):
    return (
        all(abs(actual[key] - expected[key]) < 0.01
            for key in ("yaw_deg", "pitch_deg", "radius"))
        and all(abs(a - b) < 0.01 for a, b in zip(actual["target"], expected["target"]))
    )


def diff_frames(first_png, second_png):
    first = np.asarray(Image.open(first_png).convert("RGB"), dtype=np.int16)
    second = np.asarray(Image.open(second_png).convert("RGB"), dtype=np.int16)
    delta = np.abs(first - second)
    per_pixel = delta.max(axis=2)
    # Dithering/denoise noise lives in small deltas; permanent ghosts do not.
    changed = per_pixel > 16
    rows = np.flatnonzero(changed.any(axis=1))
    cols = np.flatnonzero(changed.any(axis=0))
    bbox = None
    if rows.size and cols.size:
        bbox = [int(cols[0]), int(rows[0]), int(cols[-1]), int(rows[-1])]
    return {
        "mean_abs": float(delta.mean()),
        "p99": float(np.percentile(per_pixel, 99)),
        "p999": float(np.percentile(per_pixel, 99.9)),
        "max": float(per_pixel.max()),
        "changed_fraction_8": float((per_pixel > 8).mean()),
        "changed_fraction_16": float(changed.mean()),
        "changed_fraction_32": float((per_pixel > 32).mean()),
        "changed_bbox": bbox,
    }


def save_ppm_diff(first_ppm, second_ppm, png):
    first = np.asarray(Image.open(first_ppm).convert("RGB"), dtype=np.int16)
    second = np.asarray(Image.open(second_ppm).convert("RGB"), dtype=np.int16)
    delta = np.abs(first - second)
    heat = np.clip(delta * 8, 0, 255).astype(np.uint8)
    Image.fromarray(heat).save(png)
    return rel(png)


def _field_shape(session, shape, translation, scale, color=None, emissive=None,
                 name=None):
    params = {"shape": shape, "translation": translation, "scale": scale}
    if color is not None:
        params["color"] = color
    if emissive is not None:
        params["emissive"] = emissive
    if name:
        params["name"] = name
    return session.brp.call("spawn_shape", params)


def build_field(session):
    shapes = {}
    shapes["ground"] = _field_shape(session, "cuboid", [80, -0.05, 0], [24, 0.1, 24],
                                    [0.55, 0.55, 0.55], name="gi98-ground")
    shapes["back_wall"] = _field_shape(session, "cuboid", [80, 3, -10], [24, 6, 0.4],
                                       [0.75, 0.75, 0.75], name="gi98-wall")
    shapes["white_box"] = _field_shape(session, "cuboid", [78, 0.6, 0], [1.2, 1.2, 1.2],
                                       [0.9, 0.9, 0.9], name="gi98-white")
    shapes["red_column"] = _field_shape(session, "cuboid", [80, 1.5, -2], [0.6, 3, 0.6],
                                        [0.8, 0.1, 0.1], name="gi98-red")
    shapes["green_sphere"] = _field_shape(session, "sphere", [82, 0.8, 0], [0.8, 0.8, 0.8],
                                          [0.1, 0.7, 0.2], name="gi98-green")
    shapes["emissive_panel"] = _field_shape(session, "cuboid", [80, 2, -9.6], [0.4, 1.6, 1.6],
                                            [1.0, 0.8, 0.6], [4.0, 2.5, 1.0],
                                            name="gi98-emissive")
    return shapes


def measure_stability(session, reference_png, seconds, interval=2.0):
    samples = []
    deadline = time.time() + seconds
    index = 0
    while time.time() < deadline:
        index += 1
        snap = session.snapshot(f"probe_{index:03d}")
        diff = diff_frames(reference_png, session.out / (snap["name"] + ".png"))
        stats = session.sample()
        gi = stats.get("gi_stats") or {}
        scgi = stats.get("scgi_stats") or {}
        samples.append({
            "at": now(),
            "mean_abs": diff["mean_abs"],
            "changed_fraction_16": diff["changed_fraction_16"],
            "coverage": scgi.get("coverage"),
            "alpha_mean": (scgi.get("alpha") or {}).get("mean"),
            "active": (gi.get("pool") or {}).get("active"),
            "watermark": (gi.get("pool") or {}).get("watermark"),
            "failed": (gi.get("counters") or {}).get("failed"),
            "freed": (gi.get("counters") or {}).get("freed"),
            "alloc": (gi.get("counters") or {}).get("alloc"),
            "frame_wall_p50": ((stats.get("perf_stats") or {}).get("frame_wall_ms") or {}).get("p50"),
        })
        time.sleep(interval)
    return samples


def cmd_baseline(session):
    session.apply_baseline()
    session.set_camera({"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 5, "radius": 5},
                       settle=15)
    sheet = session.condition_sheet()
    first = session.snapshot("baseline_a")
    second = session.snapshot("baseline_b")
    noise = diff_frames(session.out / "baseline_a.png", session.out / "baseline_b.png")
    # Freshness: a sun change must move the frame; otherwise the readback is stale.
    session.brp.call("sun_set", {"elevation_deg": 30.0})
    session.settle(2)
    moved = session.snapshot("freshness_sun30")
    fresh = diff_frames(session.out / "baseline_b.png", session.out / "freshness_sun30.png")
    session.brp.call("sun_set", {"elevation_deg": 55.0})
    session.settle(2)
    cameras = []
    for name, camera in BASE_CAMERAS:
        session.set_camera(camera, settle=5)
        cameras.append({"name": name, "camera": camera,
                        "snapshot": session.snapshot("cam_" + name, expected_camera=camera),
                        "sample": session.sample()})
    result = {
        "condition_sheet": sheet,
        "sample": session.sample(),
        "baseline_a": first,
        "baseline_b": second,
        "noise_floor": noise,
        "freshness": fresh,
        "freshness_ok": fresh["mean_abs"] > 0.5,
        "cameras": cameras,
    }
    session.save("baseline.json", result)
    return result


def cmd_tour(session):
    session.apply_baseline()
    start = TOUR_POSES[0]
    session.set_camera(start["camera"], settle=15)
    start_sample = session.sample()
    start_snapshot = session.snapshot("pose_000", expected_camera=_cam(start))
    rows = [{"pose": start["name"], "camera": start["camera"],
             "snapshot": "pose_000", "sample": start_sample}]
    for index in range(1, len(TOUR_POSES)):
        pose = TOUR_POSES[index]
        session.set_camera(pose["camera"], settle=2)
        sample = session.sample()
        row = {"pose": pose["name"], "camera": pose["camera"], "sample": sample}
        if index in (4, 8, 12, 16, 19):
            snap = session.snapshot(f"pose_{index:03d}", expected_camera=pose["camera"])
            row["snapshot"] = snap
        rows.append(row)
        print(f"[tour] {index + 1}/{len(TOUR_POSES)} {pose['name']}")
    # Back to the starting pose; the ledger must not have grown. Eviction needs
    # ~20 s at 60 Hz (240-frame age + rank-1 pass), so sample a settling window.
    session.set_camera(start["camera"], settle=5)
    settle_series = []
    for _ in range(15):
        time.sleep(2)
        stats = session.brp.call("gi_stats")
        pool = stats.get("pool") or {}
        counters = stats.get("counters") or {}
        settle_series.append({
            "at": now(),
            "active": pool.get("active"),
            "watermark": pool.get("watermark"),
            "ring_count": pool.get("ring_count"),
            "alloc": counters.get("alloc"),
            "freed": counters.get("freed"),
            "failed": counters.get("failed"),
        })
    settled_active = statistics.median(
        item["active"] for item in settle_series[-10:] if item["active"] is not None)
    end_sample = session.sample()
    end_snapshot = session.snapshot("pose_000_after", expected_camera=_cam(start))
    tour_diff = diff_frames(session.out / "pose_000.png", session.out / "pose_000_after.png")
    start_gi = start_sample["gi_stats"]
    end_gi = end_sample["gi_stats"]
    start_res = start_sample["residency_stats"]
    end_res = end_sample["residency_stats"]
    start_draw = start_sample["draw_stats"]
    end_draw = end_sample["draw_stats"]
    checks = {
        "failed_zero": (end_gi.get("counters") or {}).get("failed") == 0,
        "active_start": (start_gi.get("pool") or {}).get("active"),
        "active_end": settled_active,
        "active_end_raw": (end_gi.get("pool") or {}).get("active"),
        "active_bound": settled_active
        <= ((start_gi.get("pool") or {}).get("active") or 0) * 1.1 + 64,
        "watermark_bound": ((end_gi.get("pool") or {}).get("watermark") or 0)
        <= ((end_gi.get("pool") or {}).get("capacity") or 0),
        "memory_unchanged": (start_gi.get("memory") or {}).get("total_bytes")
        == (end_gi.get("memory") or {}).get("total_bytes"),
        "ledger_rows_unchanged": (start_sample["perf_stats"].get("scale") or {}).get("ledger_rows")
        == (end_sample["perf_stats"].get("scale") or {}).get("ledger_rows"),
        "upload_unchanged": (start_sample["perf_stats"].get("memory") or {}).get("upload_bytes_total")
        == (end_sample["perf_stats"].get("memory") or {}).get("upload_bytes_total"),
        "residency_requested_zero": end_res.get("requested_rows_last") == 0,
        "residency_ejected_zero": end_res.get("ejected_last") == 0,
        "command_count_unchanged": start_draw.get("command_count") == end_draw.get("command_count"),
        "return_diff_mean": tour_diff["mean_abs"],
        "return_diff_p99": tour_diff["p99"],
        "return_diff_changed": tour_diff["changed_fraction_16"],
    }
    ledger_ok = all(checks[k] for k in (
        "ledger_rows_unchanged", "upload_unchanged", "residency_requested_zero",
        "residency_ejected_zero", "command_count_unchanged"))
    pool_ok = all(checks[k] for k in (
        "failed_zero", "active_bound", "watermark_bound", "memory_unchanged"))
    result = {
        "condition_sheet": session.condition_sheet(),
        "baseline_sample": start_sample,
        "rows": rows,
        "start_snapshot": start_snapshot,
        "settle_series": settle_series,
        "settled_active": settled_active,
        "end_sample": end_sample,
        "end_snapshot": end_snapshot,
        "return_diff": tour_diff,
        "checks": checks,
        "ledger_ok": ledger_ok,
        "pool_ok": pool_ok,
        "verdict": "PASS" if ledger_ok and pool_ok else "FAIL",
    }
    session.save("tour.json", result)
    return result


def noise_limits(noise):
    return {
        "mean_abs": max(1.0, noise["mean_abs"] * 2),
        "p99": max(3.0, noise["p99"] * 2),
        "changed_fraction_16": 0.005,
    }


def _cam(pose):
    return pose["camera"] if isinstance(pose, dict) and "camera" in pose else pose


def cmd_dynamic(session):
    session.apply_baseline()
    field = build_field(session)
    session.set_camera(FIELD_CAMERA, settle=15)
    ref = session.snapshot("field_ref")
    ref_sample = session.sample()
    session.snapshot("field_ref2")
    noise = diff_frames(session.out / "field_ref.png", session.out / "field_ref2.png")
    cases = []

    # A: move the red column 4 m and back.
    column = field["red_column"]["entity"]
    for step in range(1, 9):
        session.brp.call("set_transform", {"entity": column,
                                           "translation": [80, 1.5, -2 + step * 0.5]})
        time.sleep(0.25)
    time.sleep(2)
    for step in range(7, -1, -1):
        session.brp.call("set_transform", {"entity": column,
                                           "translation": [80, 1.5, -2 + step * 0.5]})
        time.sleep(0.25)
    session.settle(15, "A move back")
    after_a = session.snapshot("field_after_move")
    diff_a = diff_frames(session.out / "field_ref.png", session.out / "field_after_move.png")
    cases.append({"case": "A_move_column", "diff": diff_a,
                  "threshold": noise_limits(noise),
                  "convergence": measure_stability(session, session.out / "field_ref.png", 8)})

    # B: spawn 20 spheres, hold, then despawn.
    spheres = []
    for row in range(4):
        for col in range(5):
            spheres.append(_field_shape(
                session, "sphere", [76.5 + col * 1.2, 0.6, 2.5 + row * 1.0],
                [0.4, 0.4, 0.4], [0.2, 0.4, 0.9], name=f"gi98-tmp-{row}-{col}")["entity"])
    time.sleep(5)
    for entity in spheres:
        session.brp.world_call("world.despawn_entity", {"entity": entity})
    session.settle(15, "B despawn")
    after_b = session.snapshot("field_after_spawn")
    diff_b = diff_frames(session.out / "field_ref.png", session.out / "field_after_spawn.png")
    cases.append({"case": "B_spawn_despawn", "diff": diff_b,
                  "threshold": noise_limits(noise),
                  "convergence": measure_stability(session, session.out / "field_ref.png", 8)})

    # C: chunk streaming at the Sponza origin (own reference).
    session.set_camera({"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 5, "radius": 5},
                       settle=15)
    origin_ref = session.snapshot("origin_ref")
    session.snapshot("origin_ref2")
    origin_noise = diff_frames(session.out / "origin_ref.png",
                               session.out / "origin_ref2.png")
    chunks = session.brp.call("generate_chunks", {"chunk_grid": 2, "instances_per_chunk": 16,
                                                  "spacing": 2.0, "seed": 7})
    time.sleep(5)
    census = session.brp.call("chunk_census", {})
    for chunk in census["chunks"]:
        session.brp.call("chunk_despawn", {"id": chunk["id"]})
    session.settle(15, "C chunks despawn")
    origin_after = session.snapshot("origin_after_chunks")
    diff_c = diff_frames(session.out / "origin_ref.png", session.out / "origin_after_chunks.png")
    cases.append({"case": "C_chunk_stream", "chunks": chunks,
                  "census_before_despawn": census, "diff": diff_c,
                  "noise": origin_noise, "threshold": noise_limits(origin_noise),
                  "convergence": measure_stability(session, session.out / "origin_ref.png", 8)})

    # D: workload spin, own reference at the field.
    session.set_camera(FIELD_CAMERA, settle=15)
    work_ref = session.snapshot("work_ref")
    session.brp.call("workload", {"enabled": True, "rows": 50})
    time.sleep(3)
    session.brp.call("workload", {"enabled": False, "rows": 0})
    session.settle(15, "D workload off")
    work_after = session.snapshot("work_after")
    diff_d = diff_frames(session.out / "work_ref.png", session.out / "work_after.png")
    cases.append({"case": "D_workload", "diff": diff_d,
                  "threshold": noise_limits(noise),
                  "convergence": measure_stability(session, session.out / "work_ref.png", 8)})

    for case in cases:
        limit = case["threshold"]
        case["pass"] = (case["diff"]["mean_abs"] <= limit["mean_abs"]
                        and case["diff"]["p99"] <= limit["p99"]
                        and case["diff"]["changed_fraction_16"]
                        <= limit["changed_fraction_16"])
    # Cleanup the field entities.
    for key, shape in field.items():
        session.brp.world_call("world.despawn_entity", {"entity": shape["entity"]})
    result = {
        "condition_sheet": session.condition_sheet(),
        "field": field,
        "reference": ref,
        "reference_sample": ref_sample,
        "noise_floor": noise,
        "origin_noise_floor": origin_noise,
        "thresholds": {case["case"]: case["threshold"] for case in cases},
        "cases": cases,
        "verdict": "PASS" if all(case["pass"] for case in cases) else "FAIL",
    }
    session.save("dynamic.json", result)
    return result


def cmd_lighting(session):
    session.apply_baseline()
    field = build_field(session)
    session.set_camera(FIELD_CAMERA, settle=15)
    ref = session.snapshot("light_ref")
    session.snapshot("light_ref2")
    noise = diff_frames(session.out / "light_ref.png", session.out / "light_ref2.png")
    light = session.brp.call("light_spawn", {
        "pos": [80, 2, -4], "radius": 10, "color": [1.0, 0.9, 0.8],
        "intensity": 10.0,
    })
    slot = light["slot"]
    steps = []
    for intensity in (0.0, 5000.0, 20000.0, 50000.0, 0.0):
        session.brp.call("light_update", {"slot": slot, "intensity": intensity})
        session.settle(5, f"light intensity {intensity}")
        steps.append({"intensity": intensity,
                      "snapshot": session.snapshot(f"light_{int(intensity)}"),
                      "sample": session.sample()})
    session.brp.call("light_remove", {"slot": slot})
    session.settle(15, "light removed")
    after_light = session.snapshot("light_after_remove")
    diff_light = diff_frames(session.out / "light_ref.png",
                             session.out / "light_after_remove.png")
    captures = []

    def capture():
        ibl = session.brp.call("ibl_stats")
        cap = ibl.get("capture") or {}
        captures.append({"at": now(), "count": cap.get("count"),
                         "last_capture_gpu_ms": cap.get("last_capture_gpu_ms"),
                         "window_max_gpu_ms": cap.get("window_max_gpu_ms")})

    capture()
    for azimuth in range(0, 360, 30):
        session.brp.call("sun_set", {"azimuth_deg": azimuth, "elevation_deg": 45.0})
        capture()
        time.sleep(0.4)
    for elevation in (10.0, 45.0, 80.0):
        session.brp.call("sun_set", {"azimuth_deg": 90.0, "elevation_deg": elevation})
        capture()
        time.sleep(0.4)
    session.brp.call("sun_set", {"azimuth_deg": 45.0, "elevation_deg": 55.0})
    capture()
    session.settle(15, "sun back")
    after_sun = session.snapshot("sun_after")
    diff_sun = diff_frames(session.out / "light_ref.png", session.out / "sun_after.png")
    perf = session.brp.call("perf_stats")
    # Auto-exposure sanity: it must not be the reason a diff looks converged.
    session.brp.call("exposure_set", {"mode": "auto"})
    auto = []
    for _ in range(10):
        time.sleep(1)
        auto.append(session.brp.call("exposure_get"))
    session.brp.call("exposure_set", {"mode": "manual"})
    session.settle(3)
    for shape in field.values():
        session.brp.world_call("world.despawn_entity", {"entity": shape["entity"]})
    result = {
        "condition_sheet": session.condition_sheet(),
        "field": field,
        "reference": ref,
        "noise_floor": noise,
        "light": light,
        "steps": steps,
        "after_remove": after_light,
        "diff_after_remove": diff_light,
        "captures": captures,
        "capture_count_delta": (captures[-1]["count"] or 0) - (captures[0]["count"] or 0),
        "gpu_ibl_capture_ms": (perf.get("gpu_ibl_capture_ms") or {}),
        "sun_after": after_sun,
        "diff_after_sun": diff_sun,
        "auto_exposure": auto,
        "threshold": noise_limits(noise),
    }
    limit = result["threshold"]
    result["pass"] = (
        diff_light["mean_abs"] <= limit["mean_abs"]
        and diff_light["p99"] <= limit["p99"]
        and diff_light["changed_fraction_16"] <= limit["changed_fraction_16"]
        and diff_sun["mean_abs"] <= limit["mean_abs"]
        and diff_sun["p99"] <= limit["p99"]
        and diff_sun["changed_fraction_16"] <= limit["changed_fraction_16"]
    )
    result["verdict"] = "PASS" if result["pass"] else "FAIL"
    session.save("lighting.json", result)
    return result


def cmd_boundary(session):
    session.apply_baseline()
    wall = _field_shape(session, "cuboid", [100, 3, 0], [0.6, 8, 16],
                        [0.8, 0.8, 0.8], name="gi98-boundary-wall")
    session.set_camera(BOUNDARY_CAMERA, settle=10)
    # The wall occupies x 356..1238, y 152..574 at 1600x900; sample its inside.
    roi = [450, 200, 700, 300]
    rows = []
    for size, cascades in ((32, 4), (128, 4), (512, 6)):
        session.apply_baseline(size=size, cascades=cascades, capacity=65536)
        session.set_camera(BOUNDARY_CAMERA, settle=10)
        scgi = session.brp.call("scgi_stats", {"stride": 4})
        roi_stats = session.brp.call("scgi_stats", {"stride": 1, "roi": roi})
        snap = session.snapshot(f"size_{size}")
        session.brp.call("gi_set", {"scgi": False})
        session.settle(2)
        off = session.snapshot(f"size_{size}_scgi_off")
        session.brp.call("gi_set", {"scgi": True})
        session.settle(3)
        on_again = session.snapshot(f"size_{size}_scgi_on")
        diff = diff_frames(session.out / f"size_{size}_scgi_off.png",
                           session.out / f"size_{size}_scgi_on.png")
        rows.append({"size": size, "cascades": cascades, "scgi_stats": scgi,
                     "roi": roi, "roi_stats": roi_stats, "snapshot": snap,
                     "on_off_diff": diff})
        print(f"[boundary] size={size} coverage={scgi.get('coverage')} "
              f"roi_coverage={roi_stats.get('coverage')} onoff={diff['mean_abs']}")
    # Fallback baseline A/B outside the narrow volume: SH-L2 vs GlobalProbe.
    session.apply_baseline(size=32, cascades=4, capacity=65536)
    session.set_camera(BOUNDARY_CAMERA, settle=8)
    session.brp.call("ibl_set", {"diffuse_mode": "sh_l2"})
    session.settle(2)
    sh_l2 = session.snapshot("fallback_sh_l2")
    session.brp.call("ibl_set", {"diffuse_mode": "probe16"})
    session.settle(2)
    probe16 = session.snapshot("fallback_probe16")
    fallback_diff = diff_frames(session.out / "fallback_sh_l2.png",
                                session.out / "fallback_probe16.png")
    session.brp.call("ibl_set", {"diffuse_mode": "sh_l2"})
    # Upsampling A/B inside the default volume: look for a boundary ring.
    session.apply_baseline(size=128, cascades=4, capacity=65536)
    session.set_camera(BOUNDARY_CAMERA, settle=8)
    session.set_gi({"upsample_bilinear": True})
    session.settle(3)
    bilinear = session.snapshot("upsample_on")
    session.set_gi({"upsample_bilinear": False})
    session.settle(3)
    nearest = session.snapshot("upsample_off")
    upsample_diff = diff_frames(session.out / "upsample_off.png",
                                session.out / "upsample_on.png")
    # Scroll the narrow volume 40 m and return.
    session.apply_baseline(size=32, cascades=4, capacity=65536)
    session.set_camera(BOUNDARY_CAMERA, settle=10)
    before = session.snapshot("scroll_before")
    session.set_camera({"target": [128, 2, 0], "yaw_deg": -90, "pitch_deg": 2, "radius": 8},
                       settle=3)
    session.set_camera(BOUNDARY_CAMERA, settle=10)
    after = session.snapshot("scroll_after")
    scroll_diff = diff_frames(session.out / "scroll_before.png", session.out / "scroll_after.png")
    session.brp.world_call("world.despawn_entity", {"entity": wall["entity"]})
    session.apply_baseline()
    result = {
        "condition_sheet": session.condition_sheet(),
        "roi": roi,
        "sizes": rows,
        "fallback": {"sh_l2": sh_l2, "probe16": probe16, "diff": fallback_diff},
        "upsample": {"off": nearest, "on": bilinear, "diff": upsample_diff},
        "scroll": {"before": before, "after": after, "diff": scroll_diff},
    }
    session.save("boundary.json", result)
    return result


def cmd_probe(session):
    result = {"condition_sheet": session.condition_sheet(),
              "sample": session.sample()}
    session.save("probe.json", result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=("baseline", "tour", "dynamic", "lighting", "boundary", "probe"))
    parser.add_argument("--port", type=int, default=15703)
    parser.add_argument("--exe", default=str(DEFAULT_EXE))
    parser.add_argument("--scene", default=DEFAULT_SCENE)
    parser.add_argument("--out", default=str(DEFAULT_OUT))
    parser.add_argument("--keep", action="store_true", help="keep a launched renderer alive")
    args = parser.parse_args()
    Path(args.out).mkdir(parents=True, exist_ok=True)
    session = Session(args, args.command)
    try:
        result = {
            "baseline": cmd_baseline,
            "tour": cmd_tour,
            "dynamic": cmd_dynamic,
            "lighting": cmd_lighting,
            "boundary": cmd_boundary,
            "probe": cmd_probe,
        }[args.command](session)
        print(json.dumps({key: result.get(key) for key in
                          ("verdict", "checks", "threshold") if key in result},
                         ensure_ascii=False, indent=2))
    finally:
        session.close()


if __name__ == "__main__":
    main()
