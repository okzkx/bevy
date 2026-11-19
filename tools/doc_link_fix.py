# -*- coding: utf-8 -*-
"""
用途：修复 .agents 下 Markdown 相对链接的失效目标。

规则：链接目标不存在时，取 basename（含目录名）在全仓索引里找唯一同名文件/目录，
命中则把链接目标改写成从文档目录出发的正确相对路径；锚点（#...）原样保留。
不改链接文字（历史记录里的旧路径文字保持原样，点击跳到现行位置）。
多义 basename（README.md、mod.rs 等）不猜，列入待人工处理。

调用：
  python -I tools/doc_link_fix.py --dry-run     # 只报告
  python -I tools/doc_link_fix.py               # 实际改写
输出：.temp/doc_audit/link_fix.json
"""

from __future__ import annotations

import argparse
import json
import os
import re
import urllib.parse
from collections import defaultdict
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
AGENTS = REPO / ".agents"
OUT_DIR = REPO / ".temp" / "doc_audit"
SKIP_DIRS = {".git", "target", ".temp", "node_modules", "pkg", "artifacts"}

MD_LINK = re.compile(r"(\[[^\]]*\]\()([^)\n]+)(\))")


def iter_md():
    for p in sorted(AGENTS.rglob("*.md")):
        yield p


def norm(name: str) -> str:
    return (name.replace(" ", "").replace("：", ":").replace("（", "(")
            .replace("）", ")").lower())


def build_index():
    files: dict[str, list[Path]] = defaultdict(list)
    for p in REPO.rglob("*"):
        if any(part in SKIP_DIRS for part in p.parts):
            continue
        if p.is_file() or p.is_dir():
            files[norm(p.name)].append(p)
    return files


# 手工映射：basename（归一化后）→ 仓库内目标路径
MANUAL: dict[str, str] = {
    "resources.rs": "ash_renderer/src/render/core/device/memory/",
    "vulkan/lod.rs": "ash_renderer/src/render/passes/visibility/lod.rs",
    "remote/lod.rs": "ash_renderer/src/remote/features/lod.rs",
    "vulkan/indirect.rs": "ash_renderer/src/render/passes/visibility/indirect.rs",
    "vulkan/cull.rs": "ash_renderer/src/render/passes/visibility/cull.rs",
    "remote/residency.rs": "ash_renderer/src/remote/features/residency.rs",
    "vulkan/frames.rs": "ash_renderer/src/render/frame/record.rs",
    "src/frames.rs": "ash_renderer/src/render/frame/record.rs",
    "host.rs": "ash_renderer/src/driver/host/mod.rs",
    "descriptors.rs": "ash_renderer/src/render/core/binding/mod.rs",
    "remote_assets.rs": "ash_renderer/src/remote/edit/assets.rs",
    "5.3-indirect闭环/indirect闭环机制三问：API边界、dispatch驱动与布局归属.md":
        ".agents/docs/开发步骤/5-GPU驱动与常驻大场景/5.3-indirect闭环/"
        "indirect闭环的边界与归属：API能力、dispatch驱动与布局契约.md",
    "filament源码参考:标准模型brdf与多光源(8.2对照基准).md":
        ".agents/docs/笔记/渲染/Filament BRDF参考.md",
    "gpu lod 与流送.md": ".agents/docs/笔记/渲染/GPU LOD与流送.md",
    "3.6.2-scene分层:机制与业务两半.md":
        ".agents/docs/开发步骤/3-静态取数链路/3.6-代码全量整理/3.6.2-scene分层：机制与业务两半.md",
    "_assets/": None,  # 由上下文就近决定，脚本按 doc 目录上溯找到最近的 _assets
}
MANUAL = {norm(k): v for k, v in MANUAL.items() if v}


