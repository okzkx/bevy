"""Offline SCGI flicker map from retained stationary frame_dump PPM sequences.

Usage:
  python tools/gi_flicker_map.py --run high=.temp/gi97-fixed-guide-high \
      --run low=.temp/gi97-fixed-guide-low --out .temp/gi97-flicker-map

Each run directory holds <case>/frame*.ppm written by tools/gi_stage_audit.py
(linear sRGB UNORM8 readback, stationary camera and sun). The "baseline" case is
analysed; "ibl_only" (if present) is the static control and the "toward IBL"
reference for alpha-dip detection. Display-domain values use the same sRGB OETF
as tools/gi_scene_regression.display_srgb8 (applied through an exact 256-entry
LUT built from it). Writes heat maps, overlays, hotspot crops and stats.json.
No BRP, no GPU. The statistics are diagnostic evidence, not a visual verdict.
"""

import argparse
import hashlib
import json
import sys
from pathlib import Path

sys.dont_write_bytecode = True

import numpy as np
from PIL import Image, ImageDraw

from gi_scene_regression import display_srgb8

LUMA = np.array([0.2126, 0.7152, 0.0722], dtype=np.float32)
# display_srgb8 is elementwise on UNORM8 codes, so a LUT reproduces it exactly.
DISPLAY_LUT = display_srgb8(np.arange(256, dtype=np.uint8)).astype(np.float32)
INFERNO = np.array([[0, 0, 4], [40, 11, 84], [101, 21, 110], [159, 42, 99],
                    [212, 72, 66], [245, 125, 21], [250, 193, 39], [252, 255, 164]],
                   dtype=np.float32)
STD_VMAX = 8.0
RANGE_VMAX = 32.0
MAD_VMAX = 8.0
BLOCK = 4


