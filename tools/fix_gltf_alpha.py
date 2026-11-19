"""修正 glTF 材质 alpha 语义的空转/反向配置。

判定（只读 baseColor 贴图 alpha 直方图，按文件路径缓存）：
- MASK 但贴图全不透明、factor alpha >= 1 → OPAQUE（裁切阈值永远不触发）
- OPAQUE 但贴图有 >= 5% 像素 alpha=0 → MASK + alphaCutoff 0.5（贴图在抠形）
- BLEND 但贴图全不透明、factor alpha >= 1 → OPAQUE（没有可混合的 alpha）

只改 materials[].alphaMode / alphaCutoff，不动几何、贴图与其他扩展。
写盘前把原文件备份到 .temp/gltf_alpha_backup/（已有备份不覆盖）。

用法:
  python tools/fix_gltf_alpha.py [--dry-run] <a.gltf> [b.gltf ...]
"""

import argparse
import io
import json
import os
import shutil
from collections import Counter
from urllib.parse import unquote

from PIL import Image

ZERO_FRACTION_THRESHOLD = 0.05
DEFAULT_MASK_CUTOFF = 0.5
BACKUP_DIR = os.path.join(".temp", "gltf_alpha_backup")


def texture_path(gltf, base_dir, tex_index):
    if tex_index is None:
        return None
    textures = gltf.get("textures", [])
    images = gltf.get("images", [])
    if tex_index >= len(textures):
        return None
    src = textures[tex_index].get("source")
    if src is None or src >= len(images):
        return None
    uri = images[src].get("uri")
    return os.path.join(base_dir, unquote(uri)) if uri else None


def alpha_stats(path, cache):
    if path in cache:
        return cache[path]
    try:
        alpha = Image.open(path).convert("RGBA").getchannel("A")
    except (OSError, ValueError):
        cache[path] = None
        return None
    hist = alpha.histogram()
    total = sum(hist)
    stats = {"all_opaque": hist[255] == total, "zero": hist[0] / total}
    cache[path] = stats
    return stats


def material_alpha_stats(gltf, base_dir, material, cache):
    pbr = material.get("pbrMetallicRoughness", {})
    tex = pbr.get("baseColorTexture")
    if tex is None:
        return None
    return alpha_stats(texture_path(gltf, base_dir, tex.get("index")), cache)


def decide(mode, factor_alpha, stats):
    """返回 (新 alphaMode, 理由)；不满足任何修正条件返回 None。"""
    if stats is None:
        return None
    if mode == "MASK" and factor_alpha >= 0.999 and stats["all_opaque"]:
        return "OPAQUE", "MASK 但贴图/factor 无 alpha（cutoff 永不触发）"
    if mode == "OPAQUE" and stats["zero"] >= ZERO_FRACTION_THRESHOLD:
        return "MASK", f"OPAQUE 但贴图 {stats['zero']:.0%} 像素 alpha=0（在抠形）"
    if mode == "BLEND" and factor_alpha >= 0.999 and stats["all_opaque"]:
        return "OPAQUE", "BLEND 但贴图/factor 无 alpha（无可混合内容）"
    return None


def apply_mode(material, new_mode):
    if new_mode == "OPAQUE":
        material.pop("alphaMode", None)
        material.pop("alphaCutoff", None)
    elif new_mode == "MASK":
        material["alphaMode"] = "MASK"
        material.setdefault("alphaCutoff", DEFAULT_MASK_CUTOFF)


def dump_like_blender(gltf, path):
    """按 Blender 导出风格（tab 缩进、冒号后无空格）写回，保留原行尾。"""
    with open(path, "rb") as f:
        raw = f.read(4096)
    newline = "\r\n" if b"\r\n" in raw else "\n"
    buf = io.StringIO()
    json.dump(gltf, buf, ensure_ascii=False, indent="\t", separators=(",", ":"))
    text = buf.getvalue() + "\n"
    if newline != "\n":
        text = text.replace("\n", newline)
    with open(path, "wb") as f:
        f.write(text.encode("utf-8"))


def fix_file(path, dry_run):
    with open(path, "rb") as f:
        gltf = json.load(f)
    base_dir = os.path.dirname(path) or "."
    usage = Counter()
    for mesh in gltf.get("meshes", []):
        for prim in mesh.get("primitives", []):
            material = prim.get("material")
            if material is not None:
                usage[material] += 1

    cache = {}
    changes = []
    for index, material in enumerate(gltf.get("materials", [])):
        pbr = material.get("pbrMetallicRoughness", {})
        factor = pbr.get("baseColorFactor") or [1.0, 1.0, 1.0, 1.0]
        factor_alpha = factor[3]
        mode = material.get("alphaMode", "OPAQUE")
        stats = material_alpha_stats(gltf, base_dir, material, cache)
        decision = decide(mode, factor_alpha, stats)
        if decision is None:
            continue
        new_mode, reason = decision
        changes.append(
            (index, material.get("name", ""), mode, new_mode, reason, usage.get(index, 0))
        )
        if not dry_run:
            apply_mode(material, new_mode)

    if changes and not dry_run:
        os.makedirs(BACKUP_DIR, exist_ok=True)
        backup = os.path.join(BACKUP_DIR, os.path.basename(path))
        if not os.path.exists(backup):
            shutil.copy2(path, backup)
        dump_like_blender(gltf, path)

    print(
        f"{'[dry-run] ' if dry_run else ''}{os.path.basename(path)}: {len(changes)} 处修正"
        f"（材质 {len(gltf.get('materials', []))}，prim {sum(usage.values())}）"
    )
    for index, name, old, new, reason, prims in changes:
        print(f"  #{index:3d} {old:5s} -> {new:6s} prims={prims:2d} {name}  | {reason}")
    return len(changes)


def main():
    parser = argparse.ArgumentParser(description="修正 glTF 材质 alpha 语义")
    parser.add_argument("--dry-run", action="store_true", help="只打印，不写盘")
    parser.add_argument("gltf", nargs="+", help="要修正的 .gltf 文件")
    args = parser.parse_args()
    total = 0
    for path in args.gltf:
        total += fix_file(path, args.dry_run)
    print(f"合计 {total} 处")


if __name__ == "__main__":
    main()
