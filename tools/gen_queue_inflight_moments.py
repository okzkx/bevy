"""设备队列在飞快照图生成器。

用途：生成笔记《多帧在飞竞争》的配图 queue-inflight-moments.svg——假想在运行时把 GPU
冻住截五个典型时刻，按设备队列分泳道画出此刻排在队列里的每个提交（帧 cmb / mip 续批 cmb /
present 操作 / 上传批 cmb）：执行到哪段、等哪个信号量、被谁卡住；顶上是交换链三张图的去向，
底下是 CPU 主线程停在 draw_frame 的哪一步。
调用：python tools/gen_queue_inflight_moments.py [输出 svg 路径]
队列拓扑按本机（RTX 2060）实测：族 0 = GRAPHICS+COMPUTE+PRESENT 一条队列，族 1 = 专用
transfer 队列；带 mip 的贴图上传在图形队列补一条 blit 续批（uploader.rs submit_batch）。
帧/槽/图/票据编号与 gen_frame_cmd_timeline.py 的稳态推演一致，改动时两份脚本同步。
"""

import sys
from html import escape

OUT = sys.argv[1] if len(sys.argv) > 1 else ".agents/docs/笔记/Vulkan/_assets/queue-inflight-moments.svg"
FONT = "Microsoft YaHei, PingFang SC, Noto Sans CJK SC, sans-serif"

W = 2000
parts = []
labels = []  # 箭头标签：箭头本体压在框体下面，标签浮在最上层

FENCE, TICKET, IMG, RFIN, STATUS = "#dc2626", "#d97706", "#9333ea", "#0d9488", "#2563eb"
ORDER = "#64748b"
ARROW_COLORS = [FENCE, TICKET, IMG, RFIN, "#475569", ORDER]
# 卡点横条配色：绿 = 门全开执行中；灰 = 只等队列顺序（非同步原语）；
# 红/橙/紫 = 卡在 fence / 票据 / 图三道门之一
BLOCK_STYLE = {"FENCE": ("#fef2f2", FENCE), "TICKET": ("#fffbeb", TICKET),
               "IMG": ("#f5f3ff", IMG), "ORDER": ("#f1f5f9", ORDER),
               "NONE": ("#f0fdf4", "#15803d"), "DONE": ("#f8fafc", "#94a3b8")}
FC = {"N-1": "#64748b", "N": "#2563eb", "N+1": "#059669", "N+2": "#d97706", "N+3": "#7c3aed",
      "N+4": "#db2777", "K-1": "#64748b", "K": "#2563eb", "K+1": "#059669"}
# 图形队列一帧九段（编号, 名, 时长, 填色）与主图 SEGS 一致
SEGS = [("①", "进场", 15, "#e5e7eb"), ("②", "compute", 90, "#ffedd5"), ("③", "阴影", 70, "#e7e5e4"),
        ("④", "G缓冲", 110, "#dcfce7"), ("⑤", "合成+透明", 80, "#fef9c3"), ("⑥", "Hi-Z", 40, "#fce7f3"),
        ("⑦", "Bloom", 60, "#ede9fe"), ("⑧", "后链+UI", 80, "#e0f2fe"), ("⑨", "出场", 15, "#e5e7eb")]
SEG_TOTAL = sum(s[2] for s in SEGS)
# draw_frame 各步（字母, 名, 显示宽权重）与主图 CPU_LETTERS 一致
CPU_STEPS = [("a", "上传", 70), ("b", "fence", 70), ("c", "acquire", 40), ("d", "DrawList", 40),
             ("e", "UBO", 30), ("f", "四环", 40), ("g", "备料", 50), ("h", "overlay", 40),
             ("i", "快照", 25), ("j", "录制", 105), ("k", "submit", 25), ("l", "present", 25)]


def rect(x, y, w, h, fill, stroke="#475569", dash=None, rx=4, sw=1.0, opacity=None, extra_attr=""):
    extra = f' stroke-dasharray="{dash}"' if dash else ""
    if opacity is not None:
        extra += f' opacity="{opacity}"'
    parts.append(
        f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h:.1f}" rx="{rx}" '
        f'fill="{fill}" stroke="{stroke}" stroke-width="{sw}"{extra}{extra_attr}/>'
    )


def text(x, y, s, size=11, anchor="middle", weight="normal", fill="#0f172a"):
    lines = s.split("\n")
    lh = size * 1.25
    y0 = y - (len(lines) - 1) * lh / 2 + size * 0.36
    spans = "".join(
        f'<tspan x="{x:.1f}" y="{y0 + i * lh:.1f}">{escape(line)}</tspan>' for i, line in enumerate(lines)
    )
    parts.append(
        f'<text font-size="{size}" text-anchor="{anchor}" font-weight="{weight}" fill="{fill}">{spans}</text>'
    )


def line(x1, y1, x2, y2, color="#475569", sw=1.0, dash=None, arrow=False):
    extra = f' stroke-dasharray="{dash}"' if dash else ""
    if arrow:
        extra += f' marker-end="url(#ah-{color[1:]})"'
    parts.append(f'<line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" stroke="{color}" stroke-width="{sw}"{extra}/>')


