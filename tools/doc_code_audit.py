# -*- coding: utf-8 -*-
"""
用途：文档与代码对账（机械层）。扫描 .agents 下的 Markdown，校验三类引用：

  1) Markdown 相对链接：目标文件是否存在（文档改名/搬目录后最容易断）。
  2) 反引号里的代码路径：`xxx.rs` / `a/b/c.wgsl` 是否能解析到真实文件。
     若原路径找不到但仓库里有同名文件，报"疑似已迁移"。
  3) 反引号里的符号：`Foo::bar` / `snake_case` / `CamelCase` 是否在
     ash_renderer 源码、shader、或 bevy crates 源码里出现过。

调用：
  python -I tools/doc_code_audit.py              # 全量，人类可读报告
  python -I tools/doc_code_audit.py --only 笔记   # 只看某前缀
  python -I tools/doc_code_audit.py --json       # 机器可读

输出：.temp/doc_audit/report.json + 终端摘要。脚本本身无副作用。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.parse
from collections import Counter, defaultdict
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
AGENTS = REPO / ".agents"
OUT_DIR = REPO / ".temp" / "doc_audit"

CODE_ROOTS = [REPO / "ash_renderer" / "src", REPO / "ash_renderer" / "shaders",
              REPO / "ash_renderer" / "build.rs", REPO / "ash_renderer" / "Cargo.toml",
              REPO / "crates"]
CODE_EXTS = {".rs", ".wgsl", ".toml", ".py", ".md", ".json", ".glsl", ".sh"}
SKIP_DIRS = {".git", "target", ".temp", "node_modules", "pkg"}

# 外部 API / 生态名词：不是本仓库代码符号，出现即跳过
EXTERNAL_PREFIX = ("vk", "Vk", "VK_", "SDL_", "D3D", "ID3D", "wgpu", "WGPU",
                   "MTL", "CUDA", "Vma", "VMA_", "spirv_", "SPIRV_")

BACKTICK = re.compile(r"`([^`\n]+)`")
MD_LINK = re.compile(r"\[[^\]]*\]\(([^)\n]+)\)")
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
SYMBOL_SHAPE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*(?:\(\))?$")
CAMEL = re.compile(r"[a-z][A-Z]|[A-Z][a-z]+[A-Z]")

# 概念词黑名单：这些出现在反引号里但不是代码符号
CONCEPT = {
    "single", "multi", "read", "write", "none", "auto", "true", "false",
    "main", "src", "mod", "fn", "struct", "enum", "trait", "impl", "pub",
    "let", "mut", "self", "crate", "use", "as", "in", "for", "if", "else",
    "fence", "barrier", "pipeline", "descriptor", "instance", "vertex",
    "fragment", "compute", "storage", "uniform", "sampler", "image", "buffer",
    "queue", "submit", "present", "signal", "wait", "todo", "wip", "s_",
    "vs_", "fs_", "cs_", "binding", "set", "push_constant", "timestamp",
    "query", "timeout", "nan", "inf", "null", "usize", "u32", "u64", "u8",
    "f32", "f64", "i32", "bool", "vec2", "vec3", "vec4", "mat4", "array",
    "location", "builtin", "workgroup", "group", "override", "const", "var",
    "texture_2d", "texture_2d_array", "storage_buffer", "uniform_buffer",
}


def iter_md():
    for p in sorted(AGENTS.rglob("*.md")):
        if any(part in (".git", "_assets") for part in p.parts):
            continue
        yield p


def read(p: Path) -> str:
    return p.read_text(encoding="utf-8", errors="replace")


def build_code_index():
    """文件名索引 + 符号集。"""
    files: dict[str, list[Path]] = defaultdict(list)
    symbols: set[str] = set()
    for root in CODE_ROOTS:
        if not root.exists():
            continue
        if root.is_file():
            walk = [root]
        else:
            walk = [p for p in root.rglob("*") if p.is_file()]
        for p in walk:
            files[p.name].append(p)
            if p.suffix in (".rs", ".wgsl") or p.name == "Cargo.toml":
                try:
                    text = p.read_text(encoding="utf-8", errors="replace")
                except OSError:
                    continue
                for m in IDENT.finditer(text):
                    symbols.add(m.group(0))
    # 全仓文件名索引：用于判断"文档写的路径已迁移到别处"
    for p in REPO.rglob("*"):
        if not p.is_file():
            continue
        if any(part in SKIP_DIRS for part in p.parts):
            continue
        if p.suffix not in CODE_EXTS:
            continue
        if p not in files.get(p.name, []):
            files[p.name].append(p)
    return files, symbols


def resolve_code_path(token: str, doc: Path, files: dict[str, list[Path]]):
    """返回 (status, detail)。status: ok / relocated / missing。"""
    raw = token.strip().split("#")[0]
    if not raw or " " in raw:
        return None
    suf = Path(raw).suffix.lower()
    if suf not in CODE_EXTS:
        return None
    # 排除明显的命令行/通配
    if any(c in raw for c in "*?<>|"):
        return None
    cand = Path(raw.replace("\\", "/"))
    roots = [doc.parent, REPO, REPO / "ash_renderer", AGENTS, AGENTS / "docs"]
    # 逐级向上尝试 doc 的祖先
    for anc in doc.parents:
        roots.append(anc)
    seen = set()
    for r in roots:
        if r in seen:
            continue
        seen.add(r)
        target = (r / cand)
        try:
            if target.exists() and target.is_file():
                return ("ok", str(target.relative_to(REPO)))
        except (OSError, ValueError):
            continue
    base = cand.name
    if base in files:
        return ("relocated", "; ".join(str(p.relative_to(REPO)) for p in files[base][:3]))
    return ("missing", raw)


def looks_codey(tok: str) -> bool:
    t = tok.strip()
    if not t or " " in t:
        return False
    if t.lower() in CONCEPT:
        return False
    core = t[:-2] if t.endswith("()") else t
    if core.startswith(EXTERNAL_PREFIX):
        return False
    if not SYMBOL_SHAPE.match(t) and not SYMBOL_SHAPE.match(core):
        return False
    if "::" in core:
        return True
    if "_" in core.strip("_"):
        return True
    if CAMEL.search(core):
        return True
    if t.endswith("()") and len(core) > 4:
        return True
    return False


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", default=None, help="只处理路径包含该子串的文档")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    files, symbols = build_code_index()
    print(f"[index] 文件名 {len(files)}，符号 {len(symbols)}", file=sys.stderr)

    report = {"links_missing": [], "path_relocated": [], "path_missing": [],
              "symbol_missing": [], "stats": {}}
    per_doc_sym = Counter()
    all_missing_syms = Counter()

    for doc in iter_md():
        rel = doc.relative_to(REPO).as_posix()
        if args.only and args.only not in rel:
            continue
        text = read(doc)
        # 1) markdown 链接
        for m in MD_LINK.finditer(text):
            tgt = m.group(1).strip()
            if tgt.startswith(("http://", "https://", "mailto:", "#", "tel:")):
                continue
            if ' "' in tgt:  # 去掉可选的链接标题
                tgt = tgt.split(' "', 1)[0].strip()
            tgt = tgt.split("#")[0]
            if not tgt:
                continue
            tgt = urllib.parse.unquote(tgt)
            target = (doc.parent / tgt).resolve()
            try:
                exists = target.exists()
            except OSError:
                exists = False
            if not exists:
                report["links_missing"].append(
                    {"doc": rel, "target": m.group(1), "resolved_to": str(target)})
        # 2)(3) 反引号
        for m in BACKTICK.finditer(text):
            tok = m.group(1).strip()
            r = resolve_code_path(tok, doc, files)
            if r:
                status, detail = r
                if status == "relocated":
                    report["path_relocated"].append(
                        {"doc": rel, "token": tok, "found": detail})
                elif status == "missing":
                    report["path_missing"].append({"doc": rel, "token": tok})
                continue
            if looks_codey(tok):
                core = tok[:-2] if tok.endswith("()") else tok
                last = core.split("::")[-1].split("<")[0].strip("_")
                if last and last not in symbols:
                    per_doc_sym[rel] += 1
                    all_missing_syms[tok] += 1
                    report["symbol_missing"].append(
                        {"doc": rel, "token": tok, "leaf": last})

    report["stats"] = {
        "docs": len(list(iter_md())),
        "links_missing": len(report["links_missing"]),
        "path_relocated": len(report["path_relocated"]),
        "path_missing": len(report["path_missing"]),
        "symbol_missing": len(report["symbol_missing"]),
        "docs_with_missing_symbols": len(per_doc_sym),
    }

    # 4) 索引覆盖：有哪些文档没有被任何文档链接
    if not args.only:
        inbound: dict[Path, set[Path]] = defaultdict(set)
        all_md = list(iter_md())
        all_md_set = set(all_md)
        for doc in all_md:
            for m in MD_LINK.finditer(read(doc)):
                tgt = m.group(1).strip()
                if tgt.startswith(("http://", "https://", "mailto:", "#", "tel:")):
                    continue
                if ' "' in tgt:
                    tgt = tgt.split(' "', 1)[0].strip()
                tgt = urllib.parse.unquote(tgt.split("#")[0])
                if not tgt:
                    continue
                try:
                    target = (doc.parent / tgt).resolve()
                except (OSError, ValueError):
                    continue
                if target.suffix == ".md" and target.exists() and target in all_md_set:
                    inbound[target].add(doc)
        orphans = []
        for doc in all_md:
            name = doc.name
            if name.upper().startswith("README"):
                continue
            if doc == REPO / ".agents" / "AGENTS.md":
                continue
            if not inbound.get(doc):
                orphans.append(doc.relative_to(REPO).as_posix())
        report["orphans"] = orphans
        report["stats"]["orphans"] = len(orphans)

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    (OUT_DIR / "report.json").write_text(
        json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")

    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=1))
        return 0

    print("\n=== 统计 ===")
    for k, v in report["stats"].items():
        print(f"  {k}: {v}")

    def dump(title, items, key_a, key_b, limit=40):
        print(f"\n=== {title}（{len(items)}）===")
        for it in items[:limit]:
            print(f"  {it[key_a]}  ::  {it[key_b]}")
        if len(items) > limit:
            print(f"  ... 其余 {len(items)-limit} 条见 report.json")

    dump("断链", report["links_missing"], "doc", "target")
    dump("代码路径疑似迁移", report["path_relocated"], "doc", "token")
    dump("代码路径不存在", report["path_missing"], "doc", "token")
    dump("符号未命中（前 60 高频）", [
        {"doc": f"x{ n }", "token": t} for t, n in all_missing_syms.most_common(60)
    ], "doc", "token")
    dump("无入链文档", [{"doc": o, "token": ""} for o in report.get("orphans", [])],
         "doc", "token", limit=200)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
