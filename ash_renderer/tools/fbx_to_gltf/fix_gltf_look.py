"""Emerald 场景观感修正：删除 WorldGrid 占位地面，把玻璃材质改成半透明。

子命令:
  drop-placeholders  <a.gltf> [...]  删除材质名以 WorldGrid 开头的 primitive
  glass-blend        <a.gltf> [...]  把清单里的玻璃材质改成 alphaMode=BLEND

两个子命令都只改表现相关字段，可重复执行：删过的占位 primitive 不再出现，
已是 BLEND 的玻璃材质跳过。写盘前把原文件备份到 .temp/gltf_look_backup/
（已有备份不覆盖，保留首次改前快照）。

用法（仓库根目录）:
  python ash_renderer/tools/fbx_to_gltf/fix_gltf_look.py drop-placeholders [--dry-run] <a.gltf> [...]
  python ash_renderer/tools/fbx_to_gltf/fix_gltf_look.py glass-blend [--dry-run] <a.gltf> [...]
"""

import argparse
import io
import json
import os
import shutil

BACKUP_DIR = os.path.join(".temp", "gltf_look_backup")

# 材质名 -> 半透明 A 值。RGB 保持 1，颜色仍由 baseColorTexture 提供。
GLASS_ALPHA = {
    "glass": 0.35,
    "hotel_windows": 0.40,
    "Tower_Windows_Ball": 0.40,
    "Sheleg_Glass_Sign": 0.50,
    "Glass_Signs": 0.50,
    "Generic_2_Glass_Roof_Awning_Red": 0.45,
    "Generic_2_Glass_Roof_Awning_Red1": 0.45,
    "Generic_2_Glass_Roof_Awning_Red2": 0.45,
    "Generic_2_Glass_Roof_Awning_Red3": 0.45,
}

PLACEHOLDER_PREFIX = "WorldGrid"


def load_gltf(path):
    try:
        with open(path, "rb") as f:
            return json.load(f)
    except OSError as exc:
        raise SystemExit(f"读不到 {path}: {exc}") from exc
    except json.JSONDecodeError as exc:
        raise SystemExit(f"{path} 不是合法的 glTF JSON: {exc}") from exc


def material_names(gltf):
    return [material.get("name", "") for material in gltf.get("materials", [])]


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


def backup_file(path):
    """写盘前备份原文件；同名备份已存在时保留首次快照。"""
    os.makedirs(BACKUP_DIR, exist_ok=True)
    backup = os.path.join(BACKUP_DIR, os.path.basename(path))
    if not os.path.exists(backup):
        shutil.copy2(path, backup)
    return backup


def drop_placeholders_file(path, dry_run, verbose):
    """删掉一个文件里材质名以 WorldGrid 开头的 primitive，返回删除处数。"""
    gltf = load_gltf(path)
    names = material_names(gltf)
    removed = []
    empty_meshes = []
    for mesh in gltf.get("meshes", []):
        primitives = mesh.get("primitives", [])
        kept = []
        for prim_index, primitive in enumerate(primitives):
            material_index = primitive.get("material")
            name = (
                names[material_index]
                if isinstance(material_index, int) and 0 <= material_index < len(names)
                else ""
            )
            if name.startswith(PLACEHOLDER_PREFIX):
                removed.append((mesh.get("name", ""), prim_index, name))
            else:
                kept.append(primitive)
        if len(kept) == len(primitives):
            continue
        if not kept:
            empty_meshes.append(mesh.get("name", ""))
        mesh["primitives"] = kept

    if removed and not dry_run:
        backup_file(path)
        dump_like_blender(gltf, path)

    print(
        f"{'[dry-run] ' if dry_run else ''}{os.path.basename(path)}: "
        f"删除占位 primitive {len(removed)} 处（材质表里 "
        f"{sum(1 for name in names if name.startswith(PLACEHOLDER_PREFIX))} 个占位条目保留）"
    )
    if verbose:
        for mesh_name, prim_index, name in removed:
            print(f"  drop: {mesh_name} primitive[{prim_index}] {name}")
    if empty_meshes:
        print(f"  警告：{len(empty_meshes)} 个 mesh 的 primitive 被全部删除: {empty_meshes[:10]}")
    return len(removed)


def glass_blend_file(path, dry_run, verbose):
    """把清单里的玻璃材质改成 BLEND，返回本次修改的材质数。"""
    gltf = load_gltf(path)
    changed = []
    already = []
    missing = set(GLASS_ALPHA)
    for material in gltf.get("materials", []):
        name = material.get("name", "")
        if name not in GLASS_ALPHA:
            continue
        missing.discard(name)
        if material.get("alphaMode") == "BLEND":
            already.append(name)
            continue
        alpha = GLASS_ALPHA[name]
        material["alphaMode"] = "BLEND"
        pbr = material.setdefault("pbrMetallicRoughness", {})
        pbr["baseColorFactor"] = [1.0, 1.0, 1.0, alpha]
        changed.append((name, alpha))

    if changed and not dry_run:
        backup_file(path)
        dump_like_blender(gltf, path)

    print(
        f"{'[dry-run] ' if dry_run else ''}{os.path.basename(path)}: "
        f"材质 {len(gltf.get('materials', []))} 个，改 BLEND {len(changed)} 个，"
        f"已是 BLEND {len(already)} 个，清单未命中 {len(missing)} 个"
    )
    if verbose:
        for name, alpha in changed:
            print(f"  blend: {name} a={alpha:g}")
        for name in already:
            print(f"  already: {name}")
        for name in sorted(missing):
            print(f"  missing: {name}")
    return len(changed)


ACTIONS = {
    "drop-placeholders": (
        "删除材质名以 WorldGrid 开头的占位 primitive",
        drop_placeholders_file,
    ),
    "glass-blend": (
        "把玻璃材质改成 alphaMode=BLEND 并设置半透明 baseColorFactor",
        glass_blend_file,
    ),
}


def run(command, paths, dry_run=False, verbose=False):
    """按子命令处理一批文件，打印逐文件计数与合计，返回总处数。"""
    if command not in ACTIONS:
        raise SystemExit(f"未知子命令: {command}")
    handler = ACTIONS[command][1]
    total = 0
    for path in paths:
        total += handler(path, dry_run, verbose)
    verb = "删除占位 primitive" if command == "drop-placeholders" else "改 BLEND 材质"
    print(f"合计{verb} {total} 处")
    return total


def main():
    parser = argparse.ArgumentParser(description="Emerald 场景观感修正（占位地面删除 + 玻璃半透明）")
    sub = parser.add_subparsers(dest="command", required=True)
    for name, (help_text, _handler) in ACTIONS.items():
        action = sub.add_parser(name, help=help_text)
        action.add_argument("--dry-run", action="store_true", help="只打印，不写盘")
        action.add_argument("--verbose", action="store_true", help="逐个打印删除/修改的条目")
        action.add_argument("gltf", nargs="+", help="要修正的 .gltf 文件")
    args = parser.parse_args()
    run(args.command, args.gltf, args.dry_run, args.verbose)


if __name__ == "__main__":
    main()
