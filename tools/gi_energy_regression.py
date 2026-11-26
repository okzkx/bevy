"""Run precise HDR sky-energy, budget-stride and producer-gating regressions.

Usage: python tools/gi_energy_regression.py --pid PID
Requires an idle BRP renderer; creates one isolated test surface at x=80.
All output goes to .temp/gi97-energy. Uses BRP, never desktop input.
"""

import argparse
import json
import math
import sys
import time

# tools/* is a Cargo workspace glob; do not create tools/__pycache__/.
sys.dont_write_bytecode = True
from gi_scene_regression import call, snapshot, ROOT, rpc


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", required=True, type=int)
    args = parser.parse_args()
    out = ROOT / ".temp/gi97-energy"
    out.mkdir(parents=True, exist_ok=True)
    checks = []

    def check(name, ok, evidence):
        checks.append({"name": name, "pass": bool(ok), "evidence": evidence})
        print("PASS" if ok else "FAIL", name, json.dumps(evidence), flush=True)
        (out / "evidence.json").write_text(json.dumps(checks, indent=2), encoding="utf-8")

    initial_gi = call("gi_stats")["settings"]
    initial_sun = call("sun_get")
    initial_ibl = call("ibl_stats")["settings"]
    spawned = call("spawn_shape", {"shape": "cuboid", "color": [0.8, 0.8, 0.8],
                         "scale": [30, 0.2, 30], "translation": [80, -0.1, -60],
                         "name": "gi_energy_anchor"})
    try:
        call("set_camera", {"target": [80, 0, -60], "yaw_deg": 0, "pitch_deg": 89, "radius": 6})
        call("sky_uniform", {"enabled": True, "color": [1000, 1000, 1000]})
        call("sun_set", {"azimuth_deg": 0, "elevation_deg": 0, "illuminance": 0})
        call("ibl_set", {"enabled": True, "intensity": 1})
        call("gi_set", {"enabled": True, "estimation": True, "scgi": True, "view": 0,
                        "samples": 4, "warmup": 4, "multi_bounce": False,
                        "spatial_filter": True, "temporal_filter": True,
                        "denoise_passes": 2, "budget": 262144})
        roi = [600, 300, 400, 300]
        for n_source in [0, 1]:
            call("gi_set", {"normal_source": n_source})
            for intensity in [0.25, 1, 2]:
                call("ibl_set", {"intensity": intensity})
                time.sleep(5)
                stats = call("scgi_stats", {"stride": 2, "roi": roi})
                expected = math.pi * 1000 * intensity
                errors = [abs(v / expected - 1) for v in stats["l0_covered_mean"]]
                check(f"HDR sky E=pi*L*I normal={n_source} I={intensity}",
                      max(errors) < 0.05 and stats["coverage"] > 0.99
                      and stats["non_finite"] == 0 and stats["negative"] == 0,
                      {"expected": expected, "relative_errors": errors, "stats": stats})
                snapshot(out, f"anchor-n{n_source}-i{intensity}", args.pid)
        call("ibl_set", {"intensity": 1})
        # Converge the changed illumination before diluting updates: stride is a
        # history-retention check, not an instant lighting-change convergence test.
        time.sleep(5)
        call("gi_set", {"samples": 1, "budget": 1})
        time.sleep(5)
        samples = [call("scgi_stats", {"stride": 2, "roi": roi}) for _ in range(4)]
        gi = call("gi_stats")
        values = [sum(s["l0_covered_mean"]) / 3 for s in samples]
        check("budget dilution retains the latest ping-pong energy",
              all(abs(v / (math.pi * 1000) - 1) < 0.05 for v in values)
              and gi["estimation"]["stride"] > 1,
              {"values": values, "stride": gi["estimation"]["stride"]})
        call("gi_set", {"budget": 262144, "scgi": False})
        time.sleep(1)
        snapshot(out, "baseline-off", args.pid)
        baseline = (out / "baseline-off.ppm").read_bytes()
        for field in ["enabled", "estimation"]:
            call("gi_set", {"enabled": True, "estimation": True, "scgi": True, field: False})
            time.sleep(1)
            snapshot(out, "producer-" + field + "-off", args.pid)
            check(f"producer {field}=false falls back exactly to IBL",
                  baseline == (out / ("producer-" + field + "-off.ppm")).read_bytes(), {})
        call("gi_set", {"enabled": True, "estimation": True, "scgi": True})
        time.sleep(2)
        stats = call("scgi_stats", {"stride": 2, "roi": roi})
        check("restart has finite current illumination", stats["non_finite"] == 0
              and max(abs(v / (math.pi * 1000) - 1) for v in stats["l0_covered_mean"]) < 0.05, stats)
    finally:
        cleanup = rpc("127.0.0.1", 15702, "world.despawn_entity", {"entity": spawned["entity"]})
        if "error" in cleanup:
            print("Fixture cleanup failed:", cleanup["error"], flush=True)
        call("sky_uniform", {"enabled": False})
        call("sun_set", {k: initial_sun[k] for k in ["azimuth_deg", "elevation_deg", "illuminance"]})
        call("ibl_set", {"enabled": initial_ibl["enabled"], "intensity": initial_ibl["intensity"],
                         "diffuse_mode": initial_ibl["diffuse_mode"]})
        keys = ["enabled", "estimation", "scgi", "view", "samples", "warmup", "multi_bounce",
                "spatial_filter", "temporal_filter", "denoise_passes", "budget", "normal_source"]
        call("gi_set", {k: initial_gi[k] for k in keys})
        call("set_camera", {"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 10, "radius": 9})
    return 0 if all(c["pass"] for c in checks) else 1


if __name__ == "__main__":
    raise SystemExit(main())
