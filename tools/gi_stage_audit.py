"""Separate GI producer, cache filtering, and screen-gather noise through BRP.

Writes measurements and final-frame PNGs without declaring visual acceptance.
The live renderer is configured for each labelled experiment, then restored.
Usage: python tools/gi_stage_audit.py --out .temp/gi97-stage-audit --frames 24
"""

import argparse
import json
import subprocess
import sys
import time
from pathlib import Path

sys.dont_write_bytecode = True

import numpy as np
from PIL import Image

from gi_scene_regression import ROOT, CAMERAS, SUNS, call, camera_matches, display_srgb8


def scene_stamp():
    return {
        "camera": call("set_camera", {"zoom": 1})["orbit"],
        "sun": call("sun_get"),
        "ibl": call("ibl_stats")["settings"],
    }


BASE = {
    "enabled": True,
    "estimation": True,
    "scgi": True,
    "view": 0,
    "samples": 8,
    "budget": 262144,
    "normal_source": 0,
    "multi_bounce": True,
    "spatial_filter": True,
    "temporal_filter": True,
    "denoise_passes": 2,
    "lookup_samples": 3,
    "short_hysteresis": 0.85,
}

EXPERIMENTS = {
    "baseline": {},
    "no_multibounce": {"multi_bounce": False},
    "no_patch_spatial": {"spatial_filter": False},
    "seed_only_gather": {"lookup_samples": 1},
    "held_estimate": {"budget": 1, "short_hysteresis": 0.99},
    "shading_normal": {"normal_source": 1},
    "ibl_only": {"scgi": False},
}


def temporal_stats(frames, stable_dark):
    luminance = np.stack(frames).astype(np.float32).mean(axis=3)
    deviation = luminance.std(axis=0)
    mean = luminance.mean(axis=0)
    return {
        "mean_luma": float(mean.mean()),
        "mean_temporal_std": float(deviation.mean()),
        "p99_temporal_std": float(np.percentile(deviation, 99)),
        "fixed_dark_temporal_std": float(deviation[stable_dark].mean()),
        "fixed_dark_p99": float(np.percentile(deviation[stable_dark], 99)),
        "black_fraction": float((mean < 5.0).mean()),
    }


def cache_probes():
    result = []
    for index in (0, 32, 64, 128, 256, 512, 1024, 2048):
        value = call("gi_estimate_probe", {"ring": index})
        result.append(value)
    return result


def pixel_probes(screen):
    width, height = screen["extent"]["width"], screen["extent"]["height"]
    return [call("gi_estimate_probe", {"pixel": [min(int(x * width), width - 1),
                                                 min(int(y * height), height - 1)]})
            for y in (0.2, 0.4, 0.6, 0.8, 0.95)
            for x in (0.2, 0.35, 0.5, 0.65, 0.8)]