def curve(p1, p2, color, label="", sw=1.7, dash=None, lx=None, ly=None):
    (x1, y1), (x2, y2) = p1, p2
    my = (y1 + y2) / 2
    extra = f' stroke-dasharray="{dash}"' if dash else ""
    parts.append(
        f'<path d="M{x1:.1f},{y1:.1f} C{x1:.1f},{my:.1f} {x2:.1f},{my:.1f} {x2:.1f},{y2:.1f}" fill="none" '
        f'stroke="{color}" stroke-width="{sw}"{extra} marker-end="url(#ah-{color[1:]})"/>'
    )
    if label:
        tx = lx if lx is not None else (x1 + x2) / 2
        ty = ly if ly is not None else my
        w = len(label) * 11.5 + 10
        start = len(parts)
        rect(tx - w / 2, ty - 9, w, 18, "#ffffff", color, rx=9, sw=0.8)
        text(tx, ty, label, 10.5, fill=color, weight="bold")
        labels.extend(parts[start:])
        del parts[start:]


def arc_over(x1, x2, y, color, label, h=16, lx=None, dash=None):
    """同泳道相邻两项之间的拱形箭头（从 x1 拱到 x2）。标签走浮动列表，避免被框体盖住。"""
    extra = f' stroke-dasharray="{dash}"' if dash else ""
    parts.append(
        f'<path d="M{x1:.1f},{y:.1f} C{x1:.1f},{y - h:.1f} {x2:.1f},{y - h:.1f} {x2:.1f},{y:.1f}" fill="none" '
        f'stroke="{color}" stroke-width="1.5"{extra} marker-end="url(#ah-{color[1:]})"/>'
    )
    tx = lx if lx is not None else (x1 + x2) / 2
    ty = y - h - 2
    w = len(label) * 10 + 8
    labels.append(
        f'<rect x="{tx - w / 2:.1f}" y="{ty - 9:.1f}" width="{w:.1f}" height="18" rx="9" '
        f'fill="#ffffff" stroke="{color}" stroke-width="0.8"/>'
    )
    labels.append(
        f'<text x="{tx:.1f}" y="{ty:.1f}" font-size="9.5" text-anchor="middle" '
        f'font-weight="bold" fill="{color}">{escape(label)}</text>'
    )


# ---------------------------------------------------------------- 队列项绘制


def seg_bar(x, y, w, h, prog):
    """九段进度条。prog = (kind, idx, frac)：done 全彩 / run 跑到 idx 段的 frac /
    gated 停在 idx 段之前 / frozen 全灰 / queued 全白。"""
    kind, idx, frac = prog
    cx = x
    for i, (num, _, dur, fill) in enumerate(SEGS):
        sw = w * dur / SEG_TOTAL
        if kind == "done" or (kind in ("run", "gated") and i < idx):
            f, op = fill, None
        elif kind == "run" and i == idx:
            f, op = "#ffffff", None
        elif kind == "frozen":
            f, op = "url(#hatch)", None
        else:
            f, op = "#ffffff", None
        rect(cx, y, sw, h, f, "#94a3b8", rx=1, sw=0.6, opacity=op)
        if kind == "run" and i == idx:
            rect(cx, y, sw * frac, h, fill, "none", rx=1, sw=0)
            rect(cx - 1, y - 2, sw + 2, h + 4, "none", "#0f172a", rx=2, sw=1.6)
        if sw > 13:
            text(cx + sw / 2, y + h / 2, num, 9.5, fill="#334155")
        cx += sw
    if kind == "gated":
        gx = x + w * sum(s[2] for s in SEGS[:idx]) / SEG_TOTAL
        line(gx, y - 6, gx, y + h + 6, IMG, 3)
        text(gx, y - 11, "停", 10, fill=IMG, weight="bold")


def chips(x, y, items, max_w):
    """等待/信号小标签串。items = [(文字, 状态)]，状态 ok/no/sig/info。"""
    style = {"ok": ("#dcfce7", "#15803d", "✓ "), "no": ("#fee2e2", "#b91c1c", "✗ "),
             "sig": ("#ccfbf1", "#0f766e", "发 "), "info": ("#f1f5f9", "#475569", "")}
    cx = x
    for label, st in items:
        bg, fg, pre = style[st]
        s = pre + label
        w = len(s) * 10.2 + 8
        if cx + w > x + max_w:
            break
        rect(cx, y, w, 16, bg, fg, rx=8, sw=0.7)
        text(cx + w / 2, y + 8, s, 9.5, fill=fg)
        cx += w + 4


def block_banner(x, y, w, block):
    """框底卡点横条：这条 cmb 被谁卡在哪道门（图例给四色含义）。"""
    s, key = block
    bg, fg = BLOCK_STYLE[key]
    rect(x, y, w, 17, bg, fg, rx=3, sw=0.8)
    text(x + 5, y + 8.5, s, 9.8, "start", "bold", fg)


def box_frame(it, x, y, w, h):
    color = FC[it["frame"]]
    state = it["state"]
    if state == "run":
        rect(x, y, w, h, "#ffffff", color, sw=2.6, extra_attr=' filter="url(#lift)"')
    elif state == "frozen":
        rect(x, y, w, h, "#f8fafc", FENCE, dash="6 3", sw=2, extra_attr=' filter="url(#soft)"')
    elif state == "done":
        rect(x, y, w, h, "#f8fafc", "#cbd5e1", sw=1)
    else:
        rect(x, y, w, h, "#ffffff", color, dash="5 3", sw=1.6, extra_attr=' filter="url(#soft)"')
    op_start = len(parts)
    text(x + 8, y + 12, f"cmb · 帧 {it['frame']}", 12.5, "start", "bold", color)
    text(x + w - 8, y + 12, f"帧槽 {it['slot']} · 画 img{it['img']}", 10.5, "end", fill="#475569")
    seg_bar(x + 8, y + 26, w - 16, 15, it["prog"])
    chips(x + 8, y + 49, it["waits"], w - 16)
    chips(x + 8, y + 68, it["sigs"], w - 16)
    if it.get("block"):
        block_banner(x + 8, y + 87, w - 16, it["block"])
    text(x + 8, y + h - 12, it["status"], 10.5, "start", "bold", STATUS)
    if state == "done":
        parts.insert(op_start, '<g opacity="0.45">')
        parts.append("</g>")


