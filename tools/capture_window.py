"""按 PID 抓主窗口截图（PrintWindow，DPI 感知），输出 PNG + 尺寸/哈希收账。

用法:
  python tools/capture_window.py --pid <pid> --out .temp/shot.png

主窗选择：EnumWindows 按 PID + 可见 + 有标题 + 最大面积过滤，避开 winit
事件窗（14×14 隐身窗冒充主窗的教训，见 3.2 施工记录 §4）。
测量进程先 SetProcessDpiAwareness(2)，GetWindowRect 返回真实物理坐标
（画面功能视觉验证规则 §1）。stdout 打一行收账：hwnd/rect/尺寸/md5。

白图自愈（2026-10-09 无人模式实测）：目标窗口被 TOPMOST 全屏覆盖（本机
DLP 锁屏海报 CcWaterMarkWindow 轮播）时，PW_RENDERFULLCONTENT 整幅返回
近白——DWM 拒绝被完全遮挡窗口的内容重定向；窗口在屏幕上自身渲染无恙。
检测到非白占比 <10% 时自动把目标窗口 TOPMOST 提顶（300ms 后 PrintWindow
重试），成功后回落 NOTOPMOST 复原 Z 序；stdout 以「白图→提顶重试」标注。
注意：不要用屏幕区域抓取兜底——全屏遮挡物存在时它会抓到遮挡物而非目标。
"""

import argparse
import ctypes
import hashlib
import sys
import time
from ctypes import wintypes

from PIL import Image

user32 = ctypes.windll.user32
gdi32 = ctypes.windll.gdi32
shcore = ctypes.windll.shcore

user32.SetWindowPos.argtypes = [
    wintypes.HWND, wintypes.HWND, ctypes.c_int, ctypes.c_int,
    ctypes.c_int, ctypes.c_int, wintypes.UINT,
]
user32.SetWindowPos.restype = wintypes.BOOL

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


def non_white_ratio(img) -> float:
    """非纯白像素占比（下采样 200x120 足以判白图，标题栏约贡献 5%）。"""
    small = img.resize((200, 120))
    px = list(small.getdata())
    return sum(1 for p in px if p != (255, 255, 255)) / len(px)


def printwindow_frame(hwnd: int, width: int, height: int) -> Image.Image:
    """PrintWindow 单帧抓取，返回 RGB 图。"""
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
    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(mem)
    user32.ReleaseDC(hwnd, hdc)
    return image


def capture(hwnd: int, out_path: str) -> None:
    rect = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(rect))
    width, height = rect.right - rect.left, rect.bottom - rect.top

    image = printwindow_frame(hwnd, width, height)
    channel = "PrintWindow"

    # 白图自愈：被 TOPMOST 全屏覆盖（如 DLP 锁屏海报）时 DWM 拒绝内容重定向，
    # 整幅近白——TOPMOST 提顶重试，成功后回落 NOTOPMOST 复原 Z 序
    if non_white_ratio(image) < 0.10:
        if user32.SetWindowPos(hwnd, -1, 0, 0, 0, 0, 0x43):
            time.sleep(0.3)
            image = printwindow_frame(hwnd, width, height)
            user32.SetWindowPos(hwnd, -2, 0, 0, 0, 0, 0x43)
            channel = "白图→提顶重试"
        else:
            channel = "PrintWindow(白图且提顶失败)"

    ratio = non_white_ratio(image)
    image.save(out_path)
    digest = hashlib.md5(image.tobytes()).hexdigest()
    print(
        f"hwnd=0x{hwnd:X} rect=({rect.left},{rect.top},{rect.right},{rect.bottom}) "
        f"size={width}x{height} md5={digest} 非白={ratio:.1%} [{channel}] -> {out_path}"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int, required=True, help="目标进程 PID")
    parser.add_argument("--out", required=True, help="PNG 输出路径")
    args = parser.parse_args()
    capture(find_main_window(args.pid), args.out)


if __name__ == "__main__":
    main()
