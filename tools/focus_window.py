"""把目标窗口置前台（AttachThreadInput 套路，绕过前台锁定限制）。

用法:
  python tools/focus_window.py --pid <pid>

后台启动的宿主窗口 Z 序靠后，注入的鼠标事件会落到覆盖它的窗口上——
注入输入前必须先置前台。SetForegroundWindow 单打会被系统前台锁定策略
拒绝，标准解法是当前线程与前台线程附联后一起切换。
"""

import argparse
import ctypes
import sys
import time
from ctypes import wintypes

user32 = ctypes.windll.user32
kernel32 = ctypes.windll.kernel32

# 测量/注入通道去虚拟化（画面功能视觉验证规则 §1）
try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)
except OSError:
    user32.SetProcessDPIAware()


def find_main_window(pid: int) -> int:
    best = [0, -1]  # [hwnd, area]

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def on_window(hwnd, _lparam):
        owner = wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value != pid or not user32.IsWindowVisible(hwnd):
            return True
        if user32.GetWindowTextLengthW(hwnd) == 0:
            return True
        rect = wintypes.RECT()
        user32.GetWindowRect(hwnd, ctypes.byref(rect))
        area = (rect.right - rect.left) * (rect.bottom - rect.top)
        if area > best[1]:
            best[0], best[1] = hwnd, area
        return True

    user32.EnumWindows(on_window, 0)
    if best[0] == 0:
        sys.exit(f"PID {pid} 下找不到可见有标题的窗口")
    return best[0]


def focus(hwnd: int) -> None:
    if user32.IsIconic(hwnd):
        user32.ShowWindow(hwnd, 9)  # SW_RESTORE
    fg = user32.GetForegroundWindow()
    fg_thread = user32.GetWindowThreadProcessId(fg, None)
    this_thread = kernel32.GetCurrentThreadId()
    target_thread = user32.GetWindowThreadProcessId(hwnd, None)
    if fg_thread != target_thread:
        user32.AttachThreadInput(this_thread, fg_thread, True)
        user32.AttachThreadInput(this_thread, target_thread, True)
    user32.BringWindowToTop(hwnd)
    user32.SetForegroundWindow(hwnd)
    if fg_thread != target_thread:
        user32.AttachThreadInput(this_thread, fg_thread, False)
        user32.AttachThreadInput(this_thread, target_thread, False)
    time.sleep(0.15)
    ok = user32.GetForegroundWindow() == hwnd
    print(f"置前台 hwnd=0x{hwnd:X} -> {'成功' if ok else '失败（前台未切换）'}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int, required=True, help="目标进程 PID")
    args = parser.parse_args()
    focus(find_main_window(args.pid))


if __name__ == "__main__":
    main()
