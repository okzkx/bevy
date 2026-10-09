"""按 PID 抓主窗口截图（PrintWindow，DPI 感知），输出 PNG + 尺寸/哈希收账。

用法:
  python tools/capture_window.py --pid <pid> --out .temp/shot.png

主窗选择：EnumWindows 按 PID + 可见 + 有标题 + 最大面积过滤，避开 winit
事件窗（14×14 隐身窗冒充主窗的教训，见 3.2 施工记录 §4）。
测量进程先 SetProcessDpiAwareness(2)，GetWindowRect 返回真实物理坐标
（画面功能视觉验证规则 §1）。stdout 打一行收账：hwnd/rect/尺寸/md5。
"""

import argparse
import ctypes
import hashlib
import sys
from ctypes import wintypes

from PIL import Image

user32 = ctypes.windll.user32
gdi32 = ctypes.windll.gdi32
shcore = ctypes.windll.shcore

# 测量通道去虚拟化：坐标一律物理像素（规则 §1）
try:
    shcore.SetProcessDpiAwareness(2)
except OSError:
    user32.SetProcessDPIAware()

PW_RENDERFULLCONTENT = 0x00000002
BI_RGB = 0
DIB_RGB_COLORS = 0


class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [
        ("biSize", wintypes.DWORD),
        ("biWidth", wintypes.LONG),
        ("biHeight", wintypes.LONG),
        ("biPlanes", wintypes.WORD),
        ("biBitCount", wintypes.WORD),
        ("biCompression", wintypes.DWORD),
        ("biSizeImage", wintypes.DWORD),
        ("biXPelsPerMeter", wintypes.LONG),
        ("biYPelsPerMeter", wintypes.LONG),
        ("biClrUsed", wintypes.DWORD),
        ("biClrImportant", wintypes.DWORD),
    ]


class BITMAPINFO(ctypes.Structure):
    _fields_ = [("bmiHeader", BITMAPINFOHEADER), ("bmiColors", wintypes.DWORD)]


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


def capture(hwnd: int, out_path: str) -> None:
    rect = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(rect))
    width, height = rect.right - rect.left, rect.bottom - rect.top

    hdc = user32.GetWindowDC(hwnd)
    mem = gdi32.CreateCompatibleDC(hdc)
    bmp = gdi32.CreateCompatibleBitmap(hdc, width, height)
    gdi32.SelectObject(mem, bmp)
    if not user32.PrintWindow(hwnd, mem, PW_RENDERFULLCONTENT):
        sys.exit("PrintWindow 失败")
    bmi = BITMAPINFO()
    bmi.bmiHeader.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    bmi.bmiHeader.biWidth = width
    bmi.bmiHeader.biHeight = -height  # 负高 = 顶行在前
    bmi.bmiHeader.biPlanes = 1
    bmi.bmiHeader.biBitCount = 32
    bmi.bmiHeader.biCompression = BI_RGB
    buf = ctypes.create_string_buffer(width * height * 4)
    gdi32.GetDIBits(mem, bmp, 0, height, buf, ctypes.byref(bmi), DIB_RGB_COLORS)

    image = Image.frombuffer("RGB", (width, height), buf.raw, "raw", "BGRX", 0, 1)
    image.save(out_path)
    digest = hashlib.md5(image.tobytes()).hexdigest()
    print(
        f"hwnd=0x{hwnd:X} rect=({rect.left},{rect.top},{rect.right},{rect.bottom}) "
        f"size={width}x{height} md5={digest} -> {out_path}"
    )

    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(mem)
    user32.ReleaseDC(hwnd, hdc)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int, required=True, help="目标进程 PID")
    parser.add_argument("--out", required=True, help="PNG 输出路径")
    args = parser.parse_args()
    capture(find_main_window(args.pid), args.out)


if __name__ == "__main__":
    main()