def box_present(it, x, y, w, h):
    color = FC[it["frame"]]
    done = it["state"] == "done"
    if done:
        rect(x, y + 14, w, h - 28, "#ffffff", "none", rx=10, sw=0)
        parts.append('<g opacity="0.45">')
    rect(x, y + 14, w, h - 28, "#f0fdfa", RFIN, dash=None if done else "4 3", sw=1.4, rx=10,
         extra_attr=' filter="url(#soft)"')
    text(x + w / 2, y + 30, f"present · 帧 {it['frame']}", 11.5, weight="bold", fill=color)
    text(x + w / 2, y + 47, f"等 render_finished[img{it['img']}]", 9.5, fill="#0f766e")
    text(x + w / 2, y + 63, it["status"], 9.5, fill=STATUS, weight="bold")
    if done:
        parts.append("</g>")


def box_blit(it, x, y, w, h):
    state = it["state"]
    if state == "done":
        rect(x, y, w, h, "#ffffff", "none", sw=0)
        parts.append('<g opacity="0.45">')
    stroke, dash, sw = {"run": (TICKET, None, 2.4), "frozen": (FENCE, "6 3", 2),
                        "queued": (TICKET, "5 3", 1.6), "done": ("#cbd5e1", None, 1)}[state]
    rect(x, y, w, h, "#fffbeb" if state != "frozen" else "#f8fafc", stroke, dash=dash, sw=sw,
         extra_attr=' filter="url(#lift)"' if state == "run" else ' filter="url(#soft)"')
    text(x + 8, y + 12, "cmb · mip 续批（blit）", 12, "start", "bold", "#b45309")
    text(x + 8, y + 31, it["what"], 10, "start", fill="#475569")
    chips(x + 8, y + 49, it["waits"], w - 16)
    chips(x + 8, y + 68, it["sigs"], w - 16)
    if it.get("block"):
        block_banner(x + 8, y + 87, w - 16, it["block"])
    text(x + 8, y + h - 12, it["status"], 10.5, "start", "bold", STATUS)
    if state == "done":
        parts.append("</g>")


def box_upload(it, x, y, w, h):
    state = it["state"]
    if state == "done":
        rect(x, y, w, h, "#ffffff", "none", sw=0)
        parts.append('<g opacity="0.5">')
    rect(x, y, w, h, "#ffffff", TICKET if state == "run" else "#cbd5e1", sw=2.4 if state == "run" else 1,
         extra_attr=' filter="url(#lift)"' if state == "run" else ' filter="url(#soft)"')
    text(x + 8, y + 12, f"cmb · 上传批 #{it['ticket']} · 用 staging 槽{it['slot']}", 12, "start", "bold", "#b45309")
    text(x + w - 8, y + 12, it["what"], 10, "end", fill="#475569")
    bx, by, bw = x + 8, y + 28, w - 16
    rect(bx, by - 3, bw, 9, "#ffffff", "#94a3b8", rx=2, sw=0.6)
    rect(bx, by - 3, bw * it["frac"], 9, "#fcd34d", "none", rx=2, sw=0)
    text(x + 8, y + h - 9, it["status"], 10.5, "start", "bold", STATUS)
    if state == "done":
        parts.append("</g>")


ITEM_W = {"frame": 430, "present": 170, "blit": 300}
DRAW = {"frame": box_frame, "present": box_present, "blit": box_blit}

# ---------------------------------------------------------------- 五个时刻

