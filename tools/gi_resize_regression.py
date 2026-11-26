"""Verify SCGI targets during same-quarter-bucket window resizes through BRP.

Usage: python tools/gi_resize_regression.py --pid PID [--entity 69v0]
No desktop input is injected. Restores the original physical window resolution.
Output: .temp/gi97-resize/ (window/GPU PNGs and measured extents).
"""

import argparse
import json
import sys
import time

sys.dont_write_bytecode = True
from gi_scene_regression import ROOT, call, rpc, snapshot

WINDOW = "bevy_window::window::Window"


def raw(method, params):
    reply = rpc("127.0.0.1", 15702, method, params, timeout=240)
    if "error" in reply:
        raise RuntimeError(reply["error"])
    return reply["result"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", required=True, type=int)
    parser.add_argument("--entity", default="69v0")
    args = parser.parse_args()
    resolution = raw("world.get_components", {"entity": args.entity, "components": [WINDOW]})["components"][WINDOW]["resolution"]
    width, height = resolution["physical_width"], resolution["physical_height"]
    out = ROOT / ".temp/gi97-resize"
    out.mkdir(parents=True, exist_ok=True)
    evidence = []

    def resize(w, h):
        # One atomic component update: two requests can race a native resize event
        # that writes the first request's old height back into WindowResolution.
        target = dict(resolution)
        target.update(physical_width=w, physical_height=h)
        raw("world.mutate_components", {"entity": args.entity, "component": WINDOW,
                                        "path": "resolution", "value": target})
        time.sleep(2)

    try:
        call("gi_set", {"enabled": True, "estimation": True, "scgi": True, "view": 0})
        call("set_camera", {"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 5, "radius": 5})
        call("ibl_set", {"enabled": True, "intensity": 1})
        for w, h in [(width + 1, height + 1), (width + 2, height + 1)]:
            resize(w, h)
            stats = call("scgi_stats", {"stride": 8})
            edge = call("scgi_stats", {"stride": 1, "roi": [w - 2, 0, 2, h]})
            ok = (stats["extent"] == {"width": w, "height": h}
                  and stats["coverage"] > 0.5 and edge["coverage"] > 0.5
                  and edge["non_finite"] == 0 and edge["negative"] == 0)
            evidence.append({"expected": [w, h], "pass": ok, "stats": stats, "edge": edge})
            snapshot(out, f"{w}x{h}", args.pid)
            print("PASS" if ok else "FAIL", "full extent and partial-quarter edge", w, h, flush=True)
    finally:
        resize(width, height)
        snapshot(out, "restored", args.pid)
        (out / "evidence.json").write_text(json.dumps(evidence, indent=2), encoding="utf-8")
    return 0 if evidence and all(row["pass"] for row in evidence) else 1


if __name__ == "__main__":
    raise SystemExit(main())
