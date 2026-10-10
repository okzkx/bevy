"""BRP 薄客户端（3.12/3.13）：把宿主 bevy_remote 的 HTTP JSON-RPC 协议封装成逐命令
形状，AI 免记协议即可完成"查询 → 改 Transform → 相机环绕推拉 → 虚拟鼠标操控"闭环。

用法:
  python tools/brp.py tree                                        # 场景层级树（ash_renderer/scene_tree）
  python tools/brp.py inspect --entity 4v1                        # 实体详情（world.inspect）
  python tools/brp.py query --with Mesh3d --option all            # 实体查询（world.query）
  python tools/brp.py set-transform 4v1 --translation 0 0.3 0 [--rotation-deg Y P R] [--scale X Y Z]
  python tools/brp.py camera [--yaw-deg 30] [--pitch-deg 15] [--radius 2] [--target X Y Z] [--zoom 0.9]
  python tools/brp.py mouse-move --x 640 --y 360                  # 虚拟鼠标移动（points 域逻辑像素，左上原点）
  python tools/brp.py mouse-button --button left --action press   # 虚拟按下/抬起（press|release）
  python tools/brp.py mouse-button --action release --all         # 释放全部按住键（卡"按住"保险）
  python tools/brp.py mouse-wheel --lines 3                       # 虚拟滚轮（--lines|--pixels 二选一，正值向上滚=推近）
  python tools/brp.py mouse-status                                # 鼠标状态读回（位置/按住键/窗口/本帧滚轮）
  python tools/brp.py discover                                    # 全部可用方法（rpc.discover）
  python tools/brp.py raw --method world.query --params-json '{...}'

组件名支持短名（如 Mesh3d、Transform），内部向宿主 world.list_components 查
全量路径后按"最后一个 :: 段"匹配展开；含 :: 的输入视为全路径原样透传。
虚拟鼠标注入发生在应用层消息缓冲（Messages<WindowEvent>，与 bevy_winit 同层），
不碰用户真实光标/焦点，不受无人模式纪律约束；操作序列先 mouse-move 再 press
（按压需要指针落点），写值统一下一帧生效。
宿主须先起（cargo run -p ash_renderer）；--host/--port 可覆盖，默认 127.0.0.1:15702。
无鉴权仅本地：与宿主同机使用，不得对网开放（BRP 侦察笔记 §6 边界）。
连接被拒时提示宿主未起，退出码 2；BRP 层错误（含 ENTITY_NOT_FOUND 等）原样
打印后退出码 1。
"""

import argparse
import json
import sys
import urllib.error
import urllib.request

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 15702  # bevy_remote/src/http.rs DEFAULT_PORT


def rpc(host, port, method, params=None, timeout=10):
    """发一次 JSON-RPC 2.0 请求，返回完整响应（result/error 都在里面）。"""
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


def resolve_component(host, port, name):
    """短名 → 全限定路径：向宿主要 world.list_components 全量后按尾段匹配；
    含 :: 的输入按全路径原样透传。匹配不到原样返回（让服务端报错收账）。"""
    if "::" in name:
        return name
    try:
        resp = rpc(host, port, "world.list_components")
    except (urllib.error.URLError, OSError):
        return name  # 宿主不可达：交给调用方报连接错误
    names = resp.get("result") or []
    hits = [n for n in names if n.rsplit("::", 1)[-1] == name]
    return hits[0] if len(hits) == 1 else name


def out(resp):
    """收账输出：一行业务状态 + 美化 JSON。错误响应以退出码 1 收账。"""
    print(json.dumps(resp, ensure_ascii=False, indent=2))
    if "error" in resp:
        sys.exit(1)


def cmd_tree(args):
    resp = rpc(args.host, args.port, "ash_renderer/scene_tree")
    if "result" not in resp:
        out(resp)
        return

    def draw(node, depth):
        print("  " * depth + f"{node['label']}  [{node['entity']}]")
        for child in node.get("children", []):
            draw(child, depth + 1)

    result = resp["result"]
    print(f"brp: 相关 {result['relevant']} / 全部 {result['total']}")
    for root in result["roots"]:
        draw(root, 0)


def cmd_inspect(args):
    out(rpc(args.host, args.port, "world.inspect", {"entity": args.entity}))


def cmd_query(args):
    def expand(names):
        return [resolve_component(args.host, args.port, n) for n in names]

    params = {
        "data": {
            "components": expand(args.data),
            "option": expand(args.option) if args.option else [],
        },
        "filter": {
            "with": expand(args.with_),
            "without": expand(args.without),
        },
    }
    out(rpc(args.host, args.port, "world.query", params))


def cmd_set_transform(args):
    params = {"entity": args.entity}
    if args.translation:
        params["translation"] = args.translation
    if args.rotation_deg:
        params["rotation_euler_deg"] = args.rotation_deg
    if args.scale:
        params["scale"] = args.scale
    out(rpc(args.host, args.port, "ash_renderer/set_transform", params))


def cmd_camera(args):
    params = {}
    if args.target:
        params["target"] = args.target
    for key in ("yaw_deg", "pitch_deg", "radius", "zoom"):
        value = getattr(args, key)
        if value is not None:
            params[key] = value
    out(rpc(args.host, args.port, "ash_renderer/set_camera", params))


def cmd_mouse_move(args):
    out(rpc(args.host, args.port, "ash_renderer/mouse_move", {"x": args.x, "y": args.y}))


def cmd_mouse_button(args):
    params = {"action": args.action}
    if args.all:
        params["all"] = True
    elif args.button:
        params["button"] = args.button
    out(rpc(args.host, args.port, "ash_renderer/mouse_button", params))


