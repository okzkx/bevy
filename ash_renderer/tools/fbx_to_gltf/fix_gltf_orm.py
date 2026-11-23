"""把误挂到 KHR_materials_specular 的 ORM 图改接到 glTF 标准 metallicRoughnessTexture。

ORCA 源资产（Emerald Square / Bistro）的 Specular 贴图通道约定：
    R = Occlusion，G = Roughness，B = Metallic
与 glTF 的 ORM 打包一致。FBX->glTF 转换时 Blender 把这张图当成 Specular 颜色，
写进了 KHR_materials_specular；metallicRoughnessTexture 一直是空的，
roughness 只剩 FBX Shininess 推出来的全局常数。

规则（只动带 specularTexture 的材质）：
- 没有 metallicRoughnessTexture 时，把同一张贴图接上去（沿用 texCoord 与贴图变换）；
- metallicFactor、roughnessFactor 置 1.0，数值交给贴图；
- 删掉 KHR_materials_specular：它的 RGB 被当成 F0 颜色属于误读，
  A（specular factor）在原图里全是 255，本来就是中性值。--keep-specular 可保留。
- 不接 occlusionTexture：源图 R 通道全 0，接了会把光照乘成 0。

用法（仓库根目录）:
  python ash_renderer/tools/fbx_to_gltf/fix_gltf_orm.py [--dry-run] [--keep-specular] <a.gltf> [...]
"""

import argparse
import copy
import io
import json
import os
import shutil

BACKUP_DIR = os.path.join(".temp", "gltf_orm_backup")


def specular_texture(material):
    ext = material.get("extensions", {}).get("KHR_materials_specular", {})
    return ext.get("specularTexture")


def wire_material(material, keep_specular):
    """接好一个材质；返回 (动作, 名字)。动作: wired / already / untouched"""
    name = material.get("name", "")
    tex = specular_texture(material)
    if tex is None:
        return "untouched", name
    pbr = material.setdefault("pbrMetallicRoughness", {})
    if pbr.get("metallicRoughnessTexture") is not None:
        return "already", name
    pbr["metallicRoughnessTexture"] = copy.deepcopy(tex)
    pbr["metallicFactor"] = 1.0
    pbr["roughnessFactor"] = 1.0
    if not keep_specular:
        extensions = material["extensions"]
        del extensions["KHR_materials_specular"]
        if not extensions:
            del material["extensions"]
    return "wired", name


def prune_extension_usage(gltf):
    """没有材质再用 KHR_materials_specular 时，把它从 extensionsUsed/Required 摘掉。"""
    if any(
        "KHR_materials_specular" in material.get("extensions", {})
        for material in gltf.get("materials", [])
    ):
        return False
    for key in ("extensionsUsed", "extensionsRequired"):
        if key not in gltf:
            continue
        gltf[key] = [name for name in gltf[key] if name != "KHR_materials_specular"]
        if not gltf[key]:
            del gltf[key]
    return True


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


def fix_file(path, dry_run, keep_specular, verbose):
    with open(path, "rb") as f:
        gltf = json.load(f)
    textures = gltf.get("textures", [])
    counts = {"wired": [], "already": [], "untouched": [], "bad_index": []}
    for material in gltf.get("materials", []):
        tex = specular_texture(material)
        action, name = wire_material(material, keep_specular)
        counts[action].append(name)
        if action == "wired" and tex.get("index", -1) >= len(textures):
            counts["bad_index"].append(name)

    pruned = False
    if counts["wired"] and not dry_run:
        pruned = prune_extension_usage(gltf)
        os.makedirs(BACKUP_DIR, exist_ok=True)
        backup = os.path.join(BACKUP_DIR, os.path.basename(path))
        if not os.path.exists(backup):
            shutil.copy2(path, backup)
        dump_like_blender(gltf, path)

    print(
        f"{'[dry-run] ' if dry_run else ''}{os.path.basename(path)}: "
        f"材质 {len(gltf.get('materials', []))}，接 ORM {len(counts['wired'])}，"
        f"已接 {len(counts['already'])}，无 specularTexture {len(counts['untouched'])}"
        + ("，extensionsUsed 移除 KHR_materials_specular" if pruned else "")
    )
    if counts["bad_index"]:
        print(f"  警告：{len(counts['bad_index'])} 个材质引用了越界贴图索引: {counts['bad_index'][:10]}")
    if verbose:
        for name in counts["wired"]:
            print(f"  wired: {name}")
    return len(counts["wired"])


def run(paths, dry_run=False, keep_specular=False, verbose=False):
    total = 0
    for path in paths:
        total += fix_file(path, dry_run, keep_specular, verbose)
    print(f"合计接 ORM {total} 处")
    return total


def main():
    parser = argparse.ArgumentParser(description="把 ORCA 的 Specular(ORM) 图改接为 glTF metallicRoughnessTexture")
    parser.add_argument("--dry-run", action="store_true", help="只打印，不写盘")
    parser.add_argument("--keep-specular", action="store_true", help="保留 KHR_materials_specular")
    parser.add_argument("--verbose", action="store_true", help="逐个打印接线的材质")
    parser.add_argument("gltf", nargs="+", help="要修正的 .gltf 文件")
    args = parser.parse_args()
    run(args.gltf, args.dry_run, args.keep_specular, args.verbose)


if __name__ == "__main__":
    main()
