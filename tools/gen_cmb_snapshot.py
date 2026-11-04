"""多帧在飞同刻快照图生成器。

用途：生成笔记《多帧在飞竞争》的配图 cmb-queue-snapshots.svg——从主图
frame-cmd-timeline.svg 的时间轴上竖切四个典型时刻，每张快照画出四个异步主体
（CPU / 传输队列 / 图形队列 / 演示引擎）此刻手里的活：属于哪帧、哪个槽、
执行到九段中哪段、被谁卡住；底部给出四道闸门的卡/不卡状态与卡顿链。
调用：python tools/gen_cmb_snapshot.py [输出 svg 路径]
帧/票据/图编号与 gen_frame_cmd_timeline.py 的同一稳态推演保持一致；
推演或资源归属改动后两份脚本要同步更新。
"""

import sys
from html import escape

OUT = sys.argv[1] if len(sys.argv) > 1 else ".agents/docs/笔记/Vulkan/_assets/cmb-queue-snapshots.svg"
FONT = "Microsoft YaHei, PingFang SC, Noto Sans CJK SC, sans-serif"

W = 1780
parts = []

RED, AMBER, PURPLE, GRAY = "#dc2626", "#d97706", "#9333ea", "#64748b"
ARROW_COLORS = [RED, AMBER, PURPLE, GRAY]
FC = {"N-1": "#64748b", "N": "#2563eb", "N+1": "#059669", "N+2": "#d97706", "N+3": "#7c3aed"}
# 九段（名, 填色）与主图 SEGS 一致
SEGS = ["①", "②", "③", "④", "⑤", "⑥", "⑦", "⑧", "⑨"]
SEG_FILL = ["#e5e7eb", "#ffedd5", "#e7e5e4", "#dcfce7", "#fef9c3", "#fce7f3", "#ede9fe", "#e0f2fe", "#e5e7eb"]

STATUS_STYLE = {
    "卡住": ("#fee2e2", "#b91c1c"),
    "未卡": ("#dcfce7", "#15803d"),
    "不卡": ("#dcfce7", "#15803d"),
    "未到": ("#f1f5f9", "#475569"),
    "连带": ("#fef3c7", "#b45309"),
    "预告": ("#fef3c7", "#b45309"),
}


def rect(x, y, w, h, fill, stroke="#475569", dash=None, rx=3, sw=1.0):
    extra = f' stroke-dasharray="{dash}"' if dash else ""
    parts.append(
        f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h:.1f}" rx="{rx}" '
        f'fill="{fill}" stroke="{stroke}" stroke-width="{sw}"{extra}/>'
    )


def text(x, y, s, size=11, anchor="middle", weight="normal", fill="#0f172a", vcenter=True):
    lines = s.split("\n")
    lh = size * 1.22
    y0 = y - (len(lines) - 1) * lh / 2 + size * 0.36 if vcenter else y
    spans = "".join(
        f'<tspan x="{x:.1f}" y="{y0 + i * lh:.1f}">{escape(line)}</tspan>' for i, line in enumerate(lines)
    )
    parts.append(
        f'<text font-size="{size}" text-anchor="{anchor}" font-weight="{weight}" fill="{fill}">{spans}</text>'
    )


def hline(x1, y1, x2, y2, color, sw=1.0, dash=None, arrow=False):
    extra = f' stroke-dasharray="{dash}"' if dash else ""
    if arrow:
        extra += f' marker-end="url(#ah-{color[1:]})"'
    parts.append(f'<line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" stroke="{color}" stroke-width="{sw}"{extra}/>')


def elbow(x1, y1, x2, y2, ty, color, sw=1.8, dash=None):
    """跨容器长箭头：从 (x1,y1) 上行到 ty、横穿、下插到 (x2,y2)，末端箭头向下。"""
    extra = f' stroke-dasharray="{dash}"' if dash else ""
    parts.append(
        f'<path d="M{x1:.1f},{y1:.1f} L{x1:.1f},{ty:.1f} L{x2:.1f},{ty:.1f} L{x2:.1f},{y2:.1f}" '
        f'fill="none" stroke="{color}" stroke-width="{sw}"{extra} marker-end="url(#ah-{color[1:]})"/>'
    )