MOMENTS = [
    dict(
        title="时刻 ① 稳态 · GPU 受限：两帧 cmb 在图形队列排队，CPU 停在 fence",
        accent="#0ea5e9",
        note="图形队列队首在跑帧 N-1，帧 N 整条 cmb 已提交在后面排着——这就是“两帧在飞”。CPU 已录完帧 N 又跑到帧 N+1，"
             "帧槽 B 的命令缓冲还被帧 N-1 占着，只能停在 b 等 fence[B]。规范上 cmb N 的 ②compute 并不被 N-1 的 ⑥~⑧ 挡"
             "（其进场门限只涉及 TRANSFER 段），可与 N-1 尾段重叠；本图按驱动前端顺序取命令画成整条排队。",
        screen=[("img0", "app", "已交给帧 N", "帧 N 的 cmb 排队中，将写这张"),
                ("img1", "scan", "扫描显示中", "屏幕上 = 帧 N-2 的画面"),
                ("img2", "app", "已交给帧 N-1", "cmb N-1 正在写（④ 起）")],
        gfx=[
            dict(kind="frame", frame="N-1", slot="B", img=2, state="run", prog=("run", 6, 0.4),
                 waits=[("票据≥7 @ALL", "ok"), ("img_avail[B] @COLOR_OUT", "ok")],
                 sigs=[("render_finished[img2]", "sig"), ("fence[B]", "sig")],
                 block=("无卡点：三道门全开，正常执行中", "NONE"),
                 status="状态：执行中 · ⑦ Bloom 降采样 c31"),
            dict(kind="present", frame="N-1", img=2, state="queued", status="排队"),
            dict(kind="frame", frame="N", slot="A", img=0, state="queued", prog=("queued", 0, 0),
                 waits=[("票据≥7 @ALL", "ok"), ("img_avail[A] @COLOR_OUT", "ok")],
                 sigs=[("render_finished[img0]", "sig"), ("fence[A]", "sig")],
                 block=("无同步卡点：门全开，只差队列顺序", "ORDER"),
                 status="状态：已提交，排在 N-1 之后"),
            dict(kind="present", frame="N", img=0, state="queued", status="排队"),
        ],
        up=[dict(state="done", ticket=7, slot=1, what="帧 N-1 的批", frac=1.0,
                 status="已执行完 · 出队"),
            dict(state="run", ticket=8, slot=0, what="帧 N+1 的 flush_uploads · 小批", frac=0.55,
                 status="状态：拷贝中 · 完成后置位 timeline = 8")],
        ledger=[("槽0", "#8 · 拷贝中，下次复用先等它"), ("槽1", "#7 · 已到，可复用")],
        cpu=dict(frame="N+1", step="b", status="状态：帧 N+1 · b wait_for_slot 阻塞——等 fence[B]，它挂在 cmb N-1 末尾"),
        links=[("fence", "gfx0", "cpu", "fence[B]"), ("rf", "gfx0", "gfx1", "render_finished"),
               ("order", "gfx0", "gfx2", "队列顺序")],
    ),
    dict(
        title="时刻 ② 上传批进队：多级贴图拆成两条 cmb——transfer 拷贝 + 图形队列 blit 续批",
        accent="#d97706",
        note="帧 N+2 的 flush_uploads 带了 15 张多级贴图：transfer 队列拷 mip0（#9），图形队列追加 blit 续批等 #9 再生成整条 mip 链、"
             "发 #10；帧 N+2 的快照取已发出最高票据 = 10。blit 排在 cmb N+1 后面，只要 #9 赶在图形队列跑到它之前拷完就零停顿。",
        screen=[("img0", "app", "已交给帧 N", "cmb N 正在写"),
                ("img1", "app", "已交给帧 N+1", "cmb N+1 排队中"),
                ("img2", "scan", "扫描显示中", "屏幕上 = 帧 N-1 的画面")],
        gfx=[
            dict(kind="frame", frame="N", slot="A", img=0, state="run", prog=("run", 4, 0.6),
                 waits=[("票据≥7 @ALL", "ok"), ("img_avail[A] @COLOR_OUT", "ok")],
                 sigs=[("render_finished[img0]", "sig"), ("fence[A]", "sig")],
                 block=("无卡点：三道门全开，正常执行中", "NONE"),
                 status="状态：执行中 · ⑤ 透明前向 c25"),
            dict(kind="present", frame="N", img=0, state="queued", status="排队"),
            dict(kind="frame", frame="N+1", slot="B", img=1, state="queued", prog=("queued", 0, 0),
                 waits=[("票据≥8 @ALL", "ok"), ("img_avail[B] @COLOR_OUT", "ok")],
                 sigs=[("render_finished[img1]", "sig"), ("fence[B]", "sig")],
                 block=("无同步卡点：T≥8 / A[B] 均已置位，只差队列顺序", "ORDER"),
                 status="状态：排队 · 票据 #8 早已拷完"),
            dict(kind="present", frame="N+1", img=1, state="queued", status="排队"),
            dict(kind="blit", state="queued", what="槽1 blit_cb · 15 张 × 逐级 blit + 收尾屏障",
                 waits=[("票据≥9 @ALL", "no")], sigs=[("timeline = 10", "sig")],
                 block=("被 #9 卡：@ALL_COMMANDS 等 timeline", "TICKET"),
                 status="状态：排队 · 等 #9（还在拷）"),
        ],
        up=[dict(state="done", ticket=8, slot=0, what="帧 N+1 的批", frac=1.0,
                 status="已执行完 · 出队"),
            dict(state="run", ticket=9, slot=1, what="帧 N+2 · 15 张贴图 mip0 + mesh · 大批", frac=0.12,
                 status="状态：拷贝中 12% · 完成后置位 timeline = 9")],
        ledger=[("槽0", "#8 · 已到，可复用"), ("槽1", "#10 · 要等 blit 续批也跑完")],
        cpu=dict(frame="N+2", step="b", status="状态：帧 N+2 · a 已提交 #9 + blit，b 阻塞等 fence[A]（挂在 cmb N 末尾）"),
        links=[("fence", "gfx0", "cpu", "fence[A]"), ("ticket", "up1", "gfx4", "#9"),
               ("rf", "gfx0", "gfx1", "render_finished"), ("order", "gfx0", "gfx2", "队列顺序")],
    ),
    dict(
        title="时刻 ③ 大批没拷完：图形队列整条冻结，GPU 图形单元空转，屏幕停在帧 N+1",
        accent="#dc2626",
        note="cmb N+1 跑完、present 已出队，队首轮到 blit——它等的 #9 还在拷。cmb N+2、N+3 的快照都是 10，等待阶段 ALL_COMMANDS 挡在第一条命令前，"
             "连 ② compute 都动不了。CPU 已提交 N+3，接着在 N+4 停在 fence[A]——它挂在还没开跑的 cmb N+2 末尾。三张图全被占：1 显示 + 2 交给冻结的 cmb。",
        screen=[("img0", "app", "已交给帧 N+3", "cmb N+3 冻结中"),
                ("img1", "scan", "扫描显示中", "屏幕上 = 帧 N+1 的画面（停住）"),
                ("img2", "app", "已交给帧 N+2", "cmb N+2 冻结中")],
        gfx=[
            dict(kind="blit", state="frozen", what="槽1 blit_cb · 15 张 × 逐级 blit + 收尾屏障",
                 waits=[("票据≥9 @ALL", "no")], sigs=[("timeline = 10", "sig")],
                 block=("被 #9 卡：@ALL_COMMANDS 等 timeline——队首堵死", "TICKET"),
                 status="状态：队首 · 等 #9——图形单元空转"),
            dict(kind="frame", frame="N+2", slot="A", img=2, state="frozen", prog=("frozen", 0, 0),
                 waits=[("票据≥10 @ALL", "no"), ("img_avail[A] @COLOR_OUT", "ok")],
                 sigs=[("render_finished[img2]", "sig"), ("fence[A]", "sig")],
                 block=("被 #10 卡：@ALL_COMMANDS——快照 10 含续批 #10", "TICKET"),
                 status="状态：冻结 · ① 都没开始"),
            dict(kind="present", frame="N+2", img=2, state="queued", status="排队"),
            dict(kind="frame", frame="N+3", slot="B", img=0, state="frozen", prog=("frozen", 0, 0),
                 waits=[("票据≥10 @ALL", "no"), ("img_avail[B] @COLOR_OUT", "ok")],
                 sigs=[("render_finished[img0]", "sig"), ("fence[B]", "sig")],
                 block=("被 #10 卡：@ALL_COMMANDS——无新上传也等 10", "TICKET"),
                 status="状态：冻结 · ① 都没开始（同 N+2）"),
            dict(kind="present", frame="N+3", img=0, state="queued", status="排队"),
        ],
        up=[dict(state="run", ticket=9, slot=1, what="帧 N+2 · 15 张贴图 mip0 + mesh · 大批", frac=0.72,
                 status="状态：拷贝中 72% · 整条图形队列都在等它")],
        up_note="帧 N+3 无新资产 = 空批，没往 Q1 提交任何东西",
        ledger=[("槽0", "#8 · 已到，可复用"), ("槽1", "#10 · 拷贝 + blit 都没完")],
        cpu=dict(frame="N+4", step="b", status="状态：帧 N+4 · b 阻塞等 fence[A]——挂在冻结的 cmb N+2 末尾，卡顿传回 CPU"),
        links=[("fence", "gfx1", "cpu", "fence[A]"), ("ticket", "up0", "gfx0", "#9"),
               ("ticket10", "gfx0", "gfx1", "#10"), ("ticket10", "gfx0", "gfx3", "#10")],
    ),
    dict(
        title="时刻 ④ 解冻：#9 到 → blit 跑完发 #10 → cmb N+2 开跑，后面整串依次放行",
        accent="#059669",
        note="票据是单调计数：#10 一到，N+2、N+3 两个等 ≥10 的提交同时满足，只剩队列顺序。staging 槽1 记账已换成 #10（续批发布的票据），"
             "下次复用槽1 要等的是 #10 而不是 #9——mip 链生成完之前 staging 不能被覆写。",
        screen=[("img0", "app", "已交给帧 N+3", "cmb N+3 排队中"),
                ("img1", "scan", "扫描显示中", "屏幕上 = 帧 N+1 的画面"),
                ("img2", "app", "已交给帧 N+2", "cmb N+2 正在跑（④ 起写）")],
        gfx=[
            dict(kind="blit", state="done", what="槽1 blit_cb · 15 张 × 逐级 blit + 收尾屏障",
                 waits=[("票据≥9 @ALL", "ok")], sigs=[("timeline = 10", "sig")],
                 status="已完成 · 已置位 10"),
            dict(kind="frame", frame="N+2", slot="A", img=2, state="run", prog=("run", 1, 0.5),
                 waits=[("票据≥10 @ALL", "ok"), ("img_avail[A] @COLOR_OUT", "ok")],
                 sigs=[("render_finished[img2]", "sig"), ("fence[A]", "sig")],
                 block=("无卡点：T≥10 已到，A[B] 已置位，执行中", "NONE"),
                 status="状态：执行中 · ② LOD 选档 c9"),
            dict(kind="present", frame="N+2", img=2, state="queued", status="排队"),
            dict(kind="frame", frame="N+3", slot="B", img=0, state="queued", prog=("queued", 0, 0),
                 waits=[("票据≥10 @ALL", "ok"), ("img_avail[B] @COLOR_OUT", "ok")],
                 sigs=[("render_finished[img0]", "sig"), ("fence[B]", "sig")],
                 block=("无同步卡点：T≥10 / A[B] 均已置位，只差队列顺序", "ORDER"),
                 status="状态：排队 · 门全开，只等队列顺序"),
            dict(kind="present", frame="N+3", img=0, state="queued", status="排队"),
        ],
        up=[dict(state="done", ticket=9, slot=1, what="帧 N+2 · 大批", frac=1.0,
                 status="已执行完 · 出队")],
        up_note="Q1 已空",
        ledger=[("槽0", "#8 · 已到，可复用"), ("槽1", "#10 · 已到，可复用")],
        cpu=dict(frame="N+4", step="b", status="状态：帧 N+4 · 仍在 b 等 fence[A]——要等 cmb N+2 整条跑完"),
        links=[("fence", "gfx1", "cpu", "fence[A]"), ("ticket10", "gfx0", "gfx1", "#10"),
               ("order", "gfx1", "gfx3", "队列顺序")],
    ),
    dict(
        title="时刻 ⑤ 另一情形（FIFO 回退 / 显示节拍受限）：cmb 跑完 ②③，在 ④ 前等图片准备好",
        accent="#9333ea",
        note="acquire 交出图编号不等于演示引擎已放手——image_available 要到换页/合成读完才置位。等待阶段 COLOR_ATTACHMENT_OUTPUT："
             "② compute、③ 阴影不写颜色附件，先跑完；④ G 缓冲是全帧第一次写颜色（哪怕写的不是交换链图）就被拦。"
             "本工程默认 MAILBOX 三图时少见；驱动若在 acquire 里直接阻塞，则卡点挪到 CPU 的 c 段。",
        screen=[("img0", "scan", "扫描显示中", "屏幕上 = 帧 K-2 的画面"),
                ("img1", "queue", "排队等 vblank", "帧 K-1 已画完，FIFO 排队待换页"),
                ("img2", "hold", "编号已交帧 K", "演示引擎还没放手 → 信号量未置位")],
        gfx=[
            dict(kind="frame", frame="K-1", slot="B", img=1, state="done", prog=("done", 0, 0),
                 waits=[("票据 @ALL", "ok"), ("img_avail[B]", "ok")],
                 sigs=[("render_finished[img1]", "sig"), ("fence[B]", "sig")],
                 status="已完成"),
            dict(kind="present", frame="K-1", img=1, state="done", status="已出队"),
            dict(kind="frame", frame="K", slot="A", img=2, state="run", prog=("gated", 3, 0),
                 waits=[("票据 @ALL", "ok"), ("img_avail[A] @COLOR_OUT", "no")],
                 sigs=[("render_finished[img2]", "sig"), ("fence[A]", "sig")],
                 block=("被 image_available[A] 卡：@COLOR_ATTACHMENT_OUTPUT", "IMG"),
                 status="状态：等待图片准备好 · 停在 ④ 前"),
            dict(kind="present", frame="K", img=2, state="queued", status="排队"),
        ],
        up=[],
        up_note="Q1 空：这几帧无新资产",
        ledger=[("槽0", "可复用"), ("槽1", "可复用")],
        cpu=dict(frame="K+1", step="c", status="状态：帧 K+1 · fence[B] 已过，c acquire 阻塞——三张图 = 1 显示 + 1 排队 + 1 被帧 K 持有"),
        links=[("img", "scr2", "gfx2", "image_available[A] 未置位"), ("rf", "gfx0", "gfx1", "render_finished")],
    ),
]

