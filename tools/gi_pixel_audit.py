#!/usr/bin/env python3
"""Compare saved stage-audit pixel snapshots using their actual shading normals.

Read-only analysis of GPU evidence, not a runtime or visual acceptance test.
Each point contains its own submitted frame; different points are not one frame.
"""
import argparse
import json
import sys
from pathlib import Path

sys.dont_write_bytecode = True
import numpy as np


def evaluate(irradiance, normal):
    n = np.asarray(normal, dtype=np.float64)
    return np.maximum(np.asarray(irradiance["l0"])
                      + np.asarray(irradiance["l1x"]) * n[0]
                      + np.asarray(irradiance["l1y"]) * n[1]
                      + np.asarray(irradiance["l1z"]) * n[2], 0)


def analyze(probe):
    guide = probe.get("input") or {}
    row = {"pixel": guide.get("pixel"), "alive": probe.get("alive", False),
           "reason": probe.get("reason"), "frame": probe.get("gi_frame_index")}
    if "position" not in guide:
        return row
    row.update(position=guide["position"], flat=guide["flat_normal"],
               shading=guide["shading_normal"], no_l=guide["no_l"],
               albedo=guide["albedo"], material=guide["material"], hdr=guide["hdr"],
               screen=guide["screen_irradiance"])
    if not row["alive"]:
        return row
    geometry = probe["geometry"]
    row.update(stored_position=geometry["position"], stored_normal=geometry["normal"],
               normal_dot=float(np.dot(guide["flat_normal"], geometry["normal"])),
               position_distance=float(np.linalg.norm(np.asarray(guide["position"])
                                                      - geometry["position"])))
    n = guide["shading_normal"]
    active = probe["irradiance_b"] if probe["parity_hint"] == 1 else probe["irradiance_a"]
    for key, data in (("raw", active), ("spatial", probe["irradiance_spatial"]),
                      ("temporal", probe["irradiance_temporal"])):
        row[key + "_at_shading"] = evaluate(data, n).tolist()
        row[key + "_at_flat"] = evaluate(data, guide["flat_normal"]).tolist()
    row["raw_diagnostics"] = active.get("diag")
    row["rank"] = probe["statistics"]["rank"]
    if guide["screen_irradiance"] is not None:
        screen = np.asarray(guide["screen_irradiance"][:3])
        temporal = np.asarray(row["temporal_at_shading"])
        row["screen_temporal_luma_ratio"] = float(np.dot(screen, (0.2126, 0.7152, 0.0722))
                                                   / max(np.dot(temporal, (0.2126, 0.7152, 0.0722)), 1e-8))
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reports", type=Path, nargs="+")
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    result = {}
    for path in args.reports:
        report = json.loads(path.read_text(encoding="utf-8"))
        for variant, evidence in report.items():
            rows = [analyze(p) for p in evidence["pixel_probes"]]
            result[str(path) + "/" + variant] = rows
            print(path, variant, "alive", sum(r["alive"] for r in rows), "of", len(rows))
            for row in rows:
                brief = {k: row[k] for k in ("pixel", "alive", "position", "normal_dot",
                                             "no_l", "screen", "temporal_at_shading",
                                             "screen_temporal_luma_ratio", "raw_diagnostics") if k in row}
                print(json.dumps(brief))
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(result, indent=2, ensure_ascii=False), encoding="utf-8")


if __name__ == "__main__":
    main()
