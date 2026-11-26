"""Measure per-pixel temporal flicker of the final frame with a static camera.

Usage: python tools/gi_flicker_test.py --pid PID [--frames 12]
Writes .temp/gi97-flicker/ (frames + report). SCGI on/off are compared so the
measurement separates GI noise from the rest of the pipeline.
"""

import argparse
import json
import sys

import numpy as np
from PIL import Image

sys.dont_write_bytecode = True
from gi_scene_regression import ROOT, call


def dump(index, folder):
    path = folder / f"frame{index:02d}.ppm"
    call("frame_dump", {"path": str(path)})
    return np.asarray(Image.open(path), dtype=np.float32)


def stats(frames):
    stack = np.stack(frames)
    lum = stack.mean(axis=3)
    temporal_std = lum.std(axis=0)
    mean = lum.mean(axis=0)
    dark = mean < np.percentile(mean, 25)
    return {
        "mean_temporal_std": float(temporal_std.mean()),
        "p99_temporal_std": float(np.percentile(temporal_std, 99)),
        "dark_area_temporal_std": float(temporal_std[dark].mean()),
        "dark_area_p99": float(np.percentile(temporal_std[dark], 99)),
        "mean_luma": float(mean.mean()),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", required=True, type=int)
    parser.add_argument("--frames", type=int, default=12)
    args = parser.parse_args()
    folder = ROOT / ".temp/gi97-flicker"
    folder.mkdir(parents=True, exist_ok=True)
    call("set_camera", {"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 5, "radius": 5})
    call("sun_set", {"azimuth_deg": -27, "elevation_deg": 27, "illuminance": 20000})
    call("ibl_set", {"enabled": True, "intensity": 1})
    call("gi_set", {"enabled": True, "estimation": True, "spatial_filter": True,
                    "temporal_filter": True, "denoise_passes": 2, "samples": 2, "view": 0})
    report = {}
    for label, scgi in [("scgi_off", False), ("scgi_on", True)]:
        call("gi_set", {"scgi": scgi})
        import time
        time.sleep(6)
        frames = [dump(i, folder) for i in range(args.frames)]
        report[label] = stats(frames)
        print(label, json.dumps(report[label]), flush=True)
    (folder / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    ratio = report["scgi_on"]["dark_area_temporal_std"] / max(report["scgi_off"]["dark_area_temporal_std"], 1e-6)
    print(f"dark-area flicker ratio on/off = {ratio:.1f}x", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
