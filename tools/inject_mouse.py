"""注入鼠标输入（SendInput，DPI 感知）：左键拖拽与滚轮两个子命令。

用法:
  python tools/inject_mouse.py drag --x <物理x> --y <物理y> --dx <物理dx> --dy <物理dy> [--steps 24] [--step-ms 15]
  python tools/inject_mouse.py wheel --x <物理x> --y <物理y> --notches <正滚上/负滚下> [--per-notch-ms 80]

坐标为屏幕物理像素（进程已 SetProcessDpiAwareness(2)，画面功能视觉验证
规则 §1）。注入走前台窗口焦点：目标窗口须先置前台（点到空白处即聚焦，
无位移的按下-抬起不改变相机状态）。
"""

import argparse
import ctypes
import time
from ctypes import wintypes

user32 = ctypes.windll.user32

# 测量/注入通道去虚拟化：坐标一律物理像素（规则 §1）
try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except OSError:
    user32.SetProcessDPIAware()

MOUSEEVENTF_MOVE = 0x0001
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004
MOUSEEVENTF_ABSOLUTE = 0x8000
MOUSEEVENTF_WHEEL = 0x0800
WHEEL_DELTA = 120


class MOUSEINPUT(ctypes.Structure):
    _fields_ = [
        ("dx", wintypes.LONG),
        ("dy", wintypes.LONG),
        ("mouseData", wintypes.DWORD),
        ("dwFlags", wintypes.DWORD),
        ("time", wintypes.DWORD),
        ("dwExtraInfo", ctypes.POINTER(wintypes.ULONG)),
    ]


class INPUT(ctypes.Structure):
    class _I(ctypes.Union):
        _fields_ = [("mi", MOUSEINPUT)]

    _anonymous_ = ("i",)
    _fields_ = [("type", wintypes.DWORD), ("i", _I)]


def send(*flags_and_data: tuple) -> None:
    """逐条 SendInput；每项 = (flags, dx, dy, mouse_data)。"""
    for flags, dx, dy, data in flags_and_data:
        inp = INPUT(type=0)  # INPUT_MOUSE
        inp.mi = MOUSEINPUT(dx, dy, data, flags, 0, None)
        if user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT)) != 1:
            raise OSError("SendInput 失败")


def to_absolute(px: int, py: int) -> tuple[int, int]:
    screen_w = user32.GetSystemMetrics(0)  # SM_CXSCREEN
    screen_h = user32.GetSystemMetrics(1)
    return (px * 65535) // (screen_w - 1), (py * 65535) // (screen_h - 1)


def move_to(px: int, py: int) -> None:
    ax, ay = to_absolute(px, py)
    send((MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, ax, ay, 0))


def drag(px: int, py: int, dx: int, dy: int, steps: int, step_ms: int) -> None:
    move_to(px, py)
    time.sleep(0.05)
    send((MOUSEEVENTF_LEFTDOWN, 0, 0, 0))
    time.sleep(0.05)
    for i in range(1, steps + 1):
        ax, ay = to_absolute(px + dx * i // steps, py + dy * i // steps)
        send((MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, ax, ay, 0))
        time.sleep(step_ms / 1000.0)
    time.sleep(0.05)
    send((MOUSEEVENTF_LEFTUP, 0, 0, 0))


def wheel(px: int, py: int, notches: int, per_notch_ms: int) -> None:
    move_to(px, py)
    time.sleep(0.05)
    step = WHEEL_DELTA if notches > 0 else -WHEEL_DELTA
    for _ in range(abs(notches)):
        send((MOUSEEVENTF_WHEEL, 0, 0, step & 0xFFFFFFFF))
        time.sleep(per_notch_ms / 1000.0)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_drag = sub.add_parser("drag", help="左键按住拖动")
    for name in ("x", "y", "dx", "dy"):
        p_drag.add_argument(f"--{name}", type=int, required=True)
    p_drag.add_argument("--steps", type=int, default=24)
    p_drag.add_argument("--step-ms", type=int, default=15)

    p_wheel = sub.add_parser("wheel", help="滚轮（正=向前滚上，负=向后滚下）")
    for name in ("x", "y", "notches"):
        p_wheel.add_argument(f"--{name}", type=int, required=True)
    p_wheel.add_argument("--per-notch-ms", type=int, default=80)

    args = parser.parse_args()
    if args.cmd == "drag":
        drag(args.x, args.y, args.dx, args.dy, args.steps, args.step_ms)
    else:
        wheel(args.x, args.y, args.notches, args.per_notch_ms)
    print(f"注入完成：{args.cmd} @ ({args.x},{args.y})")


if __name__ == "__main__":
    main()
