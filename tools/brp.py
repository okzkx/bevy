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
4.1 对象增量族（spawn/replace/stats = ash_renderer 自定义；despawn/remove =
官方 world.despawn_entity / world.remove_components 薄封装——反射通道按组件名
操作，不需要构造组件值）：变更写值均下一帧生效，账本经 Changed/Removed 通道
自动对账，increment_stats 读到的 processed_rows 就是"动了谁"的答案。
4.2 资产增量族（mutate = ash_renderer/mutate_asset，操作本体 asset_ops 可复用层）：
改资产内容走 Assets::get_mut 原位变更，AssetEvent::Modified 经读侧失效联动 →
同柄重传（swap-on-commit）→ 画面次帧生效；stats 的 asset_events/asset_dirty_rows
是事件读数的对账口。
4.3 异步就绪族（load-mesh/load-image = ash_renderer 自定义，操作本体 object_ops
加载封装）：受理即返回**加载中柄**（AssetId 已定型），pending/到货/失败/取消
看 stats 的 readiness 段；路径失败经失败读侧收账（出重试队列，宿主存活），
到货帧画面生效。加载是异步链，命令不等到货。
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


def cmd_spawn(args):
    params = {"source": args.source}
    if args.offset:
        params["offset"] = args.offset
    if args.name_suffix:
        params["name_suffix"] = args.name_suffix
    out(rpc(args.host, args.port, "ash_renderer/spawn_primitive", params))


def cmd_spawn_shape(args):
    params = {"shape": args.shape}
    if args.color:
        params["color"] = args.color
    if args.translation:
        params["translation"] = args.translation
    if args.rotation:
        params["rotation_euler_deg"] = args.rotation
    if args.scale:
        params["scale"] = args.scale
    if args.name:
        params["name"] = args.name
    out(rpc(args.host, args.port, "ash_renderer/spawn_shape", params))


def cmd_generate_chunks(args):
    params = {
        "chunk_grid": args.chunk_grid,
        "instances_per_chunk": args.instances_per_chunk,
        "spacing": args.spacing,
        "seed": args.seed,
    }
    if args.source:
        params["source"] = args.source
    if args.jitter:
        params["jitter"] = args.jitter
    out(rpc(args.host, args.port, "ash_renderer/generate_chunks", params))


def cmd_chunk_census(args):
    out(rpc(args.host, args.port, "ash_renderer/chunk_census"))


def cmd_chunk_despawn(args):
    out(rpc(args.host, args.port, "ash_renderer/chunk_despawn", {"id": args.id}))


def cmd_generate(args):
    params = {
        "count": args.count,
        "columns": args.columns,
        "spacing": args.spacing,
        "seed": args.seed,
    }
    if args.source:
        params["source"] = args.source
    if args.jitter:
        params["jitter"] = args.jitter
    out(rpc(args.host, args.port, "ash_renderer/generate_scene", params))


def cmd_despawn(args):
    out(rpc(args.host, args.port, "world.despawn_entity", {"entity": args.entity}))


def cmd_replace(args):
    params = {"entity": args.entity}
    if args.mesh_from:
        params["mesh_from"] = args.mesh_from
    if args.material_from:
        params["material_from"] = args.material_from
    out(rpc(args.host, args.port, "ash_renderer/replace_handles", params))


def cmd_remove(args):
    params = {
        "entity": args.entity,
        "components": [resolve_component(args.host, args.port, n) for n in args.components],
    }
    out(rpc(args.host, args.port, "world.remove_components", params))


def cmd_mutate(args):
    params = {"entity": args.entity}
    if args.base_color:
        params["base_color"] = [float(x) for x in args.base_color]
    if args.slot_base_color_from:
        params["slot_base_color_from"] = args.slot_base_color_from
    if args.mesh_vertex_scale is not None:
        params["mesh_vertex_scale"] = float(args.mesh_vertex_scale)
    if args.image_tint:
        params["image_tint"] = [float(x) for x in args.image_tint]
    out(rpc(args.host, args.port, "ash_renderer/mutate_asset", params))


def cmd_stats(args):
    out(rpc(args.host, args.port, "ash_renderer/increment_stats"))


def cmd_cull_stats(args):
    out(rpc(args.host, args.port, "ash_renderer/cull_stats"))


def cmd_cull_set(args):
    out(rpc(args.host, args.port, "ash_renderer/cull_set_enabled",
            {"enabled": args.enabled == "true"}))


def cmd_draw_stats(args):
    out(rpc(args.host, args.port, "ash_renderer/draw_stats"))
def cmd_workload(args):
    if args.on and args.off:
        sys.exit("brp: --on 与 --off 互斥")
    params = {}
    if args.on:
        params["enabled"] = True
    elif args.off:
        params["enabled"] = False
    if args.rows is not None:
        params["rows"] = args.rows
    if not params:
        sys.exit("brp: 需要至少一项：--on / --off / --rows")
    out(rpc(args.host, args.port, "ash_renderer/workload", params))


def cmd_perf_stats(args):
    out(rpc(args.host, args.port, "ash_renderer/perf_stats"))


