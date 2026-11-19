# -*- coding: utf-8 -*-
"""
用途：把 .agents 下的 Markdown 统一成 UTF-8 无 BOM + CRLF 行尾（本仓工作区约定）。

背景：core.autocrlf=true，索引存 LF、工作区应为 CRLF。用 Python 文本模式读写会
把 CRLF 静默转成 LF，git status 会给出 “LF will be replaced by CRLF” 警告。
本脚本幂等：已经规范的文档不会被改写。

调用：python -I tools/doc_eol_normalize.py [--dry-run]
"""

from __future__ import annotations

import argparse
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
AGENTS = REPO / ".agents"
BOM = b"\xef\xbb\xbf"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    changed = []
    for p in sorted(AGENTS.rglob("*.md")):
        raw = p.read_bytes()
        body = raw[3:] if raw.startswith(BOM) else raw
        new = body.replace(b"\r\n", b"\n").replace(b"\n", b"\r\n")
        if new != raw:
            changed.append(str(p.relative_to(REPO)))
            if not args.dry_run:
                p.write_bytes(new)
    print(f"规范化 {len(changed)} 个文件（CRLF + 去 BOM）")
    for c in changed[:40]:
        print("  ", c)
    if len(changed) > 40:
        print(f"  ... 其余 {len(changed) - 40} 个")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
