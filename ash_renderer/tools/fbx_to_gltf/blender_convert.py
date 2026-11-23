"""在 Blender 内执行 FBX -> glTF（GLTF_SEPARATE 布局）。

由 fbx2gltf.py 调用，不单独直跑：
    blender --background --factory-startup --python blender_convert.py -- <in.fbx> <out.gltf>

材质修正规则来自 DGriffin91/bevy_bistro_scene 的处理清单：
- metallic：FBX 导出的 metallic 普遍不可信，除名字含 metal 的一律清零；
- alpha：Alpha 接常值 1 走 OPAQUE；植被的贴图 alpha 串 Math:ROUND 走 MASK；
  玻璃/液体等保持原链，由后处理按贴图 alpha 直方图再裁决。
"""

import os
import sys

import bpy

FOLIAGE_WORDS = (
    "leaf", "leaves", "foliage", "ivy", "vine", "hedge",
    "grass", "flower", "weed", "plant", "bush", "branch",
)
BLEND_WORDS = ("glass", "liquid", "ice", "beer", "wine", "water")


def parse_args():
    argv = sys.argv[sys.argv.index("--") + 1:] if "--" in sys.argv else []
    if len(argv) != 2:
        raise SystemExit(
            "用法: blender --background --factory-startup --python blender_convert.py -- <in.fbx> <out.gltf>"
        )
    return os.path.abspath(argv[0]), os.path.abspath(argv[1])


def principled_of(material):
    for node in material.node_tree.nodes:
        if node.type == "BSDF_PRINCIPLED":
            return node
    return None


def fix_materials():
    """按清单改 metallic 与 alpha 接线；返回统计供日志。"""
    fixed_metal, masked, kept_blend, specular_linked = [], [], [], []
    for material in bpy.data.materials:
        if not material.use_nodes:
            continue
        principled = principled_of(material)
        if principled is None:
            continue
        low = material.name.lower()

        want = 1.0 if "metal" in low else 0.0
        if abs(principled.inputs["Metallic"].default_value - want) > 1e-6:
            principled.inputs["Metallic"].default_value = want
            fixed_metal.append(material.name)

        alpha_in = principled.inputs["Alpha"]
        try:
            if alpha_in.is_linked and any(word in low for word in FOLIAGE_WORDS):
                # 贴图驱动的 alpha 才做 MASK；没链的植被（树干等）落回不透明分支
                source = alpha_in.links[0].from_socket
                for link in list(alpha_in.links):
                    material.node_tree.links.remove(link)
                round_node = material.node_tree.nodes.new("ShaderNodeMath")
                round_node.operation = "ROUND"
                material.node_tree.links.new(source, round_node.inputs[0])
                material.node_tree.links.new(round_node.outputs[0], alpha_in)
                masked.append(material.name)
            elif any(word in low for word in BLEND_WORDS):
                kept_blend.append(material.name)  # 保持 FBX 原样，交给后处理裁决
            else:
                for link in list(alpha_in.links):
                    material.node_tree.links.remove(link)
                alpha_in.default_value = 1.0
        except Exception as error:
            print(f"[conv] warn: alpha not fixed for {material.name}: {error}")

        specular_in = principled.inputs.get("Specular IOR Level")
        if specular_in is not None and specular_in.is_linked:
            specular_linked.append(material.name)

    return fixed_metal, masked, kept_blend, specular_linked


def report_images():
    missing = [
        image.name
        for image in bpy.data.images
        if image.source == "FILE" and not os.path.exists(bpy.path.abspath(image.filepath))
    ]
    print(f"[conv] images={len(bpy.data.images)} missing={len(missing)}")
    for name in missing[:20]:
        print(f"[conv]   missing image: {name}")


def report_meshes():
    objects = [obj for obj in bpy.context.scene.objects if obj.type == "MESH"]
    tris = sum(len(obj.data.polygons) for obj in objects)
    print(f"[conv] mesh objects={len(objects)} unique meshes={len(bpy.data.meshes)} polys={tris}")


def main():
    in_fbx, out_gltf = parse_args()
    os.makedirs(os.path.dirname(out_gltf), exist_ok=True)

    bpy.ops.wm.read_factory_settings(use_empty=True)
    print(f"[conv] importing {in_fbx}")
    bpy.ops.import_scene.fbx(filepath=in_fbx)

    fixed_metal, masked, kept_blend, specular_linked = fix_materials()
    print(
        f"[conv] materials total={len(bpy.data.materials)} "
        f"metallic fixed={len(fixed_metal)} mask={len(masked)} blend-kept={len(kept_blend)} "
        f"specular-linked={len(specular_linked)}"
    )
    for name in (fixed_metal + masked + kept_blend + specular_linked)[:20]:
        print(f"[conv]   material: {name}")
    if specular_linked:
        print("[conv] 注意：Specular 贴图会被导出成 KHR_materials_specular，随后由 fix_gltf_orm.py 改接")

    report_images()
    report_meshes()

    print(f"[conv] exporting {out_gltf}")
    bpy.ops.export_scene.gltf(
        filepath=out_gltf,
        export_format="GLTF_SEPARATE",
        export_image_format="AUTO",
        export_yup=True,
        export_animations=False,
    )
    print("[conv] DONE")


if __name__ == "__main__":
    main()