# ---------------------------------------------------------------- 版面

LABEL_W = 230
X0, X1 = 250, W - 30
HEAD_H, SCR_H, GFX_H, UP_H, CPU_H, NOTE_H = 34, 86, 152, 100, 60, 52
PANEL_H = HEAD_H + SCR_H + GFX_H + UP_H + CPU_H + NOTE_H + 18
TOP = 192
H = TOP + PANEL_H * len(MOMENTS) + 30 * len(MOMENTS) + 40


def lane_label(y, h, title, sub, color="#0f172a"):
    text(LABEL_W - 14, y + h / 2 - 9, title, 13, "end", "bold", color)
    text(LABEL_W - 14, y + h / 2 + 10, sub, 10, "end", fill="#64748b")


def draw_moment(m, top):
    accent = m["accent"]
    rect(16, top, W - 32, PANEL_H, "#ffffff", "#c3cedd", rx=10, sw=1.2, extra_attr=' filter="url(#soft)"')
    rect(21, top + 14, 6, PANEL_H - 28, accent, "none", rx=3, sw=0)
    rect(16, top, W - 32, HEAD_H, "url(#hdrgrad)", "#0f172a", rx=8)
    rect(16, top + HEAD_H - 8, W - 32, 8, "url(#hdrgrad)", "#0f172a", rx=0)
    rect(16, top + HEAD_H - 3, W - 32, 3, accent, "none", rx=0, sw=0)
    text(40, top + HEAD_H / 2, m["title"], 15, "start", "bold", "#ffffff")

    y_scr = top + HEAD_H
    y_gfx = y_scr + SCR_H
    y_up = y_gfx + GFX_H
    y_cpu = y_up + UP_H
    y_note = y_cpu + CPU_H
    for y in (y_gfx, y_up, y_cpu, y_note):
        line(30, y, X1 + 10, y, "#e2e8f0", 1)
    line(LABEL_W + 4, y_scr + 6, LABEL_W + 4, y_note - 4, "#94a3b8", 1.4)
    rect(LABEL_W + 6, y_gfx + 1, X1 - LABEL_W, GFX_H - 2, "#f8fafc", "none", rx=0)
    rect(LABEL_W + 6, y_up + 1, X1 - LABEL_W, UP_H - 2, "#fffdf5", "none", rx=0)

    lane_label(y_scr, SCR_H, "演示引擎 · 交换链", "3 张图 · MAILBOX 优先")
    lane_label(y_gfx, GFX_H, "图形队列 Q0", "族 0 · GRAPHICS+COMPUTE+PRESENT")
    lane_label(y_up, UP_H, "传输队列 Q1", "族 1 · 专用 transfer · 单队列")
    lane_label(y_cpu, CPU_H, "CPU 主线程", "draw_frame（Bevy Last）")

    anchors = {}
    under = len(parts)  # 交接箭头插到队列项之下，穿过别的泳道时被框体遮住

    # 交换链三张图
    scr_style = {"scan": ("#dbeafe", "#1d4ed8"), "app": ("#f1f5f9", "#475569"),
                 "queue": ("#ccfbf1", "#0f766e"), "hold": ("#f3e8ff", IMG)}
    cw = (X1 - X0 - 40) / 3
    for i, (name, st, badge, desc) in enumerate(m["screen"]):
        cx = X0 + i * (cw + 20)
        bg, fg = scr_style[st]
        rect(cx, y_scr + 10, cw, SCR_H - 20, bg, fg, sw=1.6 if st == "scan" else 1.0,
             extra_attr=' filter="url(#soft)"')
        if st == "scan":
            rect(cx + 10, y_scr + 18, 70, SCR_H - 36, "#1e3a8a", "#1e3a8a", rx=3)
            text(cx + 45, y_scr + SCR_H / 2, "▶ 屏幕", 11, fill="#ffffff", weight="bold")
            tx = cx + 92
        else:
            tx = cx + 14
        text(tx, y_scr + SCR_H / 2 - 12, f"{name} · {badge}", 13, "start", "bold", fg)
        text(tx, y_scr + SCR_H / 2 + 10, desc, 11, "start", fill="#334155")
        anchors[f"scr{i}"] = (cx + cw / 2, y_scr + SCR_H - 10)

    # 图形队列：右 = 队首（先提交 / 正在执行），左 = 队尾（后提交）
    text(X1 - 4, y_gfx + 9, "队首（先提交）→ GPU", 10, "end", "bold", "#64748b")
    text(X0 + 4, y_gfx + 9, "← 队尾（后提交）", 10, "start", "bold", "#64748b")
    x = X1 - 6
    iy, ih = y_gfx + 18, GFX_H - 26
    for k, it in enumerate(m["gfx"]):
        w = ITEM_W[it["kind"]]
        x -= w
        DRAW[it["kind"]](it, x, iy, w, ih)
        anchors[f"gfx{k}"] = (x, iy, w, ih)
        x -= 16

    # 传输队列：一条队列，按提交顺序排（右 = 队首）；槽号只是批次借用的 staging 内存
    text(X1 - 4, y_up + 9, "队首（先提交）→ 拷贝引擎", 10, "end", "bold", "#64748b")
    uw, ux = 470, X1 - 6
    for k, it in enumerate(m["up"]):
        ux -= uw
        box_upload(it, ux, y_up + 22, uw, 58)
        anchors[f"up{k}"] = (ux, y_up + 22, uw, 58)
        ux -= 16
    if m.get("up_note"):
        text(ux - 10, y_up + UP_H / 2 + 5, m["up_note"], 11, "end", fill="#64748b")
    # staging 槽记账：CPU 侧账本，不是队列
    lx, ly = X0 + 4, y_up + 30
    text(lx, ly, "staging 槽记账（CPU 侧账本，不是队列；每槽记最后一批的票据）", 10, "start", "bold", "#92400e")
    for k, (slot, state) in enumerate(m["ledger"]):
        cy = ly + 22 + k * 22
        rect(lx, cy - 9, 46, 18, "#fef3c7", "#d97706", rx=9, sw=0.8)
        text(lx + 23, cy, slot, 10.5, fill="#92400e", weight="bold")
        text(lx + 54, cy, state, 10.5, "start", fill="#334155")

    # CPU：draw_frame 步骤条
    c = m["cpu"]
    tot = sum(s[2] for s in CPU_STEPS)
    bx, bw = X0 + 110, 760
    text(X0, y_cpu + CPU_H / 2, f"帧 {c['frame']}", 13, "start", "bold", FC[c["frame"]])
    cx = bx
    hit = None
    passed = True
    for letter, name, d in CPU_STEPS:
        sw_ = bw * d / tot
        cur = letter == c["step"]
        if cur:
            passed = False
            hit = (cx, sw_)
        fill = "#fee2e2" if cur else ("#e2e8f0" if passed else "#ffffff")
        rect(cx, y_cpu + 14, sw_, CPU_H - 28, fill, FENCE if cur else "#94a3b8",
             sw=2 if cur else 0.6, rx=3, extra_attr=' filter="url(#soft)"' if cur else "")
        text(cx + sw_ / 2, y_cpu + CPU_H / 2, f"{letter} {name}" if sw_ > 48 else letter, 10,
             fill=FENCE if cur else "#334155", weight="bold" if cur else "normal")
        cx += sw_
    text(bx + bw + 16, y_cpu + CPU_H / 2, c["status"], 11.5, "start", "bold", STATUS)
    anchors["cpu"] = (hit[0], y_cpu + 14, hit[1], CPU_H - 28)

    # 读图说明：浅底块 + 时刻强调色左条
    rect(26, y_note + 5, W - 52, NOTE_H - 10, "#f8fafc", "none", rx=6, sw=0)
    rect(26, y_note + 5, 4, NOTE_H - 10, accent, "none", rx=2, sw=0)
    text(46, y_note + NOTE_H / 2 + 2, m["note"], 11.5, "start", fill="#334155")

    # 交接箭头
    mark = len(parts)
    for kind, a, b, label in m["links"]:
        ax, ay, aw, ah = anchors[a] if len(anchors[a]) == 4 else (*anchors[a], 0, 0)
        if kind == "fence":
            bx_, by_, bw_, _ = anchors[b]
            p1 = (ax + aw - 40, ay + ah)
            p2 = (bx_ + bw_ / 2, by_)
            curve(p1, p2, FENCE, label, ly=p2[1] - 22, lx=p2[0] + 60)
        elif kind == "rf":
            bx_, by_, bw_, _ = anchors[b]
            arc_over(ax + 30, bx_ + bw_ - 20, ay + 2, RFIN, label)
        elif kind == "order":
            bx_, by_, bw_, _ = anchors[b]
            # 队列顺序不是同步原语，画成虚线拱以区别于四类真原语；拱高控制在演示引擎泳道之下
            arc_over(ax + 30, bx_ + bw_ - 20, ay + 2, ORDER, label, h=26,
                     lx=(ax + bx_ + bw_) / 2 - 90, dash="4 3")
        elif kind in ("ticket", "ticket10"):
            bx_, by_, bw_, bh_ = anchors[b]
            if kind == "ticket":
                p1 = (ax + 60, ay)
                p2 = (bx_ + 40, by_ + bh_)
                curve(p1, p2, TICKET, label, sw=2, lx=p2[0] + 40, ly=p2[1] + 14)
            else:
                arc_over(ax + 40, bx_ + bw_ - 40, ay + 2, TICKET, label, h=18)
        elif kind == "img":
            bx_, by_, bw_, _ = anchors[b]
            gx = bx_ + 8 + (bw_ - 16) * sum(s[2] for s in SEGS[:3]) / SEG_TOTAL
            card_x0 = X0 + int(a[3:]) * (cw + 20)
            sx = min(max(gx, card_x0 + 30), card_x0 + cw - 30)
            curve((sx, ay), (gx, by_ - 1), IMG, label, sw=2, dash="5 3", ly=ay + 17, lx=min(sx, gx) - 150)
    links = parts[mark:]
    del parts[mark:]
    parts[under:under] = links
    parts.extend(labels)
    labels.clear()