def cmd_perf_samples(args):
    params = {"last": args.last} if args.last else None
    out(rpc(args.host, args.port, "ash_renderer/perf_samples", params))



def cmd_load_mesh(args):
    params = {"path": args.path}
    if args.entity and args.source:
        print("brp: entity 与 source 恰好给一个")
        sys.exit(1)
    if args.entity:
        params["entity"] = args.entity
    elif args.source:
        params["source"] = args.source
    else:
        print("brp: 需要 entity（换到既有实体）或 source（克隆挂新实体）之一")
        sys.exit(1)
    if args.offset:
        params["offset"] = args.offset
    if args.name_suffix:
        params["name_suffix"] = args.name_suffix
    out(rpc(args.host, args.port, "ash_renderer/load_mesh", params))


def cmd_load_image(args):
    out(rpc(args.host, args.port, "ash_renderer/load_image",
            {"path": args.path, "entity": args.entity}))


def cmd_reload(args):
    out(rpc(args.host, args.port, "ash_renderer/reload_asset", {"path": args.path}))


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

    p = sub.add_parser("spawn", help="克隆 primitive（ash_renderer/spawn_primitive，共享资产柄）")
    p.add_argument("--source", required=True, help="源 primitive 实体号（树内 [mesh][mat] 行）")
    p.add_argument("--offset", nargs=3, type=float, metavar=("X", "Y", "Z"), help="相对源实体的平移偏移")
    p.add_argument("--name-suffix", help="Name 后缀（默认 （副本））")
    p.set_defaults(func=cmd_spawn)

    p = sub.add_parser("spawn-shape", help="造测试体：五形状+颜色→Lambert 材质（ash_renderer/spawn_shape，6.1.1；mesh 按形状/材质按颜色缓存，同参共享零新上传）")
    p.add_argument("shape", choices=["sphere", "cylinder", "capsule", "cone", "cuboid"], help="形状（cuboid 非均匀缩放 = 墙体遮挡样本）")
    p.add_argument("--color", nargs=3, type=float, metavar=("R", "G", "B"), help="sRGB 颜色 [0,1]（默认 0.8 0.8 0.8）")
    p.add_argument("--translation", nargs=3, type=float, metavar=("X", "Y", "Z"), help="世界坐标位置（默认原点）")
    p.add_argument("--rotation", nargs=3, type=float, metavar=("YAW", "PITCH", "ROLL"), help="欧拉角 YXZ 度数（默认 0）")
    p.add_argument("--scale", nargs=3, type=float, metavar=("X", "Y", "Z"), help="非均匀缩放（默认 1 1 1）")
    p.add_argument("--name", help="树内 Name（默认 测试体-<shape>）")
    p.set_defaults(func=cmd_spawn_shape)

    p = sub.add_parser("generate", help="程序化铺实例（ash_renderer/generate_scene，5.4 常驻大场景生成器：批量克隆共享资产，种子固定可复现）")
    p.add_argument("--count", type=int, required=True, help="实例总数")
    p.add_argument("--columns", type=int, required=True, help="横向列数（行数=count 向上取整除列数；方阵给 ceil(sqrt(count))）")
    p.add_argument("--spacing", type=float, required=True, help="实例网格间距（世界单位）")
    p.add_argument("--seed", type=int, required=True, help="随机种子（抖动散列基，同参数重跑同位）")
    p.add_argument("--jitter", type=float, help="位置抖动幅度（世界单位，缺省 0=整齐网格）")
    p.add_argument("--source", help="源 primitive 实体号（缺省=场景第一个 primitive）")
    p.set_defaults(func=cmd_generate)

    p = sub.add_parser("generate-chunks", help="chunk 化批量生成（ash_renderer/generate_chunks，5.4.2：每轴 chunk_grid 块，每块一根实体+子树实例）")
    p.add_argument("--chunk-grid", type=int, required=True, help="每轴块数（总块数=chunk_grid^2）")
    p.add_argument("--instances-per-chunk", type=int, required=True, help="每块实例数（块内列数=ceil(sqrt(该值))）")
    p.add_argument("--spacing", type=float, required=True, help="实例网格间距（世界单位，块边长=块内列数×spacing）")
    p.add_argument("--seed", type=int, required=True, help="随机种子（同参数重跑同位）")
    p.add_argument("--jitter", type=float, help="位置抖动幅度（世界单位，缺省 0=整齐网格）")
    p.add_argument("--source", help="源 primitive 实体号（缺省=场景第一个 primitive）")
    p.set_defaults(func=cmd_generate_chunks)

    sub.add_parser("chunk-census", help="chunk 盘点（ash_renderer/chunk_census，只读：在场块 id/原点/边长/成员数）").set_defaults(func=cmd_chunk_census)

    p = sub.add_parser("chunk-despawn", help="整块删除（ash_renderer/chunk_despawn，按 id 找根递归 despawn；removed_rows 次帧对账）")
    p.add_argument("--id", type=int, required=True, help="目标 chunk id（chunk-census 可查）")
    p.set_defaults(func=cmd_chunk_despawn)

    p = sub.add_parser("despawn", help="删实体及其子树（官方 world.despawn_entity，递归）")
    p.add_argument("entity", help="目标实体号（树节点或 primitive）")
    p.set_defaults(func=cmd_despawn)

    p = sub.add_parser("replace", help="换柄：从源实体拷 mesh/material 柄（ash_renderer/replace_handles）")
    p.add_argument("entity", help="目标实体号")
    p.add_argument("--mesh-from", help="mesh 柄的来源实体号")
    p.add_argument("--material-from", help="material 柄的来源实体号")
    p.set_defaults(func=cmd_replace)

    p = sub.add_parser("remove", help="摘组件（官方 world.remove_components，按组件名）")
    p.add_argument("entity", help="目标实体号")
    p.add_argument("--components", nargs="+", required=True, help="要摘的组件（短名或全路径，如 Mesh3d）")
    p.set_defaults(func=cmd_remove)

    p = sub.add_parser("mutate", help="改资产内容：同 AssetId 原位变更，触发 Modified（ash_renderer/mutate_asset）")
    p.add_argument("entity", help="定位实体（取它身上的 Mesh3d/MeshMaterial3d 柄）")
    p.add_argument("--base-color", nargs=4, metavar=("R", "G", "B", "A"), help="材质 base_color（线性域 RGBA）")
    p.add_argument("--slot-base-color-from", help="换材质贴图槽：拷源实体的 base_color_texture 柄")
    p.add_argument("--mesh-vertex-scale", type=float, help="mesh 顶点绕原点均匀缩放系数")
    p.add_argument("--image-tint", nargs=3, metavar=("R", "G", "B"), help="base_color_texture 像素按通道乘的色调")
    p.set_defaults(func=cmd_mutate)

    sub.add_parser("stats", help="增量计量读数（ash_renderer/increment_stats）").set_defaults(func=cmd_stats)

    sub.add_parser("cull-stats", help="剔除读数对照：GPU 标记 vs CPU 参照逐实体 + 不一致清单（ash_renderer/cull_stats；读数陈旧 ≤2 帧，对照前等 ≥3 帧）").set_defaults(func=cmd_cull_stats)

    p = sub.add_parser("cull-set", help="剔除开关（ash_renderer/cull_set_enabled；true=compute dispatch，false=host 全可见填充——数据层 A/B）")
    p.add_argument("--enabled", required=True, choices=["true", "false"], help="开关目标态")
    p.set_defaults(func=cmd_cull_set)

    sub.add_parser("draw-stats", help="indirect 闭环读数：绘制调用数/命令条数/零实例条数/在场走访行（ash_renderer/draw_stats；读数陈旧一帧）").set_defaults(func=cmd_draw_stats)
    p = sub.add_parser("workload", help="变更形态注入开关（5.5 压测：每帧旋转迭代序前 rows 个 primitive；rows 0 = 关）（ash_renderer/workload）")
    p.add_argument("--on", action="store_true", help="开启注入")
    p.add_argument("--off", action="store_true", help="关闭注入")
    p.add_argument("--rows", type=int, help="每帧改写行数（缺省不动现值）")
    p.set_defaults(func=cmd_workload)

    sub.add_parser("perf-stats", help="帧时间分位数/CPU 分段均值/显存读数（ash_renderer/perf_stats；环形窗口 2048 帧现算）").set_defaults(func=cmd_perf_stats)

    p = sub.add_parser("perf-samples", help="原始帧样本导出（离线汇总用；ash_renderer/perf_samples）")
    p.add_argument("--last", type=int, help="取最近 N 条（缺省 256，上限 2048）")
    p.set_defaults(func=cmd_perf_samples)

    p = sub.add_parser("load-mesh", help="异步加载新 mesh：entity=换到既有实体 / source=克隆挂新实体（ash_renderer/load_mesh）")
    p.add_argument("--path", required=True, help='资产路径（如 "models/FlightHelmet/FlightHelmet.gltf#Mesh0/Primitive1"）')
    p.add_argument("--entity", help="既有实体号：换绑其 Mesh3d 柄为加载中柄")
    p.add_argument("--source", help="源 primitive 实体号：克隆结构后换柄（新对象待到货出现）")
    p.add_argument("--offset", nargs=3, type=float, metavar=("X", "Y", "Z"), help="spawn 模式相对源实体的平移偏移")
    p.add_argument("--name-suffix", help="spawn 模式 Name 后缀（默认 （异步））")
    p.set_defaults(func=cmd_load_mesh)

    p = sub.add_parser("load-image", help="异步加载新贴图换目标材质 base_color 槽（ash_renderer/load_image，共享材质全引用者同变）")
    p.add_argument("--path", required=True, help="贴图文件路径（相对资产根）")
    p.add_argument("--entity", required=True, help="定位实体（取其材质槽写入）")
    p.set_defaults(func=cmd_load_image)

    p = sub.add_parser("reload", help="手动触发资产重载（ash_renderer/reload_asset，4.4 热重载链的确定性驱动；文件监听是自动通道）")
    p.add_argument("--path", required=True, help="资产路径（相对资产根，含 glTF 子资产标签）")
    p.set_defaults(func=cmd_reload)

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
