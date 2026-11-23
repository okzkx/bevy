"""FBX -> glTF 转换 CLI（Blender 转换 + ORCA 材质接线修复）。

子命令:
  convert  <in.fbx> <out.gltf>  调 Blender 转换，随后跑 ORM 接线与 alpha 语义修复
  fix-orm  <a.gltf> [...]       只做 ORM 接线修复（已有资产可单独补跑）

示例（仓库根目录）:
  python ash_renderer/tools/fbx_to_gltf/fbx2gltf.py convert \\
      assets/external/orca/EmeraldSquare_v4_1/EmeraldSquare_Dusk.fbx \\
      assets/emerald/emerald_square_dusk/EmeraldSquare_Dusk.gltf
  python ash_renderer/tools/fbx_to_gltf/fbx2gltf.py fix-orm --dry-run \\
      assets/emerald/emerald_square_day/EmeraldSquare_Day.gltf
"""

import argparse
import glob
import os
import shutil
import subprocess
import sys
from pathlib import Path

import fix_gltf_orm

ROOT = Path(__file__).resolve().parents[3]
BLENDER_GLOBS = (
    "C:/Program Files/Blender Foundation/Blender */blender.exe",
    "C:/Program Files (x86)/Blender Foundation/Blender */blender.exe",
)


def find_blender(explicit):
    if explicit:
        path = Path(explicit)
        if path.is_file():
            return path
        raise SystemExit(f"找不到 Blender: {explicit}")
    env = os.environ.get("BLENDER")
    if env and Path(env).is_file():
        return Path(env)
    found = shutil.which("blender") or shutil.which("blender.exe")
    if found:
        return Path(found)
    candidates = []
    for pattern in BLENDER_GLOBS:
        candidates.extend(glob.glob(pattern))
    if candidates:
        return Path(sorted(candidates, reverse=True)[0])
    raise SystemExit(
        "找不到 Blender。用 --blender 指定，或设置环境变量 BLENDER，或加进 PATH。"
    )


def run_alpha_fix(gltf_path):
    script = ROOT / "tools" / "fix_gltf_alpha.py"
    if not script.is_file():
        print(f"[cli] 未找到 {script}，跳过 alpha 语义修复")
        return 0
    return subprocess.run(
        [sys.executable, str(script), str(gltf_path)], cwd=str(ROOT)
    ).returncode


def cmd_convert(args):
    in_fbx = Path(args.input).resolve()
    out_gltf = Path(args.output).resolve()
    if not in_fbx.is_file():
        raise SystemExit(f"找不到输入 FBX: {in_fbx}")
    blender = find_blender(args.blender)
    script = Path(__file__).with_name("blender_convert.py")
    out_gltf.parent.mkdir(parents=True, exist_ok=True)

    cmd = [
        str(blender), "--background", "--factory-startup",
        "--python", str(script), "--", str(in_fbx), str(out_gltf),
    ]
    print("[cli] " + " ".join(cmd))
    if subprocess.run(cmd).returncode != 0:
        raise SystemExit("[cli] Blender 转换失败")
    if not out_gltf.is_file():
        raise SystemExit(f"[cli] 转换结束但没有产物: {out_gltf}")

    if args.no_orm_fix:
        print("[cli] 跳过 ORM 接线修复")
    else:
        fix_gltf_orm.run([str(out_gltf)], dry_run=False)
    if args.no_alpha_fix:
        print("[cli] 跳过 alpha 语义修复")
    else:
        run_alpha_fix(out_gltf)
    return 0


def cmd_fix_orm(args):
    fix_gltf_orm.run(args.gltf, args.dry_run, args.keep_specular, args.verbose)
    return 0


def main():
    parser = argparse.ArgumentParser(description="FBX -> glTF 转换与 ORCA ORM 接线修复")
    sub = parser.add_subparsers(dest="command", required=True)

    convert = sub.add_parser("convert", help="FBX 转 glTF 并跑后处理")
    convert.add_argument("input", help="输入 .fbx")
    convert.add_argument("output", help="输出 .gltf")
    convert.add_argument("--blender", help="Blender 可执行文件路径（默认自动查找）")
    convert.add_argument("--no-orm-fix", action="store_true", help="跳过 ORM 接线修复")
    convert.add_argument("--no-alpha-fix", action="store_true", help="跳过 alpha 语义修复")
    convert.set_defaults(func=cmd_convert)

    fix_orm = sub.add_parser("fix-orm", help="只修 ORM 接线")
    fix_orm.add_argument("--dry-run", action="store_true", help="只打印，不写盘")
    fix_orm.add_argument("--keep-specular", action="store_true", help="保留 KHR_materials_specular")
    fix_orm.add_argument("--verbose", action="store_true", help="逐个打印接线的材质")
    fix_orm.add_argument("gltf", nargs="+", help="要修正的 .gltf 文件")
    fix_orm.set_defaults(func=cmd_fix_orm)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
