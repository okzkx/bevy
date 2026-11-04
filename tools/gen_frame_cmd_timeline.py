"""多帧在飞 cmd 级时序图生成器。

用途：生成笔记《多帧在飞竞争》的配图 frame-cmd-timeline.svg，上半是四帧宏观时序
（CPU / 传输队列 / 图形队列 / 演示引擎四泳道 + fence/票据/image_available/render_finished
四类交接），下半是帧 N 命令缓冲逐 cmd 展开 + 资源读写矩阵。
调用：python tools/gen_frame_cmd_timeline.py [输出 svg 路径]
内容按 ash_renderer 的 draw_frame（driver/host/frame.rs）与 record_frame_steps
（render/frame/frame_draw.rs）手工登记；录制顺序或资源归属改动后同步更新下方各表。
"""

import sys
from html import escape

OUT = sys.argv[1] if len(sys.argv) > 1 else ".agents/docs/笔记/Vulkan/_assets/frame-cmd-timeline.svg"
FONT = "Microsoft YaHei, PingFang SC, Noto Sans CJK SC, sans-serif"

parts = []


def rect(x, y, w, h, fill, stroke="#475569", dash=None, rx=3, sw=1.0, opacity=None):
    extra = f' stroke-dasharray="{dash}"' if dash else ""
    if opacity is not None:
        extra += f' fill-opacity="{opacity}"'
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


def line(x1, y1, x2, y2, color="#475569", sw=1.0, dash=None, arrow=False):
    extra = f' stroke-dasharray="{dash}"' if dash else ""
    if arrow:
        extra += f' marker-end="url(#ah-{color[1:]})"'
    parts.append(f'<line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" stroke="{color}" stroke-width="{sw}"{extra}/>')


def curve(x1, y1, x2, y2, color, sw=1.6, dash=None, bend=0.5):
    extra = f' stroke-dasharray="{dash}"' if dash else ""
    my = y1 + (y2 - y1) * bend
    parts.append(
        f'<path d="M{x1:.1f},{y1:.1f} C{x1:.1f},{my:.1f} {x2:.1f},{my:.1f} {x2:.1f},{y2:.1f}" fill="none" '
        f'stroke="{color}" stroke-width="{sw}"{extra} marker-end="url(#ah-{color[1:]})"/>'
    )


ARROW_COLORS = ["#dc2626", "#d97706", "#9333ea", "#0d9488", "#475569", "#2563eb"]
FENCE, TICKET, IMG, RFIN, GRAY, BLUE = ARROW_COLORS

# ---------------------------------------------------------------- 上半：四帧宏观时序

# 时间单位：抽象刻度（GPU 一帧 560，GPU 受限，队列积压两帧在飞）
G = 560
CPU_PRE = 70  # Last 调度里 draw_frame 之前：驻留 tick + flush_uploads
CPU_POST = [  # wait_for_slot 之后的 draw_frame 各步（字母, 时长）
    ("c", 20), ("d", 40), ("e", 30), ("f", 40), ("g", 50), ("h", 40), ("i", 15), ("j", 105), ("k", 10), ("l", 15)
]
CPU_LETTERS = {
    "a": "驻留 tick + flush_uploads（排空墓地·脏行盘点·合批提交）",
    "b": "wait_for_slot：等本槽 fence（上一轮同槽提交执行完）",
    "c": "acquire：取图，置位本槽 image_available",
    "d": "组装 DrawList（增量）+ 取走奇偶脏集",
    "e": "写本槽 UBO×5（帧/阴影/后处理/分簇/透明计数清零）",
    "f": "写本槽四环（模型增量/材质/在场/LOD 表）",
    "g": "compute 开关：dispatch 备料或宿主直填（视锥/遮挡/LOD/缺口/indirect/阴影）",
    "h": "overlay：图集整传（可能多一张票据）+ 镶嵌 + 顶点环",
    "i": "组装 FrameDraw，取上传票据快照",
    "j": "录制命令缓冲（下半图逐 cmd 展开）",
    "k": "reset fence + queue_submit（等 2 发 2）",
    "l": "present：交出 render_finished[图]，advance 帧槽",
}
SEGS = [  # 图形队列一帧九段（编号, 名, 时长, 填色, 描边）
    ("①", "进场", 15, "#e5e7eb", "#6b7280"),
    ("②", "compute", 90, "#ffedd5", "#ea580c"),
    ("③", "阴影", 70, "#e7e5e4", "#57534e"),
    ("④", "G缓冲", 110, "#dcfce7", "#16a34a"),
    ("⑤", "合成+透明", 80, "#fef9c3", "#ca8a04"),
    ("⑥", "Hi-Z", 40, "#fce7f3", "#db2777"),
    ("⑦", "Bloom", 60, "#ede9fe", "#7c3aed"),
    ("⑧", "后链+UI", 80, "#e0f2fe", "#0284c7"),
    ("⑨", "出场", 15, "#e5e7eb", "#6b7280"),
]
FRAMES = [  # (名, 帧槽, 交换链图, 上传批: None 或 (票据, 时长))
    ("帧 N-1", "B", 2, (7, 110)),
    ("帧 N", "A", 0, None),
    ("帧 N+1", "B", 1, (8, 90)),
    ("帧 N+2", "A", 2, (9, 760)),
]

