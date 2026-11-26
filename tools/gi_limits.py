#!/usr/bin/env python3
"""9.8.7 known-limit probes and A/B matrix.

Everything runs on the x=80 test field with the acceptance baseline; each case
leaves a PNG + a JSON diff, so the 局限自检 record can cite numbers instead of
"looks fine". Limits that need an asset we do not have (transparent layer, SSR,
matching Unity scene) are code-inspected and recorded as such in the record,
not faked here.

Usage: python tools/gi_limits.py
"""
from __future__ import annotations

import sys

sys.dont_write_bytecode = True

import argparse
import json
import time
from pathlib import Path

from gi_acceptance import (DEFAULT_EXE, DEFAULT_OUT, FIELD_CAMERA, Session,
                           build_field, diff_frames)

OUT = DEFAULT_OUT / "limits"


def snap(session, name):
    return session.snapshot(name)


def ab(session, tag, apply_a, apply_b, settle=3.0):
    apply_a()
    session.settle(settle)
    a = snap(session, f"{tag}_a")
    apply_b()
    session.settle(settle)
    b = snap(session, f"{tag}_b")
    d = diff_frames(session.out / f"{tag}_a.png", session.out / f"{tag}_b.png")
    print(f"[ab] {tag}: mean={d['mean_abs']} p99={d['p99']} max={d['max']}")
    return {"a": a, "b": b, "diff": d}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=15706)
    parser.add_argument("--exe", default=str(DEFAULT_EXE))
    parser.add_argument("--scene", default="sponza/glTF/Sponza.gltf")
    parser.add_argument("--out", default=str(DEFAULT_OUT))
    args = parser.parse_args()
    session = Session(args, "limits")
    try:
        session.apply_baseline()
        shapes = build_field(session)
        session.set_camera(FIELD_CAMERA, settle=12)
        ref = snap(session, "field_ref")
        result = {"condition_sheet": session.condition_sheet(), "reference": ref,
                  "ab": {}, "leak": {}, "csm": {}, "single_camera": {}}

        result["ab"]["ibl_on_off"] = ab(
            session, "ab_ibl",
            lambda: session.brp.call("ibl_set", {"enabled": False}),
            lambda: session.brp.call("ibl_set", {"enabled": True, "diffuse_mode": "sh_l2"}))

        # Isolate the IBL diffuse representation: SCGI would otherwise mask it.
        session.set_gi({"scgi": False})
        result["ab"]["sh_l2_probe16"] = ab(
            session, "ab_diffuse",
            lambda: session.brp.call("ibl_set", {"enabled": True, "diffuse_mode": "sh_l2"}),
            lambda: session.brp.call("ibl_set", {"enabled": True, "diffuse_mode": "probe16"}))
        session.brp.call("ibl_set", {"enabled": True, "diffuse_mode": "sh_l2"})
        session.set_gi({"scgi": True})

        result["ab"]["scgi_on_off"] = ab(
            session, "ab_scgi",
            lambda: session.set_gi({"scgi": True}),
            lambda: session.set_gi({"scgi": False}))
        session.set_gi({"scgi": True})

        result["ab"]["quality_low_high"] = ab(
            session, "ab_quality",
            lambda: session.apply_baseline(samples=1, quality_extra={
                "spatial_filter": False, "denoise_passes": 0, "temporal_filter": False}),
            lambda: session.apply_baseline(samples=4, quality_extra={
                "spatial_filter": True, "denoise_passes": 2, "temporal_filter": True}),
            settle=4.0)

        # Diffuse-only: a mirror-like sphere reflects only IBL, never the scene.
        metal = session.brp.call("spawn_shape", {
            "shape": "sphere", "translation": [82, 0.8, 2], "scale": [0.8, 0.8, 0.8],
            "color": [0.9, 0.9, 0.9], "metallic": 1.0,
            "perceptual_roughness": 0.05, "name": "gi98-metal"})
        session.set_camera({"target": [82, 0.8, 2], "yaw_deg": 0, "pitch_deg": 6,
                            "radius": 3}, settle=8)
        result["diffuse_only"] = {"metal": metal, "snapshot": snap(session, "diffuse_only_metal")}
        result["emissive"] = session.brp.call("gi_stats").get("emissive_triangles")
        session.brp.world_call("world.despawn_entity", {"entity": metal["entity"]})
        session.set_camera(FIELD_CAMERA, settle=8)

        result["ab"]["spatial_on_off"] = ab(
            session, "ab_spatial",
            lambda: session.set_gi({"spatial_filter": False}),
            lambda: session.set_gi({"spatial_filter": True}))
        session.apply_baseline()

        # Thin-geometry leak: a 5 cm wall between the camera and a strong light.
        thin = session.brp.call("spawn_shape", {
            "shape": "cuboid", "translation": [80, 2, -6],
            "scale": [12, 4, 0.05], "color": [0.2, 0.2, 0.2],
            "name": "gi98-thin-wall"})
        # An emissive panel behind the thin wall: only GI rays can carry its light,
        # so any brightening of the front face is a thin-geometry leak.
        panel = session.brp.call("spawn_shape", {
            "shape": "cuboid", "translation": [80, 2, -9],
            "scale": [3, 3, 0.2], "color": [1.0, 0.9, 0.8],
            "emissive": [40.0, 20.0, 10.0], "name": "gi98-leak-emissive"})
        session.set_camera({"target": [80, 2, -5.95], "yaw_deg": 0, "pitch_deg": 0,
                            "radius": 1.5}, settle=10)
        session.set_gi({"scgi": True})
        session.settle(4)
        scgi_on = snap(session, "leak_scgi_on")
        session.set_gi({"scgi": False})
        session.settle(4)
        scgi_off = snap(session, "leak_scgi_off")
        leak_diff = diff_frames(session.out / "leak_scgi_on.png",
                                session.out / "leak_scgi_off.png")
        # Differential leak probe: remove the hidden panel and keep SCGI on. If the
        # 5 cm wall leaks, the wall front changes at the panel's screen location.
        session.set_gi({"scgi": True})
        session.settle(6)
        panel_removed = snap(session, "leak_panel_removed")
        panel_diff = diff_frames(session.out / "leak_scgi_on.png",
                                 session.out / "leak_panel_removed.png")
        result["leak"] = {"thin_wall": thin, "panel": panel, "scgi_on": scgi_on,
                          "scgi_off": scgi_off, "panel_removed": panel_removed,
                          "diff": leak_diff, "panel_removed_diff": panel_diff}
        print(f"[leak] scgi on/off mean={leak_diff['mean_abs']} p99={leak_diff['p99']}")
        print(f"[leak] panel removed mean={panel_diff['mean_abs']} p99={panel_diff['p99']} "
              f"bbox={panel_diff['changed_bbox']}")

        session.brp.world_call("world.despawn_entity", {"entity": thin["entity"]})
        session.brp.world_call("world.despawn_entity", {"entity": panel["entity"]})
        session.set_gi({"scgi": True})

        # CSM coverage: low sun, wide camera; shadows end at the cascade edge.
        session.brp.call("sun_set", {"azimuth_deg": 30.0, "elevation_deg": 8.0})
        session.set_camera({"target": [80, 1.0, 0], "yaw_deg": 20, "pitch_deg": 25,
                            "radius": 30}, settle=10)
        csm = snap(session, "csm_wide")
        session.brp.call("shadow_set", {"enabled": False})
        session.settle(3)
        csm_off = snap(session, "csm_wide_shadow_off")
        session.brp.call("shadow_set", {"enabled": True})
        session.settle(3)
        csm_diff = diff_frames(session.out / "csm_wide.png",
                               session.out / "csm_wide_shadow_off.png")
        result["csm"] = {"snapshot": csm, "shadow_off": csm_off,
                         "diff": csm_diff, "sun": session.brp.call("sun_get")}
        session.brp.call("sun_set", {"azimuth_deg": 45.0, "elevation_deg": 55.0})

        # Single camera: walk 40 m away, then return; the volume follows the camera.
        session.set_camera(FIELD_CAMERA, settle=10)
        before = session.sample()
        before_snap = snap(session, "single_before")
        session.set_camera({"target": [120, 1.5, -2], "yaw_deg": 0, "pitch_deg": 10,
                            "radius": 9}, settle=8)
        away_snap = snap(session, "single_away")
        away = session.sample()
        session.set_camera(FIELD_CAMERA, settle=12)
        back = session.sample()
        back_snap = snap(session, "single_back")
        back_diff = diff_frames(session.out / "single_before.png",
                                session.out / "single_back.png")
        result["single_camera"] = {"before": before, "away": away, "back": back,
                                   "before_snap": before_snap, "away_snap": away_snap,
                                   "back_snap": back_snap, "return_diff": back_diff}
        print(f"[single] active before={before['gi_stats']['patches']} "
              f"away={away['gi_stats']['patches']} back={back['gi_stats']['patches']} "
              f"return mean={back_diff['mean_abs']}")

        session.save("limits.json", result)
        return result
    finally:
        session.close()


if __name__ == "__main__":
    main()