def reencode_saved(output):
    """Correct display encoding of retained evidence without modifying old reports."""
    report = json.loads((output / "report.json").read_text(encoding="utf-8"))
    supplemental = {"correction_only": True, "runtime_retested": False,
                    "source_report": "report.json", "cases": {}}
    fixed_dark = None
    for label, old in report.items():
        paths = sorted((output / label).glob("frame*.ppm"))
        if len(paths) < 2:
            raise RuntimeError(f"Insufficient retained frames for {label}")
        frames = [np.asarray(Image.open(path), dtype=np.uint8) for path in paths]
        if fixed_dark is None:
            reference = np.stack(frames).mean(axis=(0, 3))
            fixed_dark = reference < np.percentile(reference, 25)
        display_frames = display_srgb8(np.stack(frames))
        Image.fromarray(display_frames[-1]).save(output / label / "last-display.png")
        row = {"linear": temporal_stats(frames, fixed_dark),
               "display": temporal_stats(display_frames, fixed_dark),
               "frames": len(frames), "scene_before": old["scene_before"],
               "scene_after": old["scene_after"]}
        supplemental["cases"][label] = row
        print(label, json.dumps(row), flush=True)
    (output / "display-report.json").write_text(
        json.dumps(supplemental, ensure_ascii=False, indent=2), encoding="utf-8")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", default=".temp/gi97-stage-audit")
    parser.add_argument("--frames", type=int, default=24)
    parser.add_argument("--settle", type=float, default=6.0)
    parser.add_argument("--variants", default=",".join(EXPERIMENTS))
    parser.add_argument("--camera", choices=[name for name, _ in CAMERAS])
    parser.add_argument("--sun", choices=[name for name, _, _ in SUNS])
    parser.add_argument("--pid", type=int, help="Also capture each variant's actual window")
    parser.add_argument("--pixel-probes", action="store_true",
                        help="Include same-snapshot Gbuffer/cache/output point probes (new renderer)")
    parser.add_argument("--reencode-only", action="store_true",
                        help="Recompute display-domain evidence from retained PPMs; no BRP calls")
    parser.add_argument("--exposure", choices=("manual", "auto"), default="manual",
                        help="Pin the display-stage auto exposure; manual keeps brightness "
                             "comparable with pre-exposure baselines")
    args = parser.parse_args()
    variants = args.variants.split(",")
    if args.frames < 2 or any(name not in EXPERIMENTS for name in variants):
        parser.error("Need at least two frames and valid experiment names")
    output = (ROOT / args.out).resolve()
    output.mkdir(parents=True, exist_ok=True)
    if args.reencode_only:
        return reencode_saved(output)
    original = call("gi_stats")["settings"]
    restore = {key: value for key, value in original.items() if key != "config"}
    initial_scene = scene_stamp()
    expected_camera = dict(CAMERAS).get(args.camera)
    expected_sun = next(({"azimuth_deg": az, "elevation_deg": el, "illuminance": 20000}
                         for name, az, el in SUNS if name == args.sun), None)
    report = {}
    fixed_dark = None
    try:
        call("exposure_set", {"mode": args.exposure})
        if expected_camera:
            call("set_camera", expected_camera)
        if expected_sun:
            call("sun_set", expected_sun)
        for label in variants:
            settings = {**BASE, **EXPERIMENTS[label]}
            call("gi_set", settings)
            # Let the actual GPU history converge, not just the ECS settings.
            time.sleep(args.settle)
            before = call("gi_stats")
            scene_before = scene_stamp()
            if expected_camera and not camera_matches(scene_before["camera"], expected_camera):
                raise RuntimeError(f"Camera did not reach requested pose: {scene_before['camera']}")
            if expected_sun and any(abs(scene_before["sun"][key] - value) > 0.001
                                    for key, value in expected_sun.items()):
                raise RuntimeError(f"Sun did not reach requested state: {scene_before['sun']}")
            directory = output / label
            directory.mkdir(parents=True, exist_ok=True)
            frames = []
            for index in range(args.frames):
                path = directory / f"frame{index:03d}.ppm"
                call("frame_dump", {"path": str(path)})
                with Image.open(path) as image:
                    frames.append(np.asarray(image.convert("RGB"), dtype=np.uint8))
            if fixed_dark is None:
                reference = np.stack(frames).mean(axis=(0, 3))
                fixed_dark = reference < np.percentile(reference, 25)
                if not fixed_dark.any():
                    raise RuntimeError("Baseline contains no measurable dark region")
            display_frames = display_srgb8(np.stack(frames))
            Image.fromarray(display_frames[-1]).save(directory / "last.png")
            Image.fromarray(frames[-1]).save(directory / "last-linear.png")
            if args.pid:
                subprocess.run([sys.executable, str(ROOT / "tools/capture_window.py"),
                                "--pid", str(args.pid), "--out", str(directory / "last-window.png")],
                               check=True)
            scene_after = scene_stamp()
            if not camera_matches(scene_after["camera"], scene_before["camera"]):
                raise RuntimeError("Camera changed during a stationary sequence")
            if any(abs(scene_after["sun"][key] - scene_before["sun"][key]) > 0.001
                   for key in ("azimuth_deg", "elevation_deg", "illuminance")):
                raise RuntimeError("Sun changed during a stationary sequence")
            screen = call("scgi_stats")
            points = pixel_probes(screen) if args.pixel_probes else []
            probe_scene_after = scene_stamp() if args.pixel_probes else scene_after
            if not camera_matches(probe_scene_after["camera"], scene_before["camera"]):
                raise RuntimeError("Camera changed during pixel probes")
            if any(abs(probe_scene_after["sun"][key] - scene_before["sun"][key]) > 0.001
                   for key in ("azimuth_deg", "elevation_deg", "illuminance")):
                raise RuntimeError("Sun changed during pixel probes")
            row = {
                "settings": settings,
                "scene_before": scene_before,
                "scene_after": scene_after,
                "gi_before": before,
                "gi_after": call("gi_stats"),
                "screen": screen,
                "probes": cache_probes(),
                "pixel_probes": points,
                "probe_scene_after": probe_scene_after,
                "temporal": temporal_stats(frames, fixed_dark),
                "temporal_color_space": "linear_srgb_unorm8",
                "display_temporal": temporal_stats(display_frames, fixed_dark),
                "png_encoding": "srgb",
                "unique_frames": len({frame.tobytes() for frame in frames}),
                "exposure": call("exposure_get"),
            }
            report[label] = row
            (output / "report.json").write_text(
                json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8"
            )
            print(label, "linear", json.dumps(row["temporal"]),
                  "display", json.dumps(row["display_temporal"]),
                  "unique_frames", row["unique_frames"], flush=True)
    finally:
        call("gi_set", restore)
        if expected_camera:
            call("set_camera", initial_scene["camera"])
        if expected_sun:
            call("sun_set", {key: initial_scene["sun"][key]
                             for key in ("azimuth_deg", "elevation_deg", "illuminance")})
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