# 推演：CPU 串行；wait_for_slot 等两帧前同槽 GPU 结束；GPU 等 提交/前帧结束/票据 三者最晚
sched = []
cpu_t = 0
for k, (_, slot, img, upload) in enumerate(FRAMES):
    s = cpu_t
    pre_end = s + CPU_PRE
    up = (pre_end - 5, pre_end - 5 + upload[1], upload[0]) if upload else None
    fence_ready = sched[k - 2]["gpu_end"] if k >= 2 else 0
    wait_end = max(pre_end, fence_ready)
    t = wait_end
    steps = []
    for letter, d in CPU_POST:
        steps.append((letter, t, d))
        t += d
    acquire_end = steps[0][1] + steps[0][2]
    submit = steps[-2][1] + steps[-2][2]
    prev_end = sched[k - 1]["gpu_end"] if k else 0
    ticket_done = up[1] if up else 0
    gpu_start = max(submit, prev_end, ticket_done)
    sched.append(dict(s=s, pre_end=pre_end, wait_end=wait_end, steps=steps, cpu_end=t, up=up,
                      submit=submit, prev_end=prev_end, ticket_done=ticket_done,
                      gpu_start=gpu_start, gpu_end=gpu_start + G, acquire_end=acquire_end))
    cpu_t = t

X0, S = 300, 0.95
W = 3460


def tx(t):
    return X0 + t * S


LANES = {"cpu": (200, 64), "tr": (318, 40), "gpu": (420, 60), "pr": (540, 38)}
LANE_NAMES = {
    "cpu": "CPU · Bevy Last 调度\n（主线程，串行）",
    "tr": "传输队列\n（上传批）",
    "gpu": "图形队列\n（帧命令缓冲）",
    "pr": "演示引擎\n（持有交换链图）",
}

rect(10, 96, W - 20, 560, "#f8fafc", "#cbd5e1", rx=6)
text(24, 116, "上半 · 四帧在飞宏观时序：两帧槽 A/B 交替、三张交换链图轮转、GPU 受限时队列积压一帧", 15, "start", "bold")
for key, (y, h) in LANES.items():
    rect(20, y, 250, h, "#ffffff", "#94a3b8", rx=4)
    text(145, y + h / 2, LANE_NAMES[key], 12, weight="bold")
    line(280, y + h / 2, W - 20, y + h / 2, "#e2e8f0", 1, "4 4")

frame_stroke = lambda k: 2.4 if k == 1 else 1.0  # 帧 N 加粗