def legend(y):
    items = [
        (FENCE, "fence[帧槽]：GPU 跑完整条 cmb → 放行 CPU 复用该帧槽（wait_for_slot）"),
        (TICKET, "timeline 票据：transfer / blit 完成 → 放行等 ≥N 的提交（@ALL_COMMANDS，整批）"),
        (IMG, "image_available[帧槽]：演示引擎放手图 → 放行 @COLOR_ATTACHMENT_OUTPUT"),
        (RFIN, "render_finished[图]：cmb 写完交换链图 → 放行 present"),
    ]
    for k, (color, s) in enumerate(items):
        x, yy = 30 + (k % 2) * 980, y + (k // 2) * 22
        line(x, yy, x + 34, yy, color, 2.2, arrow=True)
        text(x + 42, yy, s, 11.5, "start", fill="#0f172a")
    y2 = y + 50
    x = 30
    samples = [("执行中", "#ffffff", "#2563eb", None, 2.6), ("已提交排队", "#ffffff", "#2563eb", "5 3", 1.6),
               ("冻结（等信号量）", "#f8fafc", FENCE, "6 3", 2), ("已完成（淡显）", "#f8fafc", "#cbd5e1", None, 1)]
    for name, bg, st, dash, sw in samples:
        rect(x, y2 - 9, 34, 18, bg, st, dash=dash, sw=sw)
        text(x + 42, y2, name, 11.5, "start")
        x += 42 + len(name) * 12 + 30
    seg_bar(x, y2 - 7, 260, 14, ("run", 3, 0.5))
    text(x + 270, y2, "九段进度：彩 = 已跑 · 黑框 = 当前段 · 白 = 未跑 · 斜线 = 冻结 · 紫竖线 = 停在此段前", 11.5, "start")
    y3 = y + 74
    text(30, y3, "框底横条 = 这条 cmb 的卡点：", 11.5, "start", "bold")
    x = 30 + 220
    for key, s in [("NONE", "无卡点 · 执行中"), ("ORDER", "只等队列顺序（非同步原语）"),
                   ("TICKET", "等票据 T：@ALL_COMMANDS 整批挡住"), ("IMG", "等图 A：@COLOR_ATTACHMENT_OUTPUT")]:
        bg, fg = BLOCK_STYLE[key]
        rect(x, y3 - 9, 24, 17, bg, fg, rx=3, sw=0.8)
        text(x + 32, y3, s, 11, "start")
        x += 32 + len(s) * 11.8 + 30


parts_head = [
    f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" '
    f'font-family="{FONT}">',
    "<defs>",
    '<pattern id="hatch" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)">'
    '<rect width="6" height="6" fill="#e2e8f0"/><line x1="0" y1="0" x2="0" y2="6" stroke="#94a3b8" stroke-width="2"/></pattern>',
    '<linearGradient id="bggrad" x1="0" y1="0" x2="0" y2="1">'
    '<stop offset="0" stop-color="#eef2f7"/><stop offset="1" stop-color="#e2e9f2"/></linearGradient>',
    '<linearGradient id="hdrgrad" x1="0" y1="0" x2="1" y2="0">'
    '<stop offset="0" stop-color="#0f172a"/><stop offset="1" stop-color="#1f3a58"/></linearGradient>',
    '<filter id="soft" x="-15%" y="-30%" width="130%" height="170%">'
    '<feDropShadow dx="0" dy="1.5" stdDeviation="2.2" flood-color="#1e293b" flood-opacity="0.18"/></filter>',
    '<filter id="lift" x="-15%" y="-30%" width="130%" height="170%">'
    '<feDropShadow dx="0" dy="2.5" stdDeviation="3.2" flood-color="#1e293b" flood-opacity="0.25"/></filter>',
]
for color in ARROW_COLORS:
    parts_head.append(
        f'<marker id="ah-{color[1:]}" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" '
        f'orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" fill="{color}"/></marker>'
    )
parts_head.append("</defs>")
parts_head.append(f'<rect width="{W}" height="{H}" fill="url(#bggrad)"/>')

text(30, 30, "设备队列在飞快照：冻住 GPU 截五个典型时刻，看每条 cmb 排在哪、跑到哪、被哪道门以什么理由卡住", 22, "start", "bold")
text(30, 58, "ash_renderer · RTX 2060 实测队列拓扑 · MAX_FRAMES_IN_FLIGHT = 2 · 交换链 3 图 · staging 2 槽 · "
     "帧/槽/图/票据编号与《多帧在飞竞争》§3 时序图同一稳态推演（时刻 ⑤ 为另一情形）", 12.5, "start", fill="#475569")
legend(86)

for i, m in enumerate(MOMENTS):
    draw_moment(m, TOP + i * (PANEL_H + 30))

with open(OUT, "w", encoding="utf-8", newline="\n") as f:
    f.write("\n".join(parts_head + parts + ["</svg>"]))
print(OUT, W, H)
