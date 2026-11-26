"""Isolate which SCGI stage injects the dark-area temporal flicker (BRP, read-mostly).

Usage:
  python tools/gi_flicker_isolate.py --pid PID --probes 24

For each configuration variant, track a fixed set of pixel probes across
consecutive frames and report the relative temporal std of:
  raw   = patch estimate (irradiance_a) evaluated at the pixel shading normal
  sp    = patch spatial filter output (b14)
  tp    = patch temporal filter output (b15)
  screen= final scgi_irradiance (gather + denoise + upsample) at the pixel
  alpha = coverage written next to screen
  hdr   = post-composite HDR pixel

Variants turn one stage off at a time so the stage that owns the variance is
identified by difference. Restores the original GI/IBL settings in `finally`.
Diagnostic measurements only; no visual verdict.
"""

import argparse
import json
import sys
import time
from pathlib import Path

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))

from gi_scene_regression import call, ROOT  # noqa: E402

import numpy as np  # noqa: E402

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

VARIANTS = [
    ("baseline", {}),
    ("no_spatial", {"spatial_filter": False}),
    ("no_temporal", {"temporal_filter": False}),
    ("sample1", {"lookup_samples": 1}),
    ("sample2", {"lookup_samples": 2}),
    ("no_denoise", {"denoise_passes": 0}),
    ("denoise4", {"denoise_passes": 4}),
]


def evaluate(irradiance, normal):
    if not irradiance:
        return None
    return [
        max(
            irradiance["l0"][i]
            + irradiance["l1x"][i] * normal[0]
            + irradiance["l1y"][i] * normal[1]
            + irradiance["l1z"][i] * normal[2],
            0.0,
        )
        for i in range(3)
    ]


def series(pixels, frames):
    rows = {tuple(p): [] for p in pixels}
    for _ in range(frames):
        for pixel in pixels:
            probe = call("gi_estimate_probe", {"pixel": list(pixel)})
            if not probe.get("alive"):
                rows[tuple(pixel)].append(None)
                continue
            normal = probe["input"]["shading_normal"]
            rows[tuple(pixel)].append({
                "raw": evaluate(probe["irradiance_a"], normal),
                "sp": evaluate(probe["irradiance_spatial"], normal),
                "tp": evaluate(probe["irradiance_temporal"], normal),
                "screen": list(probe["input"]["screen_irradiance"])[:3],
                "alpha": probe["input"]["screen_irradiance"][3],
                "hdr": list(probe["input"]["hdr"])[:3],
                "patch": probe.get("patch"),
            })
    return rows


def summarize(rows):
    out = {}
    for pixel, items in rows.items():
        ok = [item for item in items if item]
        entry = {"alive": len(ok), "total": len(items)}
        if not ok:
            out[str(list(pixel))] = entry
            continue
        for key in ("raw", "sp", "tp", "screen", "hdr"):
            values = np.array([item[key] for item in ok], dtype=np.float64)
            mean = values.mean(axis=0)
            rel = values.std(axis=0) / np.maximum(np.abs(mean), 1e-9)
            entry[key] = {
                "mean": [float(v) for v in mean],
                "rel_std": [float(v) for v in rel],
                "rel_std_max": float(rel.max()),
            }
        alpha = np.array([item["alpha"] for item in ok], dtype=np.float64)
        entry["alpha"] = {"mean": float(alpha.mean()), "std": float(alpha.std()),
                          "min": float(alpha.min()), "max": float(alpha.max())}
        slots = [item["patch"] for item in ok]
        entry["patch_slots"] = len(set(slots))
        out[str(list(pixel))] = entry
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", required=True, type=int)
    parser.add_argument("--probes", type=int, default=24, help="frames per variant")
    parser.add_argument("--pixels", default=".temp/gi97-hot-pixels.json")
    parser.add_argument("--out", default=str(ROOT / ".temp/gi97-flicker-isolate.json"))
    args = parser.parse_args()

    hot = json.loads(Path(args.pixels).read_text(encoding="utf-8"))[:6]
    initial_gi = call("gi_stats")["settings"]
    initial_ibl = call("ibl_stats")["settings"]
    report = {"pixels": hot, "variants": {}, "probes": args.probes}
    try:
        call("ibl_set", {"enabled": True, "intensity": 1.0})
        call("sun_set", {"azimuth_deg": 90, "elevation_deg": 70, "illuminance": 20000})
        call("set_camera", {"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 5, "radius": 5})
        call("gi_set", BASE)
        time.sleep(4)
        for name, override in VARIANTS:
            settings = dict(BASE)
            settings.update(override)
            call("gi_set", settings)
            time.sleep(3)
            rows = series(hot, args.probes)
            summary = summarize(rows)
            report["variants"][name] = {"override": override, "pixels": summary}
            means = [v["screen"]["rel_std_max"] for v in summary.values()
                     if v.get("screen")]
            tps = [v["tp"]["rel_std_max"] for v in summary.values() if v.get("tp")]
            alphas = [v["alpha"]["std"] for v in summary.values() if v.get("alpha")]
            print(f"[{name:12s}] screen_relstd p50={np.median(means) if means else -1:.4f} "
                  f"max={max(means) if means else -1:.4f} | tp_relstd "
                  f"p50={np.median(tps) if tps else -1:.4f} | alpha_std "
                  f"p50={np.median(alphas) if alphas else -1:.4f}", flush=True)
            Path(args.out).write_text(json.dumps(report, indent=1), encoding="utf-8")
        call("gi_set", BASE)
        print(json.dumps({"out": args.out}, ensure_ascii=False), flush=True)
    finally:
        call("gi_set", {k: v for k, v in initial_gi.items()
                        if k in BASE or k in ("light_samples", "emissive_intensity",
                                              "spatial_radius", "temporal_smoothing",
                                              "upsample_bilinear", "form", "seed",
                                              "warmup", "defrag_waves")})
        call("ibl_set", {"enabled": initial_ibl["enabled"],
                         "intensity": initial_ibl["intensity"]})


if __name__ == "__main__":
    main()