for k, (name, slot, img, upload) in enumerate(FRAMES):
    sc = sched[k]
    cy, ch = LANES["cpu"]
    gy, gh = LANES["gpu"]
    # 帧标题
    text(tx(sc["s"]) + 4, cy - 14, f"{name} · 槽 {slot} · 图 {img}", 12.5, "start", "bold",
         "#b91c1c" if k == 1 else "#0f172a")
    # CPU：a 段 + b 等待 + 其余字母段
    rect(tx(sc["s"]), cy, (sc["pre_end"] - sc["s"]) * S, ch, "#dbeafe", "#2563eb", sw=frame_stroke(k))
    text(tx(sc["s"]) + (sc["pre_end"] - sc["s"]) * S / 2, cy + ch / 2, "a\n上传", 10.5)
    if sc["wait_end"] > sc["pre_end"]:
        ww = (sc["wait_end"] - sc["pre_end"]) * S
        rect(tx(sc["pre_end"]), cy, ww, ch, "#fee2e2", FENCE, sw=frame_stroke(k))
        text(tx(sc["pre_end"]) + ww / 2, cy + ch / 2, "b\n等fence", 10.5, fill=FENCE, weight="bold")
    else:
        line(tx(sc["pre_end"]), cy - 2, tx(sc["pre_end"]), cy + ch + 2, FENCE, 2)
        text(tx(sc["pre_end"]), cy + ch + 9, "b 免等", 9, fill=FENCE)
    for letter, t0, d in sc["steps"]:
        fill = "#bfdbfe" if letter == "j" else "#dbeafe"
        rect(tx(t0), cy, d * S, ch, fill, "#2563eb", sw=frame_stroke(k))
        label = {"j": "j\n录制 cmd", "g": "g\n开关", "d": "d\n组装", "f": "f\n四环", "h": "h\nUI"}.get(letter, letter)
        text(tx(t0) + d * S / 2, cy + ch / 2, label, 10.5)
    # 传输队列
    ty_, th = LANES["tr"]
    if sc["up"]:
        u0, u1, tk = sc["up"]
        rect(tx(u0), ty_, (u1 - u0) * S, th, "#fef3c7", "#d97706")
        big = "（大批：贴图流送）" if u1 - u0 > 300 else ""
        text(tx(u0) + (u1 - u0) * S / 2, ty_ + th / 2, f"拷贝→池/对象表/贴图{big}\n置位票据 {tk}", 10.5)
        line(tx(sc["s"] + 50), cy + ch, tx(u0) + 6, ty_, TICKET, 1.4, arrow=True)
        curve(tx(u1), ty_ + th, tx(sc["gpu_start"]) + 3, gy, TICKET, bend=0.6)
    if not sc["up"]:
        text(tx(sc["gpu_start"]) + 4, gy + gh + 26, "无上传批：票据快照 = 7（已完成）→ 票据门免等", 10, "start", fill="#b45309", weight="bold")
    # 图形队列：排队条 + 等票据 + 九段
    if sc["gpu_start"] > sc["submit"]:
        qy = gy + gh + 4
        rect(tx(sc["submit"]), qy, (sc["gpu_start"] - sc["submit"]) * S, 11, "#ffffff", "#64748b", "3 2", rx=2)
        text(tx(sc["submit"]) + 4, qy + 6, "已提交·排队（前帧仍在执行）" if sc["prev_end"] >= sc["ticket_done"] else "已提交·整批等票据", 9, "start", fill="#475569")
    if sc["ticket_done"] > max(sc["submit"], sc["prev_end"]):
        st = max(sc["submit"], sc["prev_end"])
        rect(tx(st), gy, (sc["ticket_done"] - st) * S, gh, "#fef3c7", TICKET, "4 2")
        text(tx(st) + (sc["ticket_done"] - st) * S / 2, gy + gh / 2, "票据门\n整批冻结", 10, fill="#92400e", weight="bold")
    t = sc["gpu_start"]
    seg_x = {}
    for num, nm, d, fill, stroke in SEGS:
        rect(tx(t), gy, d * S, gh, fill, stroke, sw=frame_stroke(k))
        seg_x[num] = tx(t)
        text(tx(t) + d * S / 2, gy + gh / 2, num if d < 50 else f"{num}\n{nm}", 10.5)
        t += d
    text(tx(sc["gpu_start"]) + G * S / 2, gy - 8, f"{name} 命令缓冲（槽 {slot}）", 10, fill="#334155")
    # 图门：image_available 卡在 ④ 起点
    py, ph = LANES["pr"]
    ax = tx(sc["acquire_end"])
    line(ax, cy + ch, ax, py, IMG, 1.0, "2 3")
    curve(ax + 2, py, seg_x["④"], gy + gh, IMG, 1.4, bend=0.5)
    parts.append(f'<polygon points="{seg_x["④"]:.1f},{gy - 1} {seg_x["④"] - 5:.1f},{gy - 9} {seg_x["④"] + 5:.1f},{gy - 9}" fill="{IMG}"/>')
    # render_finished → 演示引擎；fence → 两帧后同槽 wait
    end_x = tx(sc["gpu_end"])
    nxt = sched[k + 1]["gpu_end"] if k + 1 < len(sched) else sc["gpu_end"] + 380
    rect(end_x + 6, py, (nxt - sc["gpu_end"]) * S - 8, ph, "#f3e8ff", "#9333ea")
    text(end_x + 6 + ((nxt - sc["gpu_end"]) * S - 8) / 2, py + ph / 2, f"等 render_finished[{img}] → 呈现并持有 图 {img}", 10.5)
    line(end_x - 2, gy + gh, end_x + 8, py, RFIN, 1.6, arrow=True)
    if k + 2 < len(sched):
        tgt = sched[k + 2]
        curve(end_x - 4, gy, tx(tgt["pre_end"]) + 8, cy + ch, FENCE, 1.8, bend=0.45)
        text(end_x + 4, gy - 20, f"fence[{slot}] 置位", 10, "start", fill=FENCE, weight="bold")

