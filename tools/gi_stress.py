#!/usr/bin/env python3
"""9.8.6 grouped stress matrix.

Cells are isolated: every cell starts its own renderer on `--port`, sets the
scene/spec/quality mode, runs three 5 s windows and exports raw `perf_samples`.
The report keeps p50/p95/p99 per window and the condition sheet that makes the
numbers reproducible.

Usage:
    python tools/gi_stress.py                 # full matrix
    python tools/gi_stress.py --only S1-off  # one cell (debug)
    python tools/gi_stress.py --list          # print cell ids
"""
from __future__ import annotations

import sys

sys.dont_write_bytecode = True

import argparse
import hashlib
import json
import os
import subprocess
import time
import types
from pathlib import Path

from gi_acceptance import BASELINE_GI, Brp, DEFAULT_EXE, Host, ROOT, SPEC_KEYS

DEFAULT_OUT = ROOT / ".temp" / "gi98" / "stress"
BUDGET_MS = 16.7
SCENE_SPONZA = "sponza/glTF/Sponza.gltf"
SCENE_ZERO_DAY = "zero_day/zero_day_measure_one.glb"

MODE_GI = {
    "off": {"enabled": False},
    "low": {"samples": 1, "spatial_filter": False, "denoise_passes": 0,
            "temporal_filter": False, "scgi": True},
    "mid": {"samples": 2, "spatial_filter": True, "spatial_radius": 3.0,
            "denoise_passes": 2, "temporal_filter": False, "scgi": True},
    "high": {"samples": 4, "spatial_filter": True, "spatial_radius": 3.0,
             "denoise_passes": 2, "temporal_filter": True,
             "temporal_smoothing": 0.5, "scgi": True},
}

PERF_KEYS = ("frame_wall_ms", "cpu_total_ms", "gpu_total_ms", "gpu_gi_ms",
             "gpu_lighting_ms", "gpu_shadow_ms", "gpu_gbuffer_ms",
             "gpu_ibl_capture_ms", "gpu_ray_tracing_rays_ms",
             "gpu_ray_tracing_tlas_ms")


def cells():
    out = []
    for mode in ("off", "low", "mid", "high"):
        out.append({"id": f"S1-sponza-{mode}", "scene": SCENE_SPONZA,
                    "mode": mode, "size": 128, "cascades": 4,
                    "generate": None, "workload": None})
    for mode in ("off", "low", "mid", "high"):
        out.append({"id": f"S2-zero_day-{mode}", "scene": SCENE_ZERO_DAY,
                    "mode": mode, "size": 128, "cascades": 4,
                    "generate": None, "workload": None})
    for mode in ("off", "low", "mid", "high"):
        out.append({"id": f"S3-zero_day-512-{mode}", "scene": SCENE_ZERO_DAY,
                    "mode": mode, "size": 512, "cascades": 6,
                    "generate": 50000, "workload": None})
    for count in (10000, 50000, 100000):
        for form, rows in (("none", 0), ("few", 100), ("all", count)):
            for mode in ("off", "mid"):
                out.append({"id": f"S4-{count // 1000}k-{form}-{mode}",
                            "scene": SCENE_SPONZA, "mode": mode,
                            "size": 128, "cascades": 4,
                            "generate": count,
                            "workload": {"form": form, "rows": rows}})
    return out


def pct(values, q):
    if not values:
        return None
    ordered = sorted(values)
    pos = (len(ordered) - 1) * q / 100.0
    lo = int(pos)
    hi = min(lo + 1, len(ordered) - 1)
    frac = pos - lo
    return ordered[lo] * (1 - frac) + ordered[hi] * frac


def summarize(samples, key):
    vals = [float(s[key]) for s in samples if s.get(key) is not None]
    if not vals:
        return {"p50": None, "p95": None, "p99": None, "max": None, "n": 0}
    return {"p50": round(pct(vals, 50), 4), "p95": round(pct(vals, 95), 4),
            "p99": round(pct(vals, 99), 4), "max": round(max(vals), 4),
            "n": len(vals)}


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def git(*args):
    try:
        return subprocess.run(["git", *args], cwd=str(ROOT), capture_output=True,
                              text=True, encoding="utf-8", errors="replace",
                              check=False).stdout.strip()
    except OSError:
        return ""


def gpu_name():
    try:
        out = subprocess.run(["nvidia-smi", "--query-gpu=name,driver_version,memory.total",
                              "--format=csv,noheader"],
                             capture_output=True, text=True, timeout=10, check=False)
        return out.stdout.strip() or "unknown"
    except Exception:
        return "unknown"


