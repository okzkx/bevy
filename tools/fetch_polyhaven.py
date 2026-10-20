# -*- coding: utf-8 -*-
"""Poly Haven CC0 单件资产获取脚本（7.3.1）。

通道（2026-10-20 API 实测）：
  - 资产文件表：api.polyhaven.com/files/<Asset>（gltf/<res>/gltf.url + Diffuse/
    nor_gl/Metal/Rough 各分辨率分离贴图 URL）
  - gltf 包形状：.gltf + 同目录 .bin + `textures/` 相对 URI——非自含，
    贴图按 URI 基名映射到 dl.polyhaven.org/.../Models/jpg/<res>/<Asset>/ 下

落位：assets/polyhaven/<Asset>/{<Asset>_<res>.gltf, *.bin, textures/}——
AssetServer 相对路径 polyhaven/<Asset>/<Asset>_<res>.gltf。

选件终表（材质互补，为 8.2 Filament BRDF 供料）：
  ArmChair_01（布+木）/ antique_ceramic_vase_01（陶瓷）/
  brass_vase_01（黄铜金属）/ carved_wooden_elephant（木雕）

用法：python tools/fetch_polyhaven.py [--res 2k]
退出码：全下齐 = 0，任一失败 = 1。
"""

import argparse
import json
import os
import subprocess
import sys
import urllib.request

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except AttributeError:
    pass

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ASSETS = [
    "ArmChair_01",
    "antique_ceramic_vase_01",
    "brass_vase_01",
    "carved_wooden_elephant",
]
DL = "https://dl.polyhaven.org/file/ph-assets"
UA = {"User-Agent": "bevy-ash-renderer-learning/0.1 (local fetch script)"}


def fetch(url, dest):
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    req = urllib.request.Request(url, headers=UA)
    with urllib.request.urlopen(req, timeout=60) as r, open(dest, "wb") as f:
        f.write(r.read())
    return os.path.getsize(dest)


def fetch_curl(url, dest):
    # urllib 对 dl.polyhaven.org 偶发 403（UA 敏感），curl 通道兜底
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    subprocess.run(["curl", "-sfL", "-A", UA["User-Agent"], url, "-o", dest], check=True)
    return os.path.getsize(dest)


def walk_urls(o):
    if isinstance(o, dict):
        if isinstance(o.get("url"), str):
            yield o["url"]
        for v in o.values():
            yield from walk_urls(v)
    elif isinstance(o, list):
        for v in o:
            yield from walk_urls(v)


def main():
    ap = argparse.ArgumentParser(description="Poly Haven 单件资产获取（7.3.1）")
    ap.add_argument("--res", default="2k", help="贴图分辨率档（默认 2k）")
    args = ap.parse_args()

    failed = []
    for name in ASSETS:
        out_dir = os.path.join(REPO, "assets", "polyhaven", name)
        marker = os.path.join(out_dir, ".fetched")
        if os.path.isfile(marker):
            print(f"[SKIP] {name} 已获取")
            continue
        try:
            import urllib.error
            try:
                with urllib.request.urlopen(
                        f"https://api.polyhaven.com/files/{name}", timeout=30) as r:
                    files = json.load(r)
            except urllib.error.HTTPError:
                raw = subprocess.run(
                    ["curl", "-sf", f"https://api.polyhaven.com/files/{name}"],
                    capture_output=True, check=True).stdout
                files = json.loads(raw)
            gltf_url = files["gltf"][args.res]["gltf"]["url"]
            gltf_path = os.path.join(out_dir, os.path.basename(gltf_url))
            fetch_curl(gltf_url, gltf_path)
            g = json.load(open(gltf_path, encoding="utf-8"))
            gltf_dir = gltf_url.rsplit("/", 1)[0]
            got = [os.path.basename(gltf_path)]
            # buffer：gltf 同目录兄弟路径（实测 200）
            for buf in g.get("buffers", []):
                uri = buf.get("uri")
                if uri and not uri.startswith("data:"):
                    fetch_curl(f"{gltf_dir}/{uri}", os.path.join(out_dir, uri))
                    got.append(uri)
            # 贴图：URI `textures/X.jpg` → Models/jpg/<res>/<Asset>/X（实测 200）
            jpg_urls = {os.path.basename(u): u for u in walk_urls(files) if "/jpg/" in u}
            for img in g.get("images", []):
                uri = img.get("uri")
                if not uri:
                    print(f"[FAIL] {name}: 内嵌贴图暂不支持")
                    raise RuntimeError("embedded image")
                basename = os.path.basename(uri)
                url = jpg_urls.get(basename) or f"{DL}/Models/jpg/{args.res}/{name}/{basename}"
                fetch_curl(url, os.path.join(out_dir, uri))
                got.append(uri)
            open(marker, "w").write("ok\n")
            print(f"[OK] {name}: {len(got)} 文件（{', '.join(got)}）")
        except Exception as e:
            failed.append(name)
            print(f"[FAIL] {name}: {e}")

    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