def box(x, y, w, h, color, title, lines, dashed=False, alert=False, fill=None, bar=None):
    """一格 = 一个主体手里的一份活。alert=此刻被卡；bar=九段执行进度条。"""
    f = fill or ("#fef2f2" if alert else "#ffffff")
    stroke = RED if alert else color
    rect(x, y, w, h, f, stroke, "5 3" if dashed else None, rx=5, sw=1.8 if alert else 1.4)
    text(x + 10, y + 16, title, 10.5, "start", "bold", stroke)
    ly = y + 31
    for s in lines:
        text(x + 10, ly, s, 9.5, "start", fill="#334155", vcenter=False)
        ly += 13
    if bar is not None:
        cw = (w - 16 - 8 * 2) / 9
        by, bh = y + h - 17, 13
        for i, num in enumerate(SEGS):
            if bar == "frozen":
                fill_c, tcolor, weight = "#e2e8f0", "#94a3b8", "normal"
            elif i < bar:
                fill_c, tcolor, weight = "#e2e8f0", "#94a3b8", "normal"
            elif i == bar:
                fill_c, tcolor, weight = SEG_FILL[i], "#0f172a", "bold"
            else:
                fill_c, tcolor, weight = "#ffffff", "#cbd5e1", "normal"
            cx = x + 8 + i * (cw + 2)
            rect(cx, by, cw, bh, fill_c, "#94a3b8", rx=2, sw=0.8)
            text(cx + cw / 2, by + bh / 2, num, 8.5, weight=weight, fill=tcolor)


def chip(x, y, w, label, status, note):
    fill, color = STATUS_STYLE[status]
    rect(x, y, w, 22, fill, color, rx=11, sw=1.2)
    text(x + w / 2, y + 11, f"{label} · {status}：{note}", 10, weight="bold", fill=color)


SUBJECTS = [  # (key, 名, x, w, 头部填色, 头部描边)
    ("cpu", "CPU · Bevy Last 调度（主线程串行）", 24, 290, "#dbeafe", "#2563eb"),
    ("tr", "传输队列（上传批）", 424, 290, "#fef3c7", "#d97706"),
    ("gfx", "图形队列（帧命令缓冲）", 824, 500, "#dcfce7", "#16a34a"),
    ("pr", "演示引擎（持交换链图）", 1434, 320, "#f3e8ff", "#9333ea"),
]
SUB_X = {k: (x, w) for k, _, x, w, _, _ in SUBJECTS}

CT_H = 170  # 容器总高（含头部 22）
BOX_H = 64


def render_panel(py, p):
    rect(12, py, W - 24, 306, "#f8fafc", "#cbd5e1", rx=6)
    text(26, py + 24, p["title"], 14.5, "start", "bold")
    text(26, py + 44, p["sub"], 11, "start", fill="#475569", vcenter=False)
    ctop = py + 74
    box_tops = {}
    for key, name, x, w, hfill, hstroke in SUBJECTS:
        rect(x, ctop, w, 22, hfill, hstroke, rx=4, sw=1.2)
        parts.append(f'<rect x="{x}" y="{ctop + 22}" width="{w}" height="{CT_H - 22}" fill="#ffffff" stroke="{hstroke}" stroke-width="1.0"/>')
        text(x + w / 2, ctop + 11, name, 11, weight="bold", fill=hstroke)
        box_tops[key] = []
        by = ctop + 30
        for b in p["boxes"].get(key, []):
            box(x + 8, by, w - 16, BOX_H, FC[b["frame"]], b["title"], b["lines"],
                dashed=b.get("dashed", False), alert=b.get("alert", False), fill=b.get("fill"), bar=b.get("bar"))
            box_tops[key].append(by)
            by += BOX_H + 10
    # 闸门状态条
    chips = p["gates"]
    cw = (W - 48 - 3 * 14) / 4
    for i, (label, status, note) in enumerate(chips):
        chip(24 + i * (cw + 14), py + 256, cw, label, status, note)
    # 卡顿链
    label, body = p["chain"]
    text(34, py + 292, f"{label}：", 11.5, "start", "bold", fill="#0f172a", vcenter=False)
    text(34 + 11.5 * (len(label) + 1), py + 292, body, 11.5, "start", fill="#334155", vcenter=False)
    # 箭头
    for a in p["arrows"]:
        kind = a["kind"]
        if kind == "h":  # 相邻容器之间的水平箭头，标签挂在箭头下方
            sx, sw_ = SUB_X[a["src"]]
            dx, dw = SUB_X[a["dst"]]
            y = box_tops[a["src"]][a["sbox"]] + BOX_H / 2
            hline(sx + sw_ + 2, y, dx - 3, y, a["color"], a.get("sw", 1.6), a.get("dash"), arrow=True)
            ty = y + 16
            for s in a["labels"]:
                text((sx + sw_ + dx) / 2, ty, s, 9, fill=a["color"], weight="bold" if a.get("bold") else "normal")
                ty += 12
        elif kind == "elbow":  # 跨容器箭头，从源盒顶上行、横穿、插入目标盒顶
            sx, sw_ = SUB_X[a["src"]]
            dx, _ = SUB_X[a["dst"]]
            x1 = sx + sw_ / 2 if a.get("anchor") != "left" else sx + 60
            y1 = ctop + 30 + a["sbox"] * (BOX_H + 10)
            x2 = dx + (SUB_X[a["dst"]][1]) / 2
            elbow(x1, y1, x2, y1 + 2, py + 62, a["color"], a.get("sw", 1.8), a.get("dash"))
            text((x1 + x2) / 2, py + 57, a["label"], 9.5, weight="bold", fill=a["color"])


