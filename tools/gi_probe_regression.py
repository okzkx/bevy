"""Check GI submitted-frame snapshots across camera motion and producer toggles.

Usage: python tools/gi_probe_regression.py
Output: .temp/gi97-probe/evidence.json. Uses only BRP, never desktop input.
"""

import json
import math
import sys
import time

sys.dont_write_bytecode = True
from gi_scene_regression import ROOT, call


def expected_cell(lookup, position):
    res = lookup["res"]
    distance = math.sqrt(sum((a - b) ** 2 for a, b in zip(position, lookup["center"])))
    for cascade in range(lookup["cascades"]):
        voxel = lookup["voxel_min"] * 2 ** cascade
        if distance < voxel * (res / 2 - 0.5):
            storage = [(math.floor(p / voxel) + res // 2) % res for p in position]
            # +Y maps to octahedral direction bucket 14 in the renderer's 4x4 grid.
            return cascade * res ** 3 * 16 + 16 * (storage[0] * res ** 2 + storage[1] * res + storage[2]) + 14
    return None


def main():
    evidence = []
    original = call("gi_stats")["settings"]
    point = [0, -0.02, 0]
    try:
        call("gi_set", {"enabled": True, "estimation": True, "scgi": True, "view": 0})
        for radius in [5, 13, 5]:
            call("set_camera", {"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 5, "radius": radius})
            found = False
            for attempt in range(16):
                probe = call("gi_estimate_probe", {"position": point, "normal": [0, 1, 0]})
                lookup = probe.get("lookup")
                if not lookup:
                    time.sleep(0.02)
                    continue
                expected = expected_cell(lookup, point)
                assert (lookup["ubo_slot"] + 1) % lookup["frame_slots"] == lookup["next_frame_slot"], probe
                assert probe.get("cell") == expected, probe
                evidence.append({"radius": radius, "attempt": attempt, "expected_cell": expected, "probe": probe})
                expected_x = radius * math.cos(math.radians(5))
                if abs(lookup["center"][0] - expected_x) < 0.001:
                    found = True
                    break
            assert found, f"no newly submitted camera snapshot for radius {radius}"
        for delay in [0.08, 0.13]:
            call("gi_set", {"enabled": False})
            time.sleep(delay)
            off = call("gi_estimate_probe", {"position": point, "normal": [0, 1, 0]})
            assert off["enabled"] is False, off
            call("gi_set", {"enabled": True})
            responses = []
            for _ in range(8):
                response = call("gi_estimate_probe", {"position": point, "normal": [0, 1, 0]})
                responses.append(response)
                if response.get("lookup"):
                    lookup = response["lookup"]
                    assert (lookup["ubo_slot"] + 1) % lookup["frame_slots"] == lookup["next_frame_slot"]
                    break
                assert response.get("reason") == "not_ready", response
            assert responses[-1].get("lookup"), responses
            evidence.append({"disabled_delay": delay, "off": off, "restart": responses})
        call("gi_set", {"estimation": False})
        time.sleep(0.08)
        seed_frame = call("gi_estimate_probe", {"position": point, "normal": [0, 1, 0]})
        assert seed_frame.get("lookup"), seed_frame
        evidence.append({"estimation_disabled_still_has_gi_submission": seed_frame})
        print("PASS submitted-slot/cell mapping, new camera snapshots, GI restart, estimation-independent readiness", flush=True)
    finally:
        call("gi_set", {key: original[key] for key in ["enabled", "estimation", "scgi", "view"]})
        call("set_camera", {"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 5, "radius": 5})
        out = ROOT / ".temp/gi97-probe"
        out.mkdir(parents=True, exist_ok=True)
        (out / "evidence.json").write_text(json.dumps(evidence, indent=2), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