# 时间轴
ay = 600
line(X0, ay, W - 30, ay, GRAY, 1.2, arrow=True)
text(W - 34, ay + 14, "时间", 11, "end", fill=GRAY)
for k, sc in enumerate(sched):
    line(tx(sc["s"]), ay - 4, tx(sc["s"]), ay + 4, GRAY, 1.2)
    text(tx(sc["s"]) + 4, ay - 9, f"{FRAMES[k][0]} draw_frame 开始", 9.5, "start", fill=GRAY)

# 上半图例
ly = 632
legend = [(FENCE, "fence：GPU 执行完 → 两帧后同槽 CPU 才复用"), (TICKET, "上传票据（timeline）：ALL_COMMANDS 拦整批"),
          (IMG, "image_available：COLOR_ATTACHMENT_OUTPUT，首个写颜色的 ④ 起才等"), (RFIN, "render_finished[图]：交演示引擎")]
lx = 30
for color, label in legend:
    line(lx, ly, lx + 34, ly, color, 2, arrow=True)
    text(lx + 42, ly, label, 11, "start")
    lx += 430 if color != IMG else 560
text(lx + 20, ly, "▼ = 图门卡点（④ 起点）", 11, "start", fill=IMG)

# CPU 字母图例
rect(10, 668, W - 20, 96, "#ffffff", "#cbd5e1", rx=6)
text(24, 684, "CPU 段字母（draw_frame 与之前的上传系统）", 12, "start", "bold")
items = list(CPU_LETTERS.items())
for i, (letter, desc) in enumerate(items):
    col, row = i % 4, i // 4
    text(28 + col * 855, 706 + row * 20, f"{letter}  {desc}", 11, "start", vcenter=False)

# ---------------------------------------------------------------- 下半：帧 N 命令缓冲逐 cmd 展开

TOP = 790
LABEL_W = 310
CW = 68
XCOL_W = 170

