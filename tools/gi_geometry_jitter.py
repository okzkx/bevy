"""SCGI patch 几何/辐照度的逐帧抖动取证（BRP，只读）。

用法：
  python tools/gi_geometry_jitter.py --pid <pid> --samples 24 --ring 16

对同一批 patch 槽位连续多帧调用 `gi_estimate_probe`，统计每个槽位的：
- 世界位置漂移（米）：帧间位移与到首帧的距离
- 法线夹角（度）：与首帧法线、以及帧间最大夹角
- 缓存 E（l0 + l1·n，在**首帧法线**下求值）的相对波动
- 该槽位的 update_count / rank

用途：区分"暗区闪烁"是估计噪声（E 抖而几何稳）还是 patch 身份抖动
（几何每帧被不同播种像素改写 → 求值方向变 → E 跟着变）。
只读，不改设置，不做视觉判定。
"""

import argparse
import json
import math
import sys
from pathlib import Path

sys.dont_write_bytecode = True

sys.path.insert(0, str(Path(__file__).resolve().parent))

from gi_scene_regression import call, ROOT  # noqa: E402


def angle_deg(a, b):
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(x * x for x in b))
    if na <= 0.0 or nb <= 0.0:
        return None
    c = max(-1.0, min(1.0, dot / (na * nb)))
    return math.degrees(math.acos(c))


def dist(a, b):
    return math.sqrt(sum((x - y) ** 2 for x, y in zip(a, b)))


def evaluate(irradiance, normal):
    if not irradiance:
        return None
    return max(
        irradiance["l0"] + sum(irradiance["l1"][i] * normal[i] for i in range(3)),
        0.0,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", required=True, type=int)
    parser.add_argument("--samples", type=int, default=24)
    parser.add_argument("--ring", type=int, default=16, help="每次探针取多少条 patch")
    parser.add_argument("--out", default=str(ROOT / ".temp/gi97-geometry-jitter.json"))
    args = parser.parse_args()

    gi = call("gi_stats")["settings"]
    baseline = {
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
    call("ibl_set", {"enabled": True, "intensity": 1.0})
    call("gi_set", baseline)
    call("set_camera", {"target": [0, 3, 0], "yaw_deg": 90, "pitch_deg": 5, "radius": 5})
    print(json.dumps({"camera": call("set_camera", {"zoom": 1})["orbit"],
                      "sun": call("sun_get"),
                      "ibl": call("ibl_stats")["settings"]}, ensure_ascii=False), flush=True)

    # 预热：让 patch 池稳定、收敛计数拉起来（排空 churn 后的初值）
    for _ in range(40):
        call("gi_stats")

    series = []
    for step in range(args.samples):
        probe = call("gi_estimate_probe", {"ring": args.ring})
        rows = {}
        for entry in probe.get("ring", []):
            slot = entry.get("slot")
            if slot is None:
                continue
            rows[slot] = {
                "position": entry["geometry"]["position"],
                "normal": entry["geometry"]["normal"],
                "temporal": entry.get("irradiance_temporal", {}).get("evaluated"),
                "temporal_l0": entry.get("irradiance_temporal", {}).get("l0"),
                "update_count": entry.get("statistics", {}).get("update_count"),
                "state": entry.get("statistics", {}).get("state"),
                "frame": probe.get("frame_index"),
            }
        series.append({"frame": probe.get("frame_index"), "slots": rows})
        print(f"probe {step + 1}/{args.samples} frame={probe.get('frame_index')} slots={len(rows)}",
              flush=True)

    # 只保留全程都在的槽位
    common = set(series[0]["slots"])
    for item in series[1:]:
        common &= set(item["slots"])
    common = sorted(common)

    report = {"slots": {}, "common_slot_count": len(common)}
    for slot in common:
        first = series[0]["slots"][slot]
        n0 = first["normal"]
        p0 = first["position"]
        normals = [item["slots"][slot]["normal"] for item in series]
        positions = [item["slots"][slot]["position"] for item in series]
        # 每帧都在首帧法线下求值 → 只反映辐照度本身的变化
        evals = []
        for item in series:
            row = item["slots"][slot]
            e = evaluate(row["temporal_l0"], n0) if row["temporal_l0"] else None
            evals.append(e)
        evals_ok = [e for e in evals if e is not None]
        angles = [angle_deg(n, n0) for n in normals]
        angles_ok = [a for a in angles if a is not None]
        drift = [dist(p, p0) for p in positions]
        pair_angles = [
            angle_deg(normals[i], normals[i + 1]) for i in range(len(normals) - 1)
        ]
        pair_ok = [a for a in pair_angles if a is not None]
        mean_e = sum(evals_ok) / len(evals_ok) if evals_ok else None
        report["slots"][str(slot)] = {
            "normal_angle_from_first_deg": {
                "max": max(angles_ok) if angles_ok else None,
                "mean": sum(angles_ok) / len(angles_ok) if angles_ok else None,
            },
            "consecutive_normal_angle_deg": {
                "max": max(pair_ok) if pair_ok else None,
                "mean": sum(pair_ok) / len(pair_ok) if pair_ok else None,
            },
            "position_drift_m": {"max": max(drift) if drift else None,
                                 "mean": sum(drift) / len(drift) if drift else None},
            "temporal_e": {
                "mean": mean_e,
                "min": min(evals_ok) if evals_ok else None,
                "max": max(evals_ok) if evals_ok else None,
                "rel_range": ((max(evals_ok) - min(evals_ok)) / mean_e)
                if evals_ok and mean_e and mean_e > 1e-6 else None,
            },
            "update_count": [item["slots"][slot]["update_count"] for item in series[:3]],
        }

    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text(json.dumps(report, indent=1), encoding="utf-8")

    angles_all = [v["consecutive_normal_angle_deg"]["max"] for v in report["slots"].values()
                  if v["consecutive_normal_angle_deg"]["max"] is not None]
    rel_all = [v["temporal_e"]["rel_range"] for v in report["slots"].values()
               if v["temporal_e"]["rel_range"] is not None]
    print(json.dumps({
        "common_slots": len(common),
        "consecutive_normal_angle_deg": {
            "p50": sorted(angles_all)[len(angles_all) // 2] if angles_all else None,
            "max": max(angles_all) if angles_all else None,
        },
        "temporal_e_rel_range": {
            "p50": sorted(rel_all)[len(rel_all) // 2] if rel_all else None,
            "max": max(rel_all) if rel_all else None,
        },
        "out": args.out,
    }, ensure_ascii=False, indent=1), flush=True)


if __name__ == "__main__":
    main()