def cmd_mouse_wheel(args):
    params = {}
    if args.lines is not None:
        params["lines"] = args.lines
    if args.pixels is not None:
        params["pixels"] = args.pixels
    out(rpc(args.host, args.port, "ash_renderer/mouse_wheel", params))


def cmd_mouse_status(args):
    out(rpc(args.host, args.port, "ash_renderer/mouse_status"))


def cmd_discover(args):
    resp = rpc(args.host, args.port, "rpc.discover")
    if "result" in resp:
        methods = resp["result"].get("methods", [])
        # 方法是 {name, params} 对象表；params 非空时带出形参清单（AI 免查协议）
        names = sorted(m.get("name", "?") for m in methods)
        params_of = {m.get("name", "?"): m.get("params", []) for m in methods}
        print(f"brp: {len(names)} 个可用方法")
        for n in names:
            spec = params_of.get(n) or []
            tail = f"  参数: {', '.join(p.get('name', '?') for p in spec)}" if spec else ""
            print(f"  {n}{tail}")
        if any(n.startswith("ash_renderer/") for n in names):
            print("brp: 自定义方法族 ash_renderer/* 已注册")
    else:
        out(resp)


def cmd_raw(args):
    params = json.loads(args.params_json) if args.params_json else None
    out(rpc(args.host, args.port, args.method, params))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", default=DEFAULT_HOST, help=f"BRP 服务端地址（默认 {DEFAULT_HOST}）")
    parser.add_argument("--port", type=int, default=DEFAULT_PORT, help=f"BRP 服务端端口（默认 {DEFAULT_PORT}）")
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("tree", help="场景层级树（ash_renderer/scene_tree）").set_defaults(func=cmd_tree)

    p = sub.add_parser("inspect", help="实体详情（world.inspect）")
    p.add_argument("--entity", required=True, help="实体号，如 4v1")
    p.set_defaults(func=cmd_inspect)

    p = sub.add_parser("query", help="实体查询（world.query）")
    p.add_argument("--data", nargs="*", default=[], help="要取值的组件（短名或全路径）")
    p.add_argument("--option", nargs="*", default=[], help="可选取值的组件；all = 全部可读组件")
    p.add_argument("--with", dest="with_", nargs="*", default=[], help="必须携带的组件")
    p.add_argument("--without", nargs="*", default=[], help="必须缺少的组件")
    p.set_defaults(func=cmd_query)

    p = sub.add_parser("set-transform", help="改实体 Transform（ash_renderer/set_transform）")
    p.add_argument("entity", help="目标实体号，如 4v1")
    p.add_argument("--translation", nargs=3, type=float, metavar=("X", "Y", "Z"))
    p.add_argument("--rotation-deg", nargs=3, type=float, metavar=("YAW", "PITCH", "ROLL"))
    p.add_argument("--scale", nargs=3, type=float, metavar=("X", "Y", "Z"))
    p.set_defaults(func=cmd_set_transform)

    p = sub.add_parser("camera", help="相机给值（ash_renderer/set_camera）")
    p.add_argument("--yaw-deg", type=float, help="方位角（度，绝对值）")
    p.add_argument("--pitch-deg", type=float, help="仰角（度，绝对值，±89 限位）")
    p.add_argument("--radius", type=float, help="距离（绝对值，0.5~10 限位）")
    p.add_argument("--target", nargs=3, type=float, metavar=("X", "Y", "Z"), help="环绕中心")
    p.add_argument("--zoom", type=float, help="缩放系数（radius *= zoom，0.9 = 推近 10%%）")
    p.set_defaults(func=cmd_camera)

    p = sub.add_parser("mouse-move", help="虚拟鼠标移动（ash_renderer/mouse_move）")
    p.add_argument("--x", type=float, required=True, help="points 域逻辑像素，左上原点")
    p.add_argument("--y", type=float, required=True, help="points 域逻辑像素，左上原点")
    p.set_defaults(func=cmd_mouse_move)

    p = sub.add_parser("mouse-button", help="虚拟鼠标按下/抬起（ash_renderer/mouse_button）")
    p.add_argument("--button", choices=["left", "right", "middle", "back", "forward"])
    p.add_argument("--action", choices=["press", "release"], required=True)
    p.add_argument("--all", action="store_true", help="释放全部按住键（仅 release；与 --button 互斥）")
    p.set_defaults(func=cmd_mouse_button)

    p = sub.add_parser("mouse-wheel", help="虚拟滚轮（ash_renderer/mouse_wheel）")
    p.add_argument("--lines", type=float, help="滚动格数（正值向上滚 = 相机推近）")
    p.add_argument("--pixels", type=float, help="滚动像素（egui 面板像素域滚动）")
    p.set_defaults(func=cmd_mouse_wheel)

    sub.add_parser("mouse-status", help="鼠标状态读回（ash_renderer/mouse_status）").set_defaults(func=cmd_mouse_status)

    sub.add_parser("discover", help="列出全部可用 BRP 方法").set_defaults(func=cmd_discover)

    p = sub.add_parser("raw", help="透传任意 JSON-RPC（协议兜底）")
    p.add_argument("--method", required=True)
    p.add_argument("--params-json", help='params 的 JSON，如 \'{"entity": "4v1"}\'')
    p.set_defaults(func=cmd_raw)

    args = parser.parse_args()
    try:
        args.func(args)
    except (urllib.error.URLError, OSError) as e:
        print(f"brp: 连不上 BRP 服务端 http://{args.host}:{args.port}（宿主起了吗？cargo run -p ash_renderer）：{e}")
        sys.exit(2)


if __name__ == "__main__":
    main()