# ---------------------------------------------------------------- 四个典型时刻

PANELS = [
    dict(
        title="时刻 ① · 稳态冲刺相位 —— 无卡顿窗口（在飞红利）",
        sub="截取位置 ≈ 主图稳态窗：帧 N 执行 ④ 段的一瞬 · CPU 正录帧 N+1 · 传输与呈现各飞各的",
        boxes=dict(
            cpu=[dict(frame="N+1", title="帧 N+1 · 槽 B · 图 1",
                      lines=["draw_frame j 段：录制 cmb[槽B]", "cmb[槽B] = 帧 N-1 用毕，fence[B] 已置位才可重录"])],
            tr=[dict(frame="N+1", title="上传批 #8 · 帧 N+1 的料",
                     lines=["拷贝 → 池 / 对象表 / 贴图（中批）", "完成置票据 8"])],
            gfx=[dict(frame="N", title="cmb[槽A] · 帧 N · 图 0",
                      lines=["执行中：④ G缓冲（首个写颜色的段）"], bar=3)],
            pr=[dict(frame="N", title="持有 图 0 的 present 请求 · 帧 N",
                     lines=["等 render_finished[0]（帧 N 执行完置位）", "屏幕显示：图 2（帧 N-1 画面）"])],
        ),
        gates=[("A fence", "未卡", "fence[B] 已置位（帧 N-1 早执行完）"),
               ("B 票据", "未到", "帧 N 的票据 7 已完成；批 #8 在飞但 N+1 未提交"),
               ("C 图门", "未到", "image_available 提交前已置位，④ 畅通"),
               ("D 上传槽", "未卡", "staging 槽空闲")],
        chain=("并行余量", "此刻无任何闸门拉扯：CPU 领先 GPU 一帧冲刺备料，传输 / 图形 / 呈现各飞各的——帧槽 2 × 图 3 × 上传槽 2 三套账本互不整除买来的错位并行。"),
        arrows=[
            dict(kind="h", src="tr", dst="gfx", sbox=0, color=AMBER, dash="4 3",
                 labels=["票据 8（预告）", "N+1 开跑前等"]),
            dict(kind="h", src="gfx", dst="pr", sbox=0, color=PURPLE, dash="4 3",
                 labels=["render_finished[0]", "未到（④ 后置位）"]),
        ],
    ),
    dict(
        title="时刻 ② · 稳态 fence 相位 —— 闸门 A：GPU 卡 CPU",
        sub="截取位置 ≈ 主图稳态窗：帧 N 执行 ⑦ 段的一瞬 · CPU 已追到帧 N+2，被同槽 fence 拉回节拍",
        boxes=dict(
            cpu=[dict(frame="N+2", title="帧 N+2 · 槽 A · 图 2",
                      lines=["b 段 wait_for_slot：等 fence[A]", "fence[A] 由帧 N 执行完置位——还没执行完"], alert=True)],
            tr=[dict(frame="N+1", title="空闲",
                     lines=["批 #8 已完成，票据 8 已置位"])],
            gfx=[dict(frame="N", title="cmb[槽A] · 帧 N · 图 0",
                      lines=["执行中：⑦ Bloom"], bar=6),
                 dict(frame="N+1", title="cmb[槽B] · 帧 N+1 · 图 1",
                      lines=["已提交 · 排队（同队列提交序）", "前帧执行完自动接上，不等 fence"], dashed=True)],
            pr=[dict(frame="N", title="持有 图 0 的 present 请求 · 帧 N",
                     lines=["等 render_finished[0]", "屏幕显示：图 2（帧 N-1 画面）"])],
        ),
        gates=[("A fence", "卡住", "CPU 停 b 段，卡到帧 N 执行完为止"),
               ("B 票据", "未卡", "票据 8 已完成"),
               ("C 图门", "未到", "帧 N 已过 ④；帧 N+1 还在排队"),
               ("D 上传槽", "未卡", "无在飞批")],
        chain=("卡顿链", "帧 N 执行完 → fence[A] 置位 → CPU 解锁继续录 N+2。帧 N+1 不受影响：同队列提交序保证前帧一完就自动接上（它只等执行，不等 fence）。"),
        arrows=[
            dict(kind="elbow", src="gfx", dst="cpu", sbox=0, color=RED, sw=2.0,
                 label="fence[A]：帧 N 执行完才置位，CPU 在此干等"),
        ],
    ),
    dict(
        title="时刻 ③ · 大上传批 —— 闸门 B：transfer 卡 graphics，连带演示引擎",
        sub="截取位置 ≈ 主图帧 N+2 冻结窗：批 #9（贴图流送）拷贝中 · 帧 N+1 已执行完离场",
        boxes=dict(
            cpu=[dict(frame="N+3", title="帧 N+3 · 槽 B · 图 0",
                      lines=["d~j 段：备料 + 录制 cmb[槽B]", "fence[B] 已置位（帧 N+1 执行完），不受阻"])],
            tr=[dict(frame="N+2", title="上传批 #9 · 帧 N+2 的料",
                     lines=["贴图流送大批拷贝中（760 刻度）", "完成置票据 9"], fill="#fef3c7")],
            gfx=[dict(frame="N+2", title="cmb[槽A] · 帧 N+2 · 图 2",
                      lines=["整批冻结在票据门（ALL_COMMANDS）", "0 条 cmd 开跑——② compute 也被拦"],
                      alert=True, bar="frozen")],
            pr=[dict(frame="N+2", title="持有 图 2 的 present 请求 · 帧 N+2",
                     lines=["等 render_finished[2]（冻结批次执行完才有）", "屏幕显示：图 1（帧 N+1 画面）"])],
        ),
        gates=[("A fence", "不卡", "fence[B] 已置位，CPU 照常录 N+3"),
               ("B 票据", "卡住", "整批冻结，卡到批 #9 拷完置票据 9"),
               ("C 图门", "连带", "present 挂起等 render_finished[2]"),
               ("D 上传槽", "预告", "此刻若发新批，要等 #9 释放 staging 槽")],
        chain=("卡顿链", "transfer 大批 ─票据 9→ graphics 整帧冻结 ─render_finished[2]→ 演示引擎挂起，屏幕停在帧 N+1——代价 = 图形空转 + 呈现延迟一拍；CPU 照常狂奔，积压反而加深。"),
        arrows=[
            dict(kind="h", src="tr", dst="gfx", sbox=0, color=AMBER, sw=2.2, bold=True,
                 labels=["票据 9", "ALL_COMMANDS", "拦整批"]),
            dict(kind="h", src="gfx", dst="pr", sbox=0, color=PURPLE, dash="4 3",
                 labels=["render_finished[2]", "未到（连带）"]),
        ],
    ),
    dict(
        title="时刻 ④ · 呈现受限 —— 闸门 C：演示引擎反向卡 CPU",
        sub="主图没画的前提：演示引擎消费慢（FIFO 节流 / 刷新率低于渲染速率）· 三张图全未归还",
        boxes=dict(
            cpu=[dict(frame="N+3", title="帧 N+3 · 槽 B · 图 0",
                      lines=["c 段 acquire：等演示引擎还图", "三张图均未归还 → 阻塞，录制 / 提交全停"], alert=True)],
            tr=[dict(frame="N+3", title="空闲",
                     lines=["无待传批"])],
            gfx=[dict(frame="N+2", title="空闲",
                      lines=["积压已跑完：帧 N+2 执行完离场", "render_finished[2] 已 signal"])],
            pr=[dict(frame="N+2", title="呈现中：屏幕显示 图 0（帧 N 画面）",
                     lines=["present 队列排着 图 1、图 2 未消费", "三张图无一归还 → 无图可 acquire"])],
        ),
        gates=[("A fence", "不卡", "各槽 fence 均已置位"),
               ("B 票据", "不卡", "无在飞批"),
               ("C 图门", "卡住", "acquire 等还图，CPU 全停"),
               ("D 上传槽", "不卡", "无在飞批")],
        chain=("卡顿链", "三张图全在演示引擎手里 → acquire 阻塞 → 录制 / 提交全停 → 图形队列饿死空转。GPU 侧 ④ 前的图门在正常时序下提交前已置位、不实际停顿——它兜的是异常时序，真实卡点在 acquire。"),
        arrows=[
            dict(kind="elbow", src="pr", dst="cpu", sbox=0, color=PURPLE, sw=2.0,
                 label="还图才有下一张 image_available → acquire 阻塞"),
        ],
    ),
]