def rnd(value, digits=5):
    if isinstance(value, dict):
        return {key: rnd(item, digits) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [rnd(item, digits) for item in value]
    if isinstance(value, (float, np.floating)):
        value = float(value)
        return value if not np.isfinite(value) or value == 0.0 else float(f"{value:.{digits}g}")
    if isinstance(value, np.integer):
        return int(value)
    return value


def load_case(case_dir, limit):
    paths = sorted(case_dir.glob("frame*.ppm"))
    if limit:
        paths = paths[:limit]
    if len(paths) < 2:
        raise SystemExit(f"{case_dir}: need at least two frame*.ppm, found {len(paths)}")
    frames = []
    for path in paths:
        with Image.open(path) as image:
            frames.append(np.asarray(image.convert("RGB"), dtype=np.uint8))
    return paths, np.stack(frames)


def display_luma(frame):
    return DISPLAY_LUT[frame] @ LUMA


def linear_luma(frame):
    return frame.astype(np.float32) @ LUMA


def colorize(values, vmax):
    t = np.clip(np.asarray(values, dtype=np.float32) / vmax, 0.0, 1.0) * (len(INFERNO) - 1)
    index = np.minimum(t.astype(np.int32), len(INFERNO) - 2)
    frac = (t - index)[..., None]
    return (INFERNO[index] * (1.0 - frac) + INFERNO[index + 1] * frac).astype(np.uint8)


def diverging(values, vmax):
    v = np.clip(np.asarray(values, dtype=np.float32) / vmax, -1.0, 1.0)
    pos, neg = np.clip(v, 0, 1), np.clip(-v, 0, 1)
    rgb = np.stack([pos, 0.35 * (pos + neg), neg], axis=-1)
    return (np.sqrt(rgb) * 255.0).astype(np.uint8)


def with_colorbar(rgb, vmax, label):
    image = Image.fromarray(rgb)
    width = image.width
    bar = np.tile(colorize(np.linspace(0, vmax, width), vmax)[None, :, :], (14, 1, 1))
    canvas = Image.new("RGB", (width, image.height + 34), (0, 0, 0))
    canvas.paste(image, (0, 0))
    canvas.paste(Image.fromarray(bar), (0, image.height + 2))
    draw = ImageDraw.Draw(canvas)
    for frac in (0.0, 0.25, 0.5, 0.75, 1.0):
        x = min(int(frac * (width - 1)), width - 60)
        draw.text((x, image.height + 18), f"{frac * vmax:g}", fill=(255, 255, 255))
    draw.text((width // 2 - 160, image.height + 18), label, fill=(200, 200, 200))
    return canvas


def overlay(base_rgb, values, vmax):
    gray = (base_rgb.astype(np.float32) @ LUMA)[..., None] * 0.55
    alpha = (np.clip(values / vmax, 0.0, 1.0) ** 0.6)[..., None]
    out = gray * (1.0 - alpha) + colorize(values, vmax).astype(np.float32) * alpha
    return np.clip(out, 0, 255).astype(np.uint8)


def quantiles(values, qs=(50, 75, 90, 95, 99, 99.9)):
    values = np.asarray(values, dtype=np.float64).ravel()
    row = {f"p{q:g}": float(np.percentile(values, q)) for q in qs}
    row.update(mean=float(values.mean()), max=float(values.max()))
    return row


def binned(key, edges, std):
    total = float((std.astype(np.float64) ** 2).sum())
    rows = []
    for lo, hi in zip(edges[:-1], edges[1:]):
        mask = (key >= lo) & (key < hi)
        count = int(mask.sum())
        row = {"range": [lo, hi], "pixel_fraction": count / std.size}
        if count:
            s = std[mask].astype(np.float64)
            row.update(mean_std=s.mean(), p99_std=np.percentile(s, 99),
                       frac_std_ge_1=(s >= 1.0).mean(), frac_std_ge_2=(s >= 2.0).mean(),
                       variance_share=(s ** 2).sum() / total)
        rows.append(row)
    return rows


def box_blur(a, r):
    k = 2 * r + 1
    padded = np.pad(a.astype(np.float64), r, mode="edge")
    c = np.pad(padded.cumsum(0).cumsum(1), ((1, 0), (1, 0)))
    s = c[k:, k:] - c[:-k, k:] - c[k:, :-k] + c[:-k, :-k]
    return s / (k * k)


def sobel(a):
    p = np.pad(a.astype(np.float32), 1, mode="edge")
    gx = (p[:-2, 2:] + 2 * p[1:-1, 2:] + p[2:, 2:]) - (p[:-2, :-2] + 2 * p[1:-1, :-2] + p[2:, :-2])
    gy = (p[2:, :-2] + 2 * p[2:, 1:-1] + p[2:, 2:]) - (p[:-2, :-2] + 2 * p[:-2, 1:-1] + p[:-2, 2:])
    return np.hypot(gx, gy)


def spatial_autocorr(d, max_lag=64):
    count, height, width = d.shape
    shape = (height + max_lag + 1, width + max_lag + 1)
    power = None
    for t in range(count):
        spectrum = np.fft.rfft2(d[t].astype(np.float64), s=shape)
        p = spectrum.real ** 2 + spectrum.imag ** 2
        power = p if power is None else power + p
    ac = np.fft.irfft2(power, s=shape)
    lags = np.arange(max_lag + 1)
    pos = ac[:max_lag + 1, :max_lag + 1] / ((height - lags)[:, None] * (width - lags)[None, :])
    neg = ac[:max_lag + 1, [0] + [shape[1] - k for k in lags[1:]]]
    neg = neg / ((height - lags)[:, None] * (width - lags)[None, :])
    rho_pos, rho_neg = pos / pos[0, 0], neg / pos[0, 0]
    radius = np.rint(np.hypot(lags[:, None], lags[None, :])).astype(int)
    radial = []
    for r in range(max_lag + 1):
        m = radius == r
        radial.append(float(np.concatenate([rho_pos[m], rho_neg[m]]).mean()))

    def first_below(threshold):
        return next((r for r, v in enumerate(radial) if v < threshold), None)

    probe = [1, 2, 3, 4, 5, 6, 8, 12, 16, 24, 32, 48, 64]
    return {"rho_x": {str(k): rho_pos[0, k] for k in probe},
            "rho_y": {str(k): rho_pos[k, 0] for k in probe},
            "rho_radial": {str(k): radial[k] for k in probe},
            "corr_length_px_rho_lt_0.5": first_below(0.5),
            "corr_length_px_rho_lt_1/e": first_below(1.0 / np.e),
            "corr_length_px_rho_lt_0.1": first_below(0.1)}


def phase_jumps(d, mask, period):
    count, height, width = d.shape
    mx = (mask[:, 1:] & mask[:, :-1]).astype(np.float32)
    my = (mask[1:, :] & mask[:-1, :]).astype(np.float32)
    sx, sy = np.zeros(width - 1), np.zeros(height - 1)
    for t in range(count):
        sx += (np.abs(d[t, :, 1:] - d[t, :, :-1]) * mx).sum(axis=0)
        sy += (np.abs(d[t, 1:, :] - d[t, :-1, :]) * my).sum(axis=1)
    px, py = np.arange(width - 1) % period, np.arange(height - 1) % period
    jx = np.bincount(px, sx, period) / np.maximum(np.bincount(px, mx.sum(0) * count, period), 1)
    jy = np.bincount(py, sy, period) / np.maximum(np.bincount(py, my.sum(1) * count, period), 1)
    # Phase period-1 is the pair (period*k + period-1, period*(k+1)): a block boundary.
    return {"mean_abs_jump_x_by_phase": jx.tolist(), "mean_abs_jump_y_by_phase": jy.tolist(),
            "boundary_ratio_x": jx[-1] / jx[:-1].mean(), "boundary_ratio_y": jy[-1] / jy[:-1].mean()}


def block_series(lum, std, b):
    count, height, width = lum.shape
    hb, wb = height // b, width // b
    series = lum[:, :hb * b, :wb * b].reshape(count, hb, b, wb, b).mean(axis=(2, 4))
    return series.std(axis=0), std[:hb * b, :wb * b].reshape(hb, b, wb, b).mean(axis=(1, 3))


def label_components(mask):
    height, width = mask.shape
    sentinel = height * width
    labels = np.where(mask, np.arange(sentinel).reshape(height, width), sentinel)
    while True:
        nxt = labels.copy()
        nxt[1:] = np.minimum(nxt[1:], labels[:-1])
        nxt[:-1] = np.minimum(nxt[:-1], labels[1:])
        nxt[:, 1:] = np.minimum(nxt[:, 1:], labels[:, :-1])
        nxt[:, :-1] = np.minimum(nxt[:, :-1], labels[:, 1:])
        nxt = np.where(mask, nxt, sentinel)
        if np.array_equal(nxt, labels):
            return labels, sentinel
        labels = nxt


def zoom(rgb, factor):
    return np.repeat(np.repeat(rgb, factor, axis=0), factor, axis=1)


def grid(rgb, factor, color=(90, 90, 90)):
    out = rgb.copy()
    step = BLOCK * factor
    out[::step, :, :] = color
    out[:, ::step, :] = color
    return out


def analyse_run(name, run_dir, out, limit):
    paths, frames = load_case(run_dir / "baseline", limit)
    count, height, width, _ = frames.shape
    mtimes = np.array([path.stat().st_mtime for path in paths])
    dup = np.array([np.array_equal(frames[i], frames[i - 1]) for i in range(1, count)])
    keep = np.concatenate([[0], np.nonzero(~dup)[0] + 1])
    digests = {hashlib.blake2b(frame.tobytes(), digest_size=16).hexdigest() for frame in frames}
    print(f"[{name}] {count} frames {width}x{height}; consecutive duplicates {int(dup.sum())}; "
          f"distinct {len(digests)}", flush=True)

    # Stage-audit compatible numbers (all frames, channel-mean luminance) for cross-checking.
    s1 = np.zeros((height, width)); s2 = np.zeros((height, width))
    l1 = np.zeros((height, width)); l2 = np.zeros((height, width))
    for frame in frames:
        x = DISPLAY_LUT[frame].mean(axis=2)
        s1 += x; s2 += x * x
        y = frame.astype(np.float64).mean(axis=2)
        l1 += y; l2 += y * y
    compat = {"display_mean_temporal_std": np.sqrt(np.maximum(s2 / count - (s1 / count) ** 2, 0)).mean(),
              "linear_mean_temporal_std": np.sqrt(np.maximum(l2 / count - (l1 / count) ** 2, 0)).mean()}
    del s1, s2, l1, l2

    kept = frames[keep]
    n = len(kept)
    lum = np.stack([display_luma(frame) for frame in kept])
    mean = lum.mean(axis=0)
    d = lum - mean
    std = np.sqrt((d * d).mean(axis=0))
    rng = lum.max(axis=0) - lum.min(axis=0)
    mad = np.abs(np.diff(lum, axis=0)).mean(axis=0)
    lin_mean = np.stack([linear_luma(frame) for frame in kept]).mean(axis=0)
    code_range = (kept.max(axis=0).astype(np.int16) - kept.min(axis=0).astype(np.int16)).max(axis=2)
    last_display = DISPLAY_LUT[frames[-1]].astype(np.uint8)
    np.save(out / f"{name}_std.npy", std.astype(np.float32))

    ibl = None
    ibl_dir = run_dir / "ibl_only"
    if ibl_dir.is_dir():
        _, ibl_frames = load_case(ibl_dir, limit)
        ibl_lum = np.stack([display_luma(frame) for frame in ibl_frames])
        ibl = {"mean": ibl_lum.mean(axis=0), "std": ibl_lum.std(axis=0),
               "distinct": len({hashlib.blake2b(f.tobytes(), digest_size=16).hexdigest()
                                for f in ibl_frames}), "frames": len(ibl_frames)}
        del ibl_lum, ibl_frames

    report = {"frames_total": count, "frames_used_after_consecutive_dedup": n,
              "consecutive_duplicate_indices": (np.nonzero(dup)[0] + 1).tolist(),
              "distinct_frames": len(digests),
              "dump_interval_ms": quantiles(np.diff(mtimes) * 1000.0, (0, 50, 100)),
              "compat_all_frames_channel_mean": compat,
              "display_std": quantiles(std), "display_range": quantiles(rng),
              "display_mean_abs_frame_diff": quantiles(mad),
              "fraction_std_ge": {str(t): float((std >= t).mean()) for t in (0.5, 1, 2, 4, 8)},
              "fraction_range_ge": {str(t): float((rng >= t).mean()) for t in (4, 8, 16, 32)}}

    report["by_display_mean_luma"] = binned(mean, [0, 8, 16, 32, 48, 64, 96, 128, 192, 256], std)
    report["by_linear_mean_code"] = binned(lin_mean, [0, 1, 2, 4, 8, 16, 32, 64, 256], std)
    visible = std >= 1.0
    lsb_steps = {str(c): float(DISPLAY_LUT[c + 1] - DISPLAY_LUT[c]) for c in range(0, 16)}
    report["linear_quantization"] = {
        "display_levels_per_linear_lsb_at_code": lsb_steps,
        "visible_flicker_pixels_std_ge_1": int(visible.sum()),
        "visible_max_channel_code_range_hist": {str(k): float((np.minimum(code_range[visible], 8) == k).mean())
                                                for k in range(0, 9)},
        "visible_linear_mean_code": quantiles(lin_mean[visible], (10, 50, 90)),
        "all_pixels_code_range_ge_1": float((code_range >= 1).mean()),
    }

    # Spatial structure of the temporal deviation.
    report["spatial_autocorr"] = spatial_autocorr(d)
    total_var = float((d.astype(np.float64) ** 2).sum())
    lowpass = {}
    for r in (1, 2, 4, 8, 16, 32):
        lowpass[str(2 * r + 1)] = sum(float((box_blur(d[t], r) ** 2).sum()) for t in range(n)) / total_var
    report["lowpass_variance_fraction_by_box_px"] = lowpass
    global_dev = d.reshape(n, -1).mean(axis=1)
    report["global_mode"] = {"frame_mean_dev_std": float(global_dev.std()),
                             "variance_share": float((global_dev ** 2).sum() * height * width / total_var),
                             "series": global_dev.tolist()}
    hot_px = std >= 1.0
    report["block_phase_jumps_4"] = phase_jumps(d, hot_px, 4)
    report["block_phase_jumps_8"] = phase_jumps(d, hot_px, 8)
    coherence = {}
    block_maps = {}
    for b in (4, 8, 16, 32):
        series_std, pix_std = block_series(lum, std, b)
        block_maps[b] = (series_std, pix_std)
        m = pix_std >= 1.0
        ratio = series_std[m] / pix_std[m]
        coherence[str(b)] = {"blocks_with_mean_pixel_std_ge_1": int(m.sum()),
                             "coherence_median": float(np.median(ratio)) if m.any() else None,
                             "coherence_p10": float(np.percentile(ratio, 10)) if m.any() else None,
                             "coherence_p90": float(np.percentile(ratio, 90)) if m.any() else None,
                             "independent_noise_reference": 1.0 / b}
    report["block_coherence"] = coherence

    # Temporal structure on the hottest pixels.
    flat_std = std.ravel()
    k = int(min(60000, max(1000, (flat_std >= 0.5).sum())))
    index = np.argpartition(flat_std, -k)[-k:]
    x = d.reshape(n, -1)[:, index].astype(np.float64)
    z = x / np.maximum(x.std(axis=0), 1e-9)
    autocorr = {str(lag): float((z[:-lag] * z[lag:]).mean()) for lag in range(1, min(17, n))}
    power = (np.abs(np.fft.rfft(z, axis=0)) ** 2).mean(axis=1)[1:]
    freqs = np.fft.rfftfreq(n)[1:]
    share = power / power.sum()
    top = np.argsort(share)[::-1][:6]
    corr = np.corrcoef(x)
    off = corr[~np.eye(n, dtype=bool)].reshape(n, n - 1)
    by_lag = {str(lag): float(np.mean(np.diagonal(corr, lag))) for lag in range(1, min(13, n))}
    pairs = sorted(((corr[i, j], i, j) for i in range(n) for j in range(i + 1, n)), reverse=True)[:12]
    report["temporal_hot_pixels"] = {
        "pixels": k, "min_std_of_selection": float(flat_std[index].min()),
        "autocorr_by_dump_lag": autocorr,
        "spectrum_top_bins": [{"cycles_per_dump": float(freqs[i]), "period_dumps": float(1 / freqs[i]),
                               "power_share": float(share[i])} for i in top],
        "white_noise_share_per_bin": 1.0 / len(share),
        "spectral_flatness": float(np.exp(np.mean(np.log(power))) / np.mean(power)),
        "excess_kurtosis": quantiles((z ** 4).mean(axis=0) - 3.0, (10, 50, 90)),
        "skewness": quantiles((z ** 3).mean(axis=0), (10, 50, 90)),
        "fraction_abs_z_gt_3": float((np.abs(z) > 3).mean()),
        "gaussian_fraction_abs_z_gt_3": 0.0027,
        "frame_similarity": {"mean_offdiag": float(off.mean()), "by_lag": by_lag,
                             "row_max_median": float(np.median(off.max(axis=1))),
                             "row_max_min": float(off.max(axis=1).min()),
                             "top_pairs": [{"rho": float(c), "i": int(keep[i]), "j": int(keep[j]),
                                            "lag_in_used_frames": int(j - i)} for c, i, j in pairs]},
    }
    sim = ((np.clip(corr, -1, 1) + 1) * 0.5)
    Image.fromarray(zoom(colorize(sim, 1.0), 8)).save(out / f"{name}_frame_similarity.png")
    if ibl is not None:
        toward = np.sign(ibl["mean"].ravel()[index] - mean.ravel()[index])
        extreme = np.abs(z) > 2.5
        report["toward_ibl"] = {
            "ibl_control_distinct_frames": ibl["distinct"], "ibl_control_frames": ibl["frames"],
            "ibl_control_display_std": quantiles(ibl["std"], (50, 99)),
            "hot_pixels_ibl_brighter_fraction": float((toward > 0).mean()),
            "extreme_samples": int(extreme.sum()),
            "extreme_toward_ibl_fraction": float((np.sign(z) == toward)[extreme].mean()),
            "extreme_brighter_fraction": float((z > 0)[extreme].mean()),
            "max_excursion_over_ibl_gap": quantiles(np.abs(x).max(axis=0) /
                                                    np.maximum(np.abs(ibl["mean"].ravel()[index] -
                                                                      mean.ravel()[index]), 0.5),
                                                    (10, 50, 90)),
            "base_over_ibl_display_mean_hot": quantiles(mean.ravel()[index] /
                                                        np.maximum(ibl["mean"].ravel()[index], 1), (10, 50, 90)),
        }
        edge_source = ibl["mean"]
    else:
        edge_source = mean
    grad = sobel(edge_source)
    edge = grad >= np.percentile(grad, 90)
    hot = std >= np.percentile(std, 99)
    report["edge_proxy"] = {"edge_definition": "Sobel(ibl_only mean display luma) >= p90",
                            "hot_definition": "display std >= p99",
                            "hot_on_edge_fraction": float(edge[hot].mean()),
                            "enrichment_vs_10pct": float(edge[hot].mean() / 0.1),
                            "mean_std_edge": float(std[edge].mean()), "mean_std_non_edge": float(std[~edge].mean()),
                            "variance_share_edge": float((std[edge] ** 2).sum() / (std ** 2).sum())}

    # Hotspot components on the 4x4 gather-block grid.
    series_std, pix_std = block_maps[4]
    block_hot = pix_std >= 2.0
    labels, sentinel = label_components(block_hot)
    comps = []
    for label in np.unique(labels[labels != sentinel]):
        ys, xs = np.nonzero(labels == label)
        weight = (pix_std[ys, xs] ** 2).sum() * BLOCK * BLOCK
        best = np.argmax(pix_std[ys, xs])
        comps.append({"blocks": int(len(ys)), "variance_sum": float(weight),
                      "bbox_px": [int(xs.min() * BLOCK), int(ys.min() * BLOCK),
                                  int((xs.max() + 1) * BLOCK), int((ys.max() + 1) * BLOCK)],
                      "peak_px": [int(xs[best] * BLOCK + 2), int(ys[best] * BLOCK + 2)],
                      "peak_block_std": float(pix_std[ys[best], xs[best]])})
    comps.sort(key=lambda c: c["variance_sum"], reverse=True)
    sizes = np.array([c["blocks"] for c in comps]) if comps else np.zeros(1)
    report["hotspots_block4_std_ge_2"] = {
        "components": len(comps), "blocks_hot": int(block_hot.sum()),
        "variance_share_in_hot_blocks": float((pix_std[block_hot] ** 2).sum() * 16 / (std ** 2).sum()),
        "size_blocks": quantiles(sizes, (50, 90, 99)), "top": comps[:10]}

    # Images.
    with_colorbar(colorize(std, STD_VMAX), STD_VMAX, f"{name}: display luma temporal std (0..255)").save(
        out / f"{name}_std.png")
    with_colorbar(overlay(last_display, std, STD_VMAX), STD_VMAX, f"{name}: std over last frame").save(
        out / f"{name}_std_overlay.png")
    with_colorbar(colorize(rng, RANGE_VMAX), RANGE_VMAX, f"{name}: display luma max-min").save(
        out / f"{name}_range.png")
    with_colorbar(colorize(mad, MAD_VMAX), MAD_VMAX, f"{name}: mean |frame diff|").save(out / f"{name}_mad.png")
    with_colorbar(colorize(zoom(series_std, BLOCK), STD_VMAX), STD_VMAX,
                  f"{name}: std of 4x4 block mean").save(out / f"{name}_block4_std.png")
    base = ((last_display.astype(np.float32) @ LUMA) * 0.45).astype(np.uint8)
    klass = np.stack([base] * 3, axis=-1)
    colors = {1: (0, 220, 255), 2: (255, 220, 0), 3: (255, 40, 40)}
    for value, color in colors.items():
        m = visible & ((code_range == value) if value < 3 else (code_range >= 3))
        klass[m] = color
    Image.fromarray(klass).save(out / f"{name}_code_range_class.png")
    energy = (np.diff(lum, axis=0) ** 2).mean(axis=(1, 2))
    pair = int(np.argsort(energy)[len(energy) // 2])
    with_colorbar(colorize(np.abs(lum[pair + 1] - lum[pair]), 16.0), 16.0,
                  f"{name}: |diff| used frames {pair}->{pair + 1}").save(out / f"{name}_diff_example.png")
    rows = []
    for comp in comps[:3]:
        cx, cy = comp["peak_px"]
        x0 = int(np.clip(cx - 48, 0, width - 96)); y0 = int(np.clip(cy - 48, 0, height - 96))
        window = (slice(y0, y0 + 96), slice(x0, x0 + 96))
        dev = d[:, window[0], window[1]]
        t = int(np.argmax((dev ** 2).mean(axis=(1, 2))))
        panels = [zoom(last_display[window], 4), grid(zoom(colorize(std[window], STD_VMAX), 4), 4),
                  grid(zoom(diverging(dev[t], STD_VMAX), 4), 4)]
        rows.append(np.concatenate(panels, axis=1))
        comp["crop_origin_px"] = [x0, y0]
    if rows:
        montage = Image.fromarray(np.concatenate(rows, axis=0))
        draw = ImageDraw.Draw(montage)
        for i, comp in enumerate(comps[:3]):
            draw.text((4, i * 384 + 4), f"crop @{comp['crop_origin_px']} 96px x4 | std | worst dev (4px grid)",
                      fill=(255, 255, 255))
        montage.save(out / f"{name}_hotspots.png")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--run", action="append", required=True, help="name=run_dir (repeatable)")
    parser.add_argument("--out", default=".temp/gi97-flicker-map")
    parser.add_argument("--frames", type=int, default=0, help="limit frames per case (0 = all)")
    args = parser.parse_args()
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    result = {"tool": "tools/gi_flicker_map.py", "domain": "display sRGB luma (Rec.709 weights), 0..255",
              "runs": {}}
    for spec in args.run:
        name, _, directory = spec.partition("=")
        result["runs"][name] = analyse_run(name, Path(directory), out, args.frames)
    (out / "stats.json").write_text(json.dumps(rnd(result), indent=1, ensure_ascii=False), encoding="utf-8")
    print(f"wrote {out / 'stats.json'}", flush=True)


if __name__ == "__main__":
    main()
