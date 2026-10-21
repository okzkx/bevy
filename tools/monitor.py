#!/usr/bin/env python3
"""无人值守持续监控采样（7.5）：定间隔轮询 BRP `monitor_stats` 落 JSONL。

用法：
    python tools/monitor.py --duration 1800 --interval 2 --out .temp/7.5/monitor.jsonl
    python tools/monitor.py --skip-launch          # 宿主已在跑，只采样
    python tools/monitor.py --check                # 只采一次打到 stdout 自检

样本 = BRP monitor_stats 原样响应 + 采集侧时间戳；缺帧不补（时间戳如实），
完整性用 --check 后核对 history_len/时间戳连续性。修复前后同配方对照的数据源。
"""

import argparse
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request

DEFAULT_PORT = 15702
CARGO_MANIFEST = "ash_renderer"


def rpc(host, port, method, params=None, timeout=10):
    body = {"jsonrpc": "2.0", "method": method, "id": 1}
    if params is not None:
        body["params"] = params
    req = urllib.request.Request(
        f"http://{host}:{port}",
        data=json.dumps(body).encode("utf-8"),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def launch_host(args):
    """后台拉起宿主（默认场景 Sponza），等 BRP 端口就绪。"""
    proc = subprocess.Popen(
        ["cargo", "run", "-p", CARGO_MANIFEST],
        cwd=".",
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    print(f"宿主已拉起 PID {proc.pid}，等 BRP {args.host}:{args.port} ...")
    for _ in range(300):
        try:
            rpc(args.host, args.port, "ash_renderer/monitor_stats", timeout=2)
            print("BRP 就绪")
            return proc
        except OSError:
            time.sleep(1)
    raise SystemExit("300s 内 BRP 未就绪")


def sample_once(args):
    return rpc(args.host, args.port, "ash_renderer/monitor_stats")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--interval", type=float, default=2.0, help="采样间隔秒")
    ap.add_argument("--duration", type=float, default=1800.0, help="采样时长秒")
    ap.add_argument("--out", default=".temp/7.5/monitor.jsonl")
    ap.add_argument("--launch", action="store_true", help="先拉起宿主（默认不拉）")
    ap.add_argument("--skip-launch", action="store_true", help="兼容占位：默认就不拉")
    ap.add_argument("--check", action="store_true", help="只采一次打到 stdout")
    args = ap.parse_args()

    if args.check:
        print(json.dumps(sample_once(args), ensure_ascii=False, indent=2))
        return

    proc = launch_host(args) if args.launch else None

    import os
    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    t0 = time.monotonic()
    n = 0
    with open(args.out, "a", encoding="utf-8", newline="\n") as f:
        while time.monotonic() - t0 < args.duration:
            t = time.monotonic() - t0
            try:
                resp = sample_once(args)
                f.write(json.dumps({"t": round(t, 2), "resp": resp}, ensure_ascii=False) + "\n")
                f.flush()
                n += 1
                if n % 15 == 0:  # 每 30s（2s 间隔）一行进度
                    fps = (resp.get("fps") or 0)
                    ws = resp.get("proc_working_set") or 0
                    print(f"t={t:7.1f}s 样本 {n} fps {fps:5.1f} 进程 {ws/1048576:.1f}MB")
            except OSError as e:
                f.write(json.dumps({"t": round(t, 2), "error": str(e)}, ensure_ascii=False) + "\n")
                f.flush()
                print(f"t={t:7.1f}s 采样失败 {e}")
            time.sleep(args.interval)

    print(f"完成：{n} 个样本 → {args.out}")
    if proc is not None:
        print(f"宿主 PID {proc.pid} 仍在运行（收尾请自行关闭或 --help 看说明）")


if __name__ == "__main__":
    main()