# ---------------------------------------------------------------- 组装

py = 118
rect(12, 14, W - 24, 88, "#ffffff", "#cbd5e1", rx=6)
text(W / 2, 40, "多帧在飞 · 同一时刻快照：四个主体的 cmb 分布与互相卡顿", 22, weight="bold")
text(W / 2, 66, "ash_renderer · 2 帧槽 / 3 交换链图 / 2 上传槽 · 四个典型时刻的竖切面，与 frame-cmd-timeline.svg 的时间轴互为正交视角", 12.5, fill="#475569")
lx = 40
text(lx, 90, "帧色：", 11, "start", weight="bold")
lx += 44
for name in ["N-1", "N", "N+1", "N+2", "N+3"]:
    rect(lx, 82, 14, 14, FC[name], FC[name], rx=3)
    text(lx + 20, 89, f"帧 {name}", 10.5, "start")
    lx += 78
text(lx + 16, 89, "红框 = 此刻被卡 · 虚线框 = 排队 / 挂起 / 预告 · 九段条 = 帧命令缓冲执行到的渲染段（灰 = 已跑完，彩 = 正在跑，白 = 未到）", 10.5, "start", fill="#475569")

for p in PANELS:
    render_panel(py, p)
    py += 322

fy = py + 4
rect(12, fy, W - 24, 64, "#ffffff", "#cbd5e1", rx=6)
text(34, fy + 24, "读法：", 12, "start", "bold", vcenter=False)
text(82, fy + 24, "一格一块 = 该异步主体此刻手里的活。cmb[槽A] / cmb[槽B] 是仅有的两个图形命令缓冲，被相邻帧轮流借用——「帧 N 的 cmb[槽A]」和「帧 N+2 的 cmb[槽A]」是同一物理对象，靠 fence 保证前一个用完。",
     11.5, "start", vcenter=False)
text(82, fy + 46, "帧 / 票据 / 图编号与 frame-cmd-timeline.svg 为同一稳态推演（帧 N-1~N+3、票据 7~9、图 0~2），两张图可对表读；时刻 ④ 是主图没画的前提（呈现受限）。",
     11.5, "start", fill="#475569", vcenter=False)
H = fy + 78

head = [
    f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" font-family="{FONT}">',
    "<defs>",
]
for c in ARROW_COLORS:
    head.append(
        f'<marker id="ah-{c[1:]}" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">'
        f'<path d="M0,0 L10,5 L0,10 z" fill="{c}"/></marker>'
    )
head.append("</defs>")
head.append(f'<rect width="{W}" height="{H}" fill="#ffffff"/>')

with open(OUT, "w", encoding="utf-8", newline="\n") as f:
    f.write("\n".join(head + parts + ["</svg>"]))
print(f"written {OUT} ({W}x{H})")