# (段, 文案, 可选)
CMDS = [
    ("①", "begin\ncmd buffer", False),
    ("①", "重置查询池\n时戳起点", True),
    ("①", "屏障 交换链\nUNDEF→\nCOLOR", False),
    ("①", "屏障 主深度\nUNDEF→\n附件", False),
    ("①", "屏障×4\nG缓冲+HDR\n→COLOR", False),
    ("①", "fill 排序键\n= -inf\n+屏障", True),
    ("②", "dispatch\n视锥剔除\n主视图+\n4级阴影", True),
    ("②", "dispatch\n遮挡剔除\n读 Hi-Z", True),
    ("②", "dispatch\nLOD 选档", True),
    ("②", "dispatch\nindirect\n组装", True),
    ("②", "dispatch\n阴影命令\n4级一次", True),
    ("②", "dispatch\n光源分簇\n32px×32片", True),
    ("②", "dispatch\n透明\ncollect", True),
    ("②", "dispatch\n×105\n双调排序", True),
    ("②", "dispatch\n透明\ngather", True),
    ("③", "屏障×4\n阴影图→\n深度附件", False),
    ("③", "×4 级联\nbegin 2048²\n间接绘制", True),
    ("③", "屏障×4\n阴影图→\n只读", False),
    ("④", "begin\n3色+深度\nCLEAR", False),
    ("④", "绑管线\n3 个集\n视口 VB/IB", True),
    ("④", "间接绘制\n场景命令环\n×count", True),
    ("④", "end\nrendering", False),
    ("⑤", "屏障×4\nG缓冲/深度\n→只读", False),
    ("⑤", "全屏 draw3\n合成光照\n→HDR", False),
    ("⑤", "透明前向\n深度→附件\n混合间接绘", True),
    ("⑤", "屏障\nHDR→只读", False),
    ("⑥", "屏障 深度\n附件→只读", True),
    ("⑥", "屏障\nHi-Z[A]全mip\n→GENERAL", False),
    ("⑥", "dispatch\n×10 逐级\n归约(1080p)", False),
    ("⑦", "全屏\n预滤波\nHDR→降0", True),
    ("⑦", "全屏×4\n降采样\n1..4", True),
    ("⑦", "全屏×4\n升采样\n3..0", True),
    ("⑧", "全屏\ntonemap\n→LDR", False),
    ("⑧", "全屏\nSMAA\n边缘", True),
    ("⑧", "全屏\nSMAA\n混合权重", True),
    ("⑧", "begin 交换链\nneighbor-\nhood draw3", False),
    ("⑧", "UI 逐裁剪\n区 draw_\nindexed", True),
    ("⑧", "end\nrendering", False),
    ("⑨", "屏障 交换链\nCOLOR→\nPRESENT", False),
    ("⑨", "时戳终点\nend cmd\nbuffer", False),
    ("⑨", "reset fence\nqueue_\nsubmit", False),
]
SYNC = {  # 列序号(1 起) → (交接屏障, 时戳)
    3: ("链式等图", ""), 5: ("链式等图", ""), 6: ("T→C", ""), 7: ("→VCRW", ""), 8: ("→VCR", ""),
    9: ("→VCR", ""), 10: ("→ICR", ""), 11: ("→ICR", ""), 12: ("→CFR", ""), 13: ("→CR", ""),
    14: ("→CRW×105", ""), 15: ("→ICR", "时戳 计算"), 2: ("", "时戳 起点"), 18: ("", "时戳 阴影"),
    22: ("", "时戳 G缓冲"), 26: ("", "时戳 光照"), 29: ("→CR×9", "时戳 HiZ"), 38: ("", "时戳 后链"),
    40: ("", "时戳 终点"), 41: ("等2 发2", ""),
}
SEG_STYLE = {num: (nm, fill, stroke) for num, nm, _, fill, stroke in SEGS}

cols = ["T", "H"] + [str(i) for i in range(1, len(CMDS) + 1)]
col_x = {key: LABEL_W + 20 + i * CW for i, key in enumerate(cols)}
col_x["X"] = LABEL_W + 20 + len(cols) * CW
right = col_x["X"] + XCOL_W