class CellHost:
    def __init__(self, args, out, scene):
        self.out = out
        self.args = types.SimpleNamespace(port=args.port, exe=args.exe,
                                          scene=scene, keep=False)
        self.host = Host(self.args, out)
        self.host.ensure()
        if self.host.attached:
            raise SystemExit(f"port {args.port} is busy; kill the old renderer first")
        self.brp = self.host.brp

    def close(self):
        self.host.close()
        deadline = time.time() + 20
        while time.time() < deadline:
            if self.brp.try_call("gi_stats", timeout=2) is None:
                return
            time.sleep(0.5)
        raise SystemExit("renderer did not stop")


def run_cell(args, cell, root_out):
    out = root_out / cell["id"]
    out.mkdir(parents=True, exist_ok=True)
    print(f"\n=== {cell['id']} scene={cell['scene']} mode={cell['mode']} ===")
    host = CellHost(args, out, cell["scene"])
    brp = host.brp
    try:
        brp.call("ibl_set", {"enabled": True, "diffuse_mode": "sh_l2"})
        brp.call("exposure_set", {"mode": "manual"})
        brp.call("sun_set", {"azimuth_deg": 45.0, "elevation_deg": 55.0})
        if cell["generate"]:
            # zero_day loads asynchronously; wait until its rows are in the
            # ledger, otherwise generate_scene has no primitive to clone.
            deadline = time.time() + 90
            last_rows = -1
            while time.time() < deadline:
                rows = ((brp.try_call("perf_stats") or {}).get("scale") or {}).get(
                    "instance_rows")
                if rows and rows > 0 and rows == last_rows:
                    break
                last_rows = rows
                time.sleep(1.0)
            t0 = time.time()
            brp.call("generate_scene", {"count": cell["generate"], "columns": 250,
                                        "spacing": 2.0, "seed": 11})
            print(f"[cell] generated {cell['generate']} instances in {time.time() - t0:.1f}s")
            time.sleep(3.0)
        # Apply the GI spec while off, then the rest, then the mode overrides.
        brp.call("gi_set", {"enabled": False})
        brp.call("gi_set", {"size": cell["size"], "res": 32,
                            "cascades": cell["cascades"], "capacity": 65536})
        settings = {k: v for k, v in BASELINE_GI.items() if k not in SPEC_KEYS}
        settings.update({"enabled": cell["mode"] != "off"})
        if cell["mode"] != "off":
            settings.update(MODE_GI[cell["mode"]])
        brp.call("gi_set", settings)
        if cell["mode"] == "off":
            brp.call("gi_set", {"enabled": False})
        if cell["workload"]:
            wl = cell["workload"]
            brp.call("workload", {"enabled": wl["form"] != "none", "rows": wl["rows"]})
        time.sleep(3.0)

        condition = {
            "cell": cell,
            "git_head": git("rev-parse", "HEAD"),
            "git_branch": git("rev-parse", "--abbrev-ref", "HEAD"),
            "exe": str(Path(args.exe).resolve()),
            "exe_sha256": sha256(args.exe),
            "exe_mtime": time.strftime("%Y-%m-%dT%H:%M:%S",
                                       time.localtime(Path(args.exe).stat().st_mtime)),
            "gpu": gpu_name(), "budget_ms": BUDGET_MS,
            "extent": brp.call("scgi_stats", {"stride": 64}).get("extent"),
            "quality_mode": cell["mode"],
            "workload": cell["workload"],
            "started": time.strftime("%Y-%m-%dT%H:%M:%S"),
        }
        reps = []
        for rep in range(3):
            time.sleep(5.0)
            raw = brp.call("perf_samples", {"last": 240})
            perf = brp.call("perf_stats")
            readout = {
                "gi": brp.try_call("gi_stats"),
                "scgi": brp.try_call("scgi_stats", {"stride": 8}),
                "ibl": brp.try_call("ibl_stats"),
                "residency": brp.try_call("residency_stats"),
                "draw": brp.try_call("draw_stats"),
                "perf": perf,
            }
            samples = raw.get("samples", [])
            summary = {key: summarize(samples, key) for key in PERF_KEYS}
            rep_row = {"rep": rep + 1, "samples": len(samples),
                       "summary": summary, "readout": readout}
            reps.append(rep_row)
            with open(out / f"samples_rep{rep + 1}.json", "w", encoding="utf-8") as fh:
                json.dump(raw, fh)
            print(f"[rep {rep + 1}] n={len(samples)} "
                  f"wall p50={summary['frame_wall_ms']['p50']} "
                  f"gpu p50={summary['gpu_total_ms']['p50']} "
                  f"gi p50={summary['gpu_gi_ms']['p50']}")
        all_samples = []
        for rep in range(3):
            with open(out / f"samples_rep{rep + 1}.json", "r", encoding="utf-8") as fh:
                all_samples.extend(json.load(fh).get("samples", []))
        pooled = {key: summarize(all_samples, key) for key in PERF_KEYS}
        last = brp.call("gi_stats")
        result = {"condition": condition, "reps": reps, "pooled": pooled,
                  "samples_total": len(all_samples), "end_readout": {
                      "gi": last,
                      "ibl": brp.try_call("ibl_stats"),
                      "perf": brp.try_call("perf_stats"),
                      "draw": brp.try_call("draw_stats"),
                      "residency": brp.try_call("residency_stats"),
                  }}
        with open(out / "cell.json", "w", encoding="utf-8") as fh:
            json.dump(result, fh, indent=2)
        return result
    finally:
        host.close()