def enc(rel: str) -> str:
    return rel.replace("\\", "/").replace(" ", "%20")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    index = build_index()
    changes = []
    ambiguous = []
    still_missing = []

    for doc in iter_md():
        text = doc.read_bytes().decode("utf-8", "replace")
        new_text = text
        rel_doc = doc.relative_to(REPO).as_posix()
        for m in MD_LINK.finditer(text):
            target = m.group(2).strip()
            if target.startswith(("http://", "https://", "mailto:", "#", "tel:")):
                continue
            title = ""
            if ' "' in target:
                target, _, title = target.partition(' "')
                target = target.strip()
                title = ' "' + title
            raw = urllib.parse.unquote(target)
            path_part, sep, anchor = raw.partition("#")
            if not path_part:
                continue
            resolved = (doc.parent / path_part).resolve()
            if resolved.exists():
                continue
            base = Path(path_part.rstrip("/")).name
            if not base:
                continue
            key = norm(base)
            tail2 = norm("/".join(path_part.rstrip("/").split("/")[-2:]))
            # 就近 _assets：从文档目录向上找第一个含 _assets 子目录的祖先
            if key == "_assets":
                anc = doc.parent
                while anc != REPO and not (anc / "_assets").is_dir():
                    anc = anc.parent
                if (anc / "_assets").is_dir():
                    new_rel = os.path.relpath(anc / "_assets", doc.parent)
                    new_tgt = enc(new_rel) + "/" + (("#" + anchor) if sep else "")
                    old_link = m.group(0)
                    new_link = m.group(1) + new_tgt + title + m.group(3)
                    new_text = new_text.replace(old_link, new_link, 1)
                    changes.append({"doc": rel_doc, "from": target, "to": new_tgt,
                                    "file": str((anc / "_assets").relative_to(REPO))})
                continue
            # 手工映射优先
            manual = MANUAL.get(tail2) or MANUAL.get(key)
            if manual:
                target_path = REPO / manual
                if target_path.exists():
                    new_rel = os.path.relpath(target_path, doc.parent)
                    new_tgt = enc(new_rel) + (("#" + anchor) if sep else "")
                    if path_part.endswith("/"):
                        new_tgt = enc(new_rel) + "/" + (("#" + anchor) if sep else "")
                    old_link = m.group(0)
                    new_link = m.group(1) + new_tgt + title + m.group(3)
                    new_text = new_text.replace(old_link, new_link, 1)
                    changes.append({"doc": rel_doc, "from": target, "to": new_tgt,
                                    "file": manual})
                    continue
            cands = index.get(key, [])
            if len(cands) == 1:
                new_rel = os.path.relpath(cands[0], doc.parent)
                new_tgt = enc(new_rel) + (("#" + anchor) if sep else "")
                if path_part.endswith("/"):
                    new_tgt = enc(new_rel) + "/" + (("#" + anchor) if sep else "")
                old_link = m.group(0)
                new_link = m.group(1) + new_tgt + m.group(3)
                if old_link != new_link:
                    new_text = new_text.replace(old_link, new_link, 1)
                    changes.append({"doc": rel_doc, "from": target,
                                    "to": new_tgt, "file": str(cands[0].relative_to(REPO))})
            elif len(cands) > 1:
                ambiguous.append({"doc": rel_doc, "target": target,
                                  "candidates": [str(p.relative_to(REPO)) for p in cands[:6]]})
            else:
                still_missing.append({"doc": rel_doc, "target": target})

        if new_text != text and not args.dry_run:
            doc.write_bytes(new_text.encode("utf-8"))

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    (OUT_DIR / "link_fix.json").write_text(
        json.dumps({"changes": changes, "ambiguous": ambiguous,
                    "missing": still_missing}, ensure_ascii=False, indent=1),
        encoding="utf-8")

    print(f"修复 {len(changes)} 条；多义 {len(ambiguous)} 条；无匹配 {len(still_missing)} 条")
    for c in changes[:200]:
        print(f"  {c['doc']}\n    {c['from']}  ->  {c['to']}")
    print("\n--- 多义（人工） ---")
    for a in ambiguous[:60]:
        print(f"  {a['doc']} :: {a['target']} -> {a['candidates']}")
    print("\n--- 无匹配 ---")
    for a in still_missing[:60]:
        print(f"  {a['doc']} :: {a['target']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