rect(10, TOP - 30, W - 20, 1260, "#f8fafc", "#cbd5e1", rx=6)
text(24, TOP - 12, "下半 · 帧 N 命令缓冲逐 cmd 展开（槽 A · 交换链图 0）与资源读写矩阵 —— 一列一条（组）命令，一行一类资源", 15, "start", "bold")

# 段条
sy = TOP + 6
text(LABEL_W, sy + 13, "录制段 →", 12, "end", "bold")
rect(col_x["T"] + 2, sy, CW - 4, 26, "#fef3c7", "#d97706")
text(col_x["T"] + CW / 2, sy + 13, "传输队列\n帧前", 9.5)
rect(col_x["H"] + 2, sy, CW - 4, 26, "#dbeafe", "#2563eb")
text(col_x["H"] + CW / 2, sy + 13, "宿主写\n帧前 e/f/h", 9.5)
seg_spans = {}
for i, (seg, _, _) in enumerate(CMDS, start=1):
    seg_spans.setdefault(seg, [i, i])[1] = i
for seg, (a, b) in seg_spans.items():
    nm, fill, stroke = SEG_STYLE[seg]
    x0, x1 = col_x[str(a)] + 2, col_x[str(b)] + CW - 2
    rect(x0, sy, x1 - x0, 26, fill, stroke, sw=1.4)
    text((x0 + x1) / 2, sy + 13, f"{seg} {nm}", 12, weight="bold")
rect(col_x["X"] + 2, sy, XCOL_W - 4, 26, "#f1f5f9", "#64748b")
text(col_x["X"] + XCOL_W / 2, sy + 13, "跨帧去向", 11, weight="bold")

# cmd 行
cy0, chh = sy + 32, 78
text(LABEL_W, cy0 + chh / 2, "命令（虚线框 = 可选：\n开关关闭/空场时不录）", 11, "end")
for i, (seg, label, optional) in enumerate(CMDS, start=1):
    _, fill, stroke = SEG_STYLE[seg]
    x = col_x[str(i)]
    rect(x + 2, cy0, CW - 4, chh, fill, stroke, "4 2" if optional else None, sw=1.2)
    text(x + CW / 2, cy0 + 12, f"c{i}", 9, fill="#64748b", vcenter=False)
    text(x + CW / 2, cy0 + chh / 2 + 6, label, 9.5)

# 交接行
yy, yh = cy0 + chh + 4, 34
text(LABEL_W, yy + yh / 2, "命令后屏障 / 时戳", 11, "end")
for i, (handoff, ts) in SYNC.items():
    x = col_x[str(i)] + CW / 2
    color = IMG if "等图" in handoff else "#0f172a"
    if handoff:
        text(x, yy + 10, handoff, 9.5, fill=color, weight="bold")
    if ts:
        text(x, yy + 25, ts, 9, fill="#0369a1")

