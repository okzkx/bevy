"""Capture a BRP-driven Sponza GI matrix without injecting desktop input.

Usage: python tools/gi_scene_regression.py --pid PID --label before|after
Outputs: .temp/gi97-scene/<label>/ (window PNG, renderer PPM, state JSON).
This records evidence, not an automatic claim of visual correctness. The caller
must inspect the PNGs and compare shadowed geometry against the IBL baseline.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time

import numpy as np
from PIL import Image

# tools/* is a Cargo workspace glob: keep Python module caches out of tools/.
sys.dont_write_bytecode = True
from brp import rpc

ROOT = Path(__file__).resolve().parents[1]
CAMERAS = [
    ("default", {"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 5, "radius": 5}),
    ("gallery", {"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 10, "radius": 9}),
    ("courtyard", {"target": [0, 1.5, 0], "yaw_deg": 90, "pitch_deg": 8, "radius": 3}),
    ("overhead", {"target": [0, 1.0, 0], "yaw_deg": 90, "pitch_deg": 65, "radius": 5}),
]
SUNS = [("low", -27, 27), ("high", 45, 65), ("reverse", 160, 35),
        ("along_hall", 90, 70), ("along_hall_reverse", 270, 70)]


def call(method, params=None):
    payload = rpc("127.0.0.1", 15702, "ash_renderer/" + method, params or {}, timeout=240)
    if "error" in payload:
        raise RuntimeError(f"{method}: {payload['error']}")
    return payload["result"]


def display_srgb8(linear_rgb8):
    """Encode linear UNORM readback for display; keep the source bytes for physics."""
    linear = np.asarray(linear_rgb8, dtype=np.float32) / 255.0
    encoded = np.where(linear <= 0.0031308, linear * 12.92,
                       1.055 * np.power(linear, 1.0 / 2.4) - 0.055)
    return np.rint(np.clip(encoded, 0.0, 1.0) * 255.0).astype(np.uint8)


def camera_matches(actual, expected):
    return (all(abs(actual[key] - expected[key]) < 0.001 for key in ("yaw_deg", "pitch_deg", "radius"))
            and all(abs(a - b) < 0.001 for a, b in zip(actual["target"], expected["target"])))


def snapshot(out, name, pid, expected_camera=None):
    before = call("set_camera", {"zoom": 1})["orbit"] if expected_camera else None
    ppm = out / (name + ".ppm")
    call("frame_dump", {"path": str(ppm)})
    subprocess.run([sys.executable, str(ROOT / "tools/capture_window.py"),
                    "--pid", str(pid), "--out", str(out / (name + "-window.png"))],
                   check=True)
    # LDR is linear UNORM; only display PNG gets the swapchain's sRGB encoding.
    raw = np.asarray(Image.open(ppm), dtype=np.uint8)
    display = display_srgb8(raw)
    Image.fromarray(display).save(out / (name + ".png"))
    lum = raw.astype(np.float32) @ np.array([0.2126, 0.7152, 0.0722])
    display_lum = display.astype(np.float32) @ np.array([0.2126, 0.7152, 0.0722])
    after = call("set_camera", {"zoom": 1})["orbit"] if expected_camera else None
    return {"mean_luma": float(lum.mean()), "p10_luma": float(np.percentile(lum, 10)),
            "p50_luma": float(np.percentile(lum, 50)), "black_fraction": float((lum < 5).mean()),
            "color_space": "linear_srgb", "png_encoding": "srgb",
            "display_mean_luma": float(display_lum.mean()),
            "display_black_fraction": float((display_lum < 5).mean()),
            "orbit_before": before, "orbit_after": after,
            "pose_valid": not expected_camera or (camera_matches(before, expected_camera) and camera_matches(after, expected_camera))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", required=True, type=int)
    parser.add_argument("--label", required=True)
    parser.add_argument("--settle", type=float, default=12)
    args = parser.parse_args()
    out = ROOT / ".temp/gi97-scene" / args.label
    out.mkdir(parents=True, exist_ok=True)
    initial_gi = call("gi_stats")["settings"]
    initial_ibl = call("ibl_stats")["settings"]
    initial_sun = call("sun_get")
    evidence = {"initial_gi": initial_gi, "initial_ibl": initial_ibl,
                "initial_sun": initial_sun, "cases": []}
    (out / "evidence.json").write_text(json.dumps(evidence, indent=2), encoding="utf-8")
    try:
        call("gi_set", {"enabled": True, "estimation": True, "multi_bounce": True,
                        "spatial_filter": True, "temporal_filter": True,
                        "denoise_passes": 2, "scgi": True, "view": 0})
        call("ibl_set", {"enabled": True, "intensity": 1})
        for camera_name, camera in CAMERAS:
            call("set_camera", camera)
            for sun_name, azimuth, elevation in SUNS:
                applied_camera = call("set_camera", camera)["orbit"]
                call("sun_set", {"azimuth_deg": azimuth, "elevation_deg": elevation})
                time.sleep(args.settle)
                case = {"camera": camera_name, "orbit": camera, "applied_orbit": applied_camera, "sun": sun_name,
                        "azimuth_deg": azimuth, "elevation_deg": elevation}
                prefix = camera_name + "-" + sun_name
                case["gi_stats"] = call("gi_stats")
                case["scgi_stats"] = call("scgi_stats")
                for enabled, suffix in [(True, "gi"), (False, "ibl")]:
                    call("gi_set", {"scgi": enabled})
                    time.sleep(1)
                    case[suffix] = snapshot(out, prefix + "-" + suffix, args.pid, camera)
                call("gi_set", {"scgi": True})
                evidence["cases"].append(case)
                (out / "evidence.json").write_text(json.dumps(evidence, indent=2), encoding="utf-8")
                print(prefix, json.dumps({"gi": case["gi"], "ibl": case["ibl"],
                                         "E": case["scgi_stats"].get("l0_covered_mean")}), flush=True)
    finally:
        call("sun_set", {k: initial_sun[k] for k in ("azimuth_deg", "elevation_deg")})
        call("ibl_set", {"enabled": initial_ibl["enabled"], "intensity": initial_ibl["intensity"],
                         "diffuse_mode": initial_ibl["diffuse_mode"]})
        allowed = ("enabled", "estimation", "multi_bounce", "spatial_filter", "temporal_filter",
                   "denoise_passes", "scgi", "view")
        call("gi_set", {key: initial_gi[key] for key in allowed})
        call("set_camera", CAMERAS[0][1])
    return 0 if all(row[k]["pose_valid"] for row in evidence["cases"] for k in ["gi", "ibl"]) else 1


if __name__ == "__main__":
    raise SystemExit(main())