def write_report(root_out, results):
    lines = [
        "# 9.8.6 成组压测读数（自动生成）",
        "",
        f"- 生成时间：{time.strftime('%Y-%m-%d %H:%M:%S')}",
        f"- HEAD：`{results[0]['condition'].get('git_head', '-') if results else '-'}`",
        f"- exe：`{results[0]['condition'].get('exe_sha256', '-') if results else '-'}`",
        f"- GPU：{results[0]['condition'].get('gpu', '-') if results else '-'}",
        f"- 预算：{BUDGET_MS} ms；每格 3×5 s，读数取 240 帧窗口的 p50/p95/p99。",
        "",
        "| 单元 | 场景 | 模式 | 实例行 | draw | wall p50/p95/p99 | gpu p50/p95/p99 | gpu_gi p50/p95/p99 | patches | 显存 B |",
        "| --- | --- | --- | ---: | ---: | --- | --- | --- | ---: | ---: |",
    ]
    for r in results:
        c = r["condition"]
        if "quality_mode" not in c:
            continue
        p = r["pooled"]
        end = r["end_readout"]
        gi = end.get("gi") or {}
        draw = end.get("draw") or {}
        mem = (gi.get("memory") or {}).get("total_bytes")
        rows = (end.get("perf") or {}).get("scale", {}).get("instance_rows")
        lines.append(
            f"| {c['cell']['id']} | {Path(c['cell']['scene']).name} | {c['quality_mode']} "
            f"| {rows} | {draw.get('draw_calls_recorded')} "
            f"| {p['frame_wall_ms']['p50']}/{p['frame_wall_ms']['p95']}/{p['frame_wall_ms']['p99']} "
            f"| {p['gpu_total_ms']['p50']}/{p['gpu_total_ms']['p95']}/{p['gpu_total_ms']['p99']} "
            f"| {p['gpu_gi_ms']['p50']}/{p['gpu_gi_ms']['p95']}/{p['gpu_gi_ms']['p99']} "
            f"| {gi.get('patches')} | {mem} |")
    (root_out / "stress.md").write_text("\r\n".join(lines) + "\r\n", encoding="utf-8")
    with open(root_out / "stress.json", "w", encoding="utf-8") as fh:
        json.dump({"cells": results, "condition": results[0]["condition"] if results else None},
                  fh, indent=2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=15704)
    parser.add_argument("--exe", default=str(DEFAULT_EXE))
    parser.add_argument("--out", default=str(DEFAULT_OUT))
    parser.add_argument("--only", default=None, help="cell id prefix")
    parser.add_argument("--list", action="store_true")
    args = parser.parse_args()
    matrix = cells()
    if args.list:
        for cell in matrix:
            print(cell["id"])
        return 0
    if not Path(args.exe).exists():
        raise SystemExit(f"missing {args.exe}; build release first")
    todo = [c for c in matrix if not args.only or c["id"].startswith(args.only)]
    root_out = Path(args.out) / time.strftime("%Y%m%d-%H%M%S")
    root_out.mkdir(parents=True, exist_ok=True)
    print(f"[stress] {len(todo)} cells -> {root_out}")
    results = []
    for cell in todo:
        try:
            results.append(run_cell(args, cell, root_out))
        except Exception as exc:  # keep the matrix running; record the failure
            print(f"[stress] {cell['id']} FAILED: {exc}")
            results.append({"condition": {"cell": cell, "error": repr(exc)},
                            "pooled": {}, "end_readout": {}})
        write_report(root_out, [r for r in results if "pooled" in r])
    write_report(root_out, [r for r in results if "pooled" in r])
    print(f"[stress] report {root_out / 'stress.md'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