# 矩阵
R = "R"
ROWS = [
    ("group", "帧槽私有（×2，图示为槽 A；槽 B 同构一份）"),
    ("每槽 UBO 组\n帧/阴影/分簇/Bloom/SMAA", {"H": "W", **{str(c): R for c in (7, 12, 13, 17, 21, 24, 25, 32, 33, 34, 36)}}, ""),
    ("实例环 b1 模型 b2 材质\nb4 在场 b7 LOD 表", {"H": "W", **{str(c): R for c in (7, 8, 9, 10, 11, 13, 21, 25)}}, ""),
    ("b3 视锥标记", {"7": "W", "8": "RW", "10": R}, ""),
    ("b6 遮挡标记", {"H": "填*", "8": "W"}, ""),
    ("b8 LOD 档位", {"9": "RW", "10": R, "11": R, "15": R}, "N+2 选档\n读作滞回"),
    ("b9 缺口标记", {"7": "W", "10": "W"}, "N+2 宿主\n缺口快照读"),
    ("b5 场景命令环", {"10": "W", "21": "R"}, ""),
    ("阴影标记 + 4 级\n阴影命令环", {"7": "W", "11": "RW", "17": R}, ""),
    ("光源分簇表", {"12": "W", "24": R, "25": R}, ""),
    ("透明 键/索引/\n计数/命令", {"H": "清计数", "6": "W", "13": "W", "14": "RW", "15": "RW", "25": R}, ""),
    ("主深度 depth[A]", {"4": "L", "19": "W", "21": "W", "23": "L", "24": R, "25": "L·R", "27": "L", "29": R}, ""),
    ("G 缓冲三色[A]", {"5": "L", "19": "W", "21": "W", "23": "L", "24": R}, ""),
    ("HDR 目标[A]", {"5": "L", "24": "W", "25": "RW", "26": "L", "30": R, "33": R}, ""),
    ("Hi-Z 金字塔[A]", {"8": "R旧", "28": "L", "29": "W"}, "N+2 遮挡读\n（c8 读的是 N-2 写）"),
    ("LDR / 边缘 /\n混合权重[A]", {"33": "L·W", "34": "R·W", "35": "R·W", "36": R}, ""),
    ("UI 顶点/索引环[A]", {"H": "W", "37": R}, ""),
    ("时戳查询池[A]", {str(c): "W" for c in (2, 15, 18, 22, 26, 29, 38, 40)}, "N+2 等完 fence\n后宿主读回"),
    ("group", "全局单份（帧槽间共享）"),
    ("对象表 b0", {"T": "W", **{str(c): R for c in (7, 8, 9, 10, 11, 13, 15, 21, 25)}}, "旧槽进墓地：\n全槽 fence 后复用"),
    ("顶点 / 索引池", {"T": "W", "17": R, "21": R, "25": R}, "旧区间进墓地：\nfence+票据双凭据"),
    ("bindless 贴图\n（含 UI 图集）", {"T": "W·L", "21": R, "25": R, "37": R}, "旧图进墓地：\nfence+票据双凭据"),
    ("阴影图 ×4", {"16": "L", "17": "W", "18": "L", "24": R}, "下帧 c16 进场屏障\n挡 WAR（同队列）"),
    ("Bloom 降 ×5 / 升 ×4", {"30": "L·W", "31": "RW", "32": "RW", "33": R}, "下帧进场屏障\n挡 WAR（同队列）"),
    ("交换链图[0]", {"3": "L", "36": "W", "37": "W", "39": "L"}, "演示引擎读\n（render_finished）"),
]


def cell_style(mark):
    if mark.startswith("L"):
        return "#fde68a", "#a16207"
    if "R" in mark and "W" in mark:
        return "#e9d5ff", "#7e22ce"
    if "W" in mark or mark in ("填*", "清计数"):
        return "#fecaca", "#b91c1c"
    if "R" in mark:
        return "#bfdbfe", "#1d4ed8"
    return "#e2e8f0", "#475569"


y = yy + yh + 6
RH = 30
matrix_top = y
for row in ROWS:
    if row[0] == "group":
        rect(20, y, right - 20, 22, "#e2e8f0", "#94a3b8", rx=2)
        text(28, y + 11, row[1], 12, "start", "bold")
        y += 26
        continue
    label, marks, cross = row
    rect(20, y, LABEL_W - 4, RH - 2, "#ffffff", "#cbd5e1", rx=2)
    text(LABEL_W - 10, y + RH / 2 - 1, label, 10.5, "end")
    for key in cols:
        x = col_x[key]
        parts.append(f'<rect x="{x + 1}" y="{y}" width="{CW - 2}" height="{RH - 2}" fill="#ffffff" stroke="#eef2f7"/>')
        if key in marks:
            fill, stroke = cell_style(marks[key])
            rect(x + 5, y + 3, CW - 10, RH - 8, fill, stroke, rx=4)
            text(x + CW / 2, y + RH / 2 - 1, marks[key], 10.5, weight="bold", fill=stroke)
    if cross:
        rect(col_x["X"] + 2, y, XCOL_W - 4, RH - 2, "#f1f5f9", "#94a3b8", rx=2)
        text(col_x["X"] + XCOL_W / 2, y + RH / 2 - 1, cross, 9.5, fill="#334155")
    y += RH
