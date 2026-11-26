"""Read GI cache records at known Sponza floor/wall positions through BRP.

Usage: python tools/gi_point_audit.py --out .temp/gi97-points.json
Records CPU/GPU evidence only; does not decide visual correctness. Optional
one-shot GPU diagnostic fields are preserved if the running build exports them.
"""

import argparse
import json
import sys
import time
from pathlib import Path

sys.dont_write_bytecode = True
from gi_scene_regression import call, ROOT


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", default=".temp/gi97-points.json")
    args = parser.parse_args()
    result = []
    call("set_camera", {"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 5, "radius": 5})
    call("gi_set", {"enabled": True, "estimation": True, "scgi": True,
                    "spatial_filter": True, "temporal_filter": True, "samples": 8})
    call("ibl_set", {"enabled": True, "intensity": 1})
    for sun in [{"azimuth_deg": -27, "elevation_deg": 27},
                {"azimuth_deg": 45, "elevation_deg": 65},
                {"azimuth_deg": 160, "elevation_deg": 35}]:
        call("sun_set", sun)
        time.sleep(5)
        points = []
        for x in [-10, -6, -2, 2, 6, 10]:
            for z in [-2, 0, 2]:
                position, normal = [x, -0.02, z], [0, 1, 0]
                value = call("gi_estimate_probe", {"position": position, "normal": normal})
                points.append({"position": position, "normal": normal, "cache": value})
        for y in [1, 3, 5, 7]:
            position, normal = [-11.5, y, 0], [1, 0, 0]
            value = call("gi_estimate_probe", {"position": position, "normal": normal})
            points.append({"position": position, "normal": normal, "cache": value})
        row = {"sun": sun, "stats": call("gi_stats"), "screen": call("scgi_stats"), "points": points}
        result.append(row)
        print(json.dumps(row, ensure_ascii=False), flush=True)
        target = ROOT / args.out
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    call("gi_set", {"samples": 2})
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