matrix_bottom = y

# 两道门：竖线贯穿 cmd 行到矩阵底
gx = col_x["1"]
line(gx, cy0 - 4, gx, matrix_bottom, TICKET, 2.4, "7 4")
text(gx + 4, cy0 - 20 + 0, "", 10)
gx2 = col_x["19"]
line(gx2, cy0 - 4, gx2, matrix_bottom, IMG, 2.4, "7 4")
gate_y = matrix_bottom + 16
text(gx + 4, gate_y, "▲ 票据门：ALL_COMMANDS —— 上传未完成时整批（含 compute）都不开跑", 11.5, "start", "bold", "#b45309")
text(gx2 + 4, gate_y, "▲ 图门：COLOR_ATTACHMENT_OUTPUT —— 首个写颜色的 c19 起等 image_available；c3/c5 屏障 src=CAO 链式同等；c7~c18 compute 与阴影深度不受拦，与等图重叠", 11.5, "start", "bold", IMG)

# 底部读法与要点
ny = gate_y + 26
rect(20, ny, right - 20, 120, "#ffffff", "#cbd5e1", rx=6)
cell_legend = [("W", "写"), ("R", "读"), ("RW", "读写"), ("L", "布局迁移（迁移本身是一次写）"), ("R旧", "读两帧前同槽写入的内容")]
lx = 34
text(lx, ny + 18, "格子：", 11.5, "start", "bold")
lx += 48
for mark, desc in cell_legend:
    fill, stroke = cell_style(mark)
    rect(lx, ny + 8, 40, 20, fill, stroke, rx=4)
    text(lx + 20, ny + 18, mark, 10.5, weight="bold", fill=stroke)
    text(lx + 48, ny + 18, desc, 11, "start")
    lx += 70 + len(desc) * 11.5
text(lx + 20, ny + 18, "屏障缩写：V 顶点 · C 计算 · F 片段 · I 间接命令读 · R/W 读/写访问（如 →ICR = 计算写交给间接绘制读）；T→C = 传输写交给计算", 11, "start")
notes = [
    "帧槽私有物：同槽隔两帧复用，凭据是本槽 fence（CPU 侧 wait_for_slot）。Hi-Z 金字塔、LOD 档位、缺口标记、时戳四样故意跨帧读——读的正是两帧前同槽的结果，fence 同时保证它已写完。",
    "全局单份物：阴影图与 Bloom 链两帧共用一份，靠同一图形队列的提交序 + 下一帧进场屏障（UNDEFINED，src=片段）挡住写后读的反向竞争（WAR），不需要额外信号量。",
    "传输队列写的对象表 / 池 / 贴图：只写新槽或新区间，被换下的旧条目进墓地，等「全部帧槽 fence + 上传票据」两本账都清才回收——在飞帧可能还在读旧的。",
]
for i, note in enumerate(notes):
    text(34, ny + 46 + i * 24, f"{i + 1}. {note}", 11.5, "start", vcenter=False)

H = int(ny + 140)

# 标题与箭头标记
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
head.append(
    f'<text x="{W / 2}" y="44" font-size="26" font-weight="bold" text-anchor="middle" fill="#0f172a">'
    "多帧在飞全景：draw_frame × 4 与帧 N 命令缓冲逐 cmd 读写</text>"
)
head.append(
    f'<text x="{W / 2}" y="74" font-size="14" text-anchor="middle" fill="#475569">'
    "ash_renderer · 2 帧槽 / 3 交换链图 / 2 上传槽 · 图形与传输两条队列 · 时长为示意刻度（GPU 受限场景）</text>"
)

with open(OUT, "w", encoding="utf-8", newline="\n") as f:
    f.write("\n".join(head + parts + ["</svg>"]))
print(f"written {OUT} ({W}x{H}), 宏观时序最右 x = {tx(sched[-1]['gpu_end'] + 380):.0f}")
