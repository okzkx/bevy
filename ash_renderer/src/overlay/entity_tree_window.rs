//! 实体层级树窗口（业务面板，3.10）：World 全部实体按 [`Children`]/[`ChildOf`]
//! 关系递归展示，节点带 Name（无名按组件角色命名）+ 组件标签；点选节点落
//! [`SelectedEntity`] 资源，面板下部显示选中实体的柄与 Transform 终值——3.11
//! 编辑面板的目标从这里来。
//!
//! 取数走 SystemParam 直查 World（参数束样式照 [`super::debug_window`] 的
//! `RendererStats`），不另造快照通道——增删实体由查询每帧自然反映，本窗口
//! 零缓存态（唯一状态 = 点选）。机制与证据：
//! `.agents/docs/3-静态取数链路/3.10-egui实体层级树/`。

use bevy::{
    camera::Camera,
    ecs::system::SystemParam,
    light::DirectionalLight,
    math::EulerRot,
    pbr::{MeshMaterial3d, StandardMaterial},
    prelude::*,
    world_serialization::WorldAssetRoot,
};

/// 树内点选的实体（3.10 落账）。选中实体 despawn 后由详情区检空清账，
/// 不残留死引用；3.11 的 Transform 编辑以它为目标。
#[derive(Resource, Default)]
pub(super) struct SelectedEntity(pub(super) Option<Entity>);

/// 层级树一行的查询数据（11 项组合体抽型，免 clippy type_complexity）：
/// 层级关系（Children/ChildOf）+ 角色标记（相机/光/资产根）+ 呈现要素
/// （Name/Mesh/Material/两 Transform），逐项可缺。
type EntityRow = (
    Entity,
    Option<&'static Name>,
    Option<&'static Children>,
    Has<ChildOf>,
    Has<Camera>,
    Has<DirectionalLight>,
    Option<&'static Mesh3d>,
    Option<&'static MeshMaterial3d<StandardMaterial>>,
    Has<WorldAssetRoot>,
    Option<&'static Transform>,
    Option<&'static GlobalTransform>,
);

/// 层级树一次取数（SystemParam 束）：全部实体的层级与呈现要素一次查询拿全。
/// 逐项 Option/Has——非场景实体（窗口/相机/灯光/资产根）也进树，缺什么降级什么；
/// 单查询即可完成根定位（`Has<ChildOf>`）与按实体号取行（`.get`），无需第二查询。
#[derive(SystemParam)]
pub(super) struct EntityTreeData<'w, 's> {
    entities: Query<'w, 's, EntityRow>,
}

/// 层级树窗口的一次构建输入：框架 pass 每帧组好（借用样式照 [`super::debug_window`]）。
pub(super) struct EntityTreeWindow<'a> {
    pub(super) ctx: &'a egui::Context,
    /// 显隐开关：总控 checkbox 与窗口 [×] 写同一字段（egui `Window::open` 借用）。
    pub(super) open: &'a mut bool,
    /// 点选状态（资源借用，点选就地落账）。
    pub(super) selected: &'a mut SelectedEntity,
    /// 本帧 World 查询束。
    pub(super) data: &'a EntityTreeData<'a, 'a>,
}

impl EntityTreeWindow<'_> {
    /// 面板本体：递归层级树（滚动区）+ 选中实体详情。
    pub(super) fn show(self) {
        let Self { ctx, open, selected, data } = self;
        egui::Window::new("实体层级树")
            .open(open)
            .default_pos(egui::pos2(250.0, 12.0))
            .default_width(360.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                    // 根 = 无 ChildOf 的相关实体；查询序随 archetype 波动，按实体号排稳
                    let mut roots: Vec<Entity> = Vec::new();
                    let (mut relevant, mut total) = (0usize, 0usize);
                    for (entity, name, children, has_parent, cam, light, mesh, mat, root, local, global) in
                        &data.entities
                    {
                        total += 1;
                        if is_relevant(name, children, has_parent, cam, light, mesh, mat, root, local, global) {
                            relevant += 1;
                            if !has_parent {
                                roots.push(entity);
                            }
                        }
                    }
                    ui.monospace(format!("相关 {relevant} / 全部 {total}"));
                    roots.sort_by_key(|e| e.index());
                    for root in roots {
                        draw_entity(ui, root, data, &mut selected.0, 0);
                    }
                });
                show_details(ui, data, selected);
            });
    }
}

/// 相关性过滤：至少带一个被关注组件（名字/层级/相机/灯/mesh/材质/变换）才进树。
/// DefaultPlugins 注册的数百个 observer 等内部裸实体会淹没场景树，不显示；
/// 计数行"相关 X / 全部 Y"如实报出被滤掉的量。
#[expect(
    clippy::too_many_arguments,
    reason = "过滤谓词逐项对应 EntityRow 查询元组的字段，收拢成结构体只是换个地方数数"
)]
fn is_relevant(
    name: Option<&Name>,
    children: Option<&Children>,
    has_parent: bool,
    cam: bool,
    light: bool,
    mesh: Option<&Mesh3d>,
    mat: Option<&MeshMaterial3d<StandardMaterial>>,
    root: bool,
    local: Option<&Transform>,
    global: Option<&GlobalTransform>,
) -> bool {
    name.is_some()
        || children.is_some()
        || has_parent
        || cam
        || light
        || mesh.is_some()
        || mat.is_some()
        || root
        || local.is_some()
        || global.is_some()
}

/// 递归画一个节点：有子挂折叠头（点击头 = 展开/收起 + 选中），无子挂可选行。
/// 折叠状态由 egui 按 `id_salt(entity)` 逐实体记忆，跨帧稳定。
fn draw_entity(
    ui: &mut egui::Ui,
    entity: Entity,
    data: &EntityTreeData,
    selected: &mut Option<Entity>,
    depth: usize,
) {
    // 悬空引用兜底：despawn 会同步从父链摘除，正常到不了这里
    let Ok((e, name, children, has_parent, cam, light, mesh, mat, root, local, global)) =
        data.entities.get(entity)
    else {
        return;
    };
    if !is_relevant(name, children, has_parent, cam, light, mesh, mat, root, local, global) {
        return; // 不相关子树（无被关注组件，也无子可递归）整枝跳过
    }
    let label = node_label(e, name, cam, light, mesh.is_some(), mat.is_some(), root);
    let is_selected = *selected == Some(entity);
    if let Some(children) = children {
        let resp = egui::CollapsingHeader::new(header_text(&label, is_selected))
            .id_salt(entity)
            .default_open(depth < 2)
            .show(ui, |ui| {
                for child in children.iter() {
                    draw_entity(ui, child, data, selected, depth + 1);
                }
            });
        if resp.header_response.clicked() {
            *selected = Some(entity);
        }
    } else if ui.selectable_label(is_selected, &label).clicked() {
        *selected = Some(entity);
    }
}

/// 节点行文本：Name 优先，无名按组件角色命名（相机/方向光/资产根），实体号
/// 恒显（glTF 同名节点可区分），尾缀组件标签。
fn node_label(
    entity: Entity,
    name: Option<&Name>,
    cam: bool,
    light: bool,
    mesh: bool,
    mat: bool,
    root: bool,
) -> String {
    let role = if let Some(name) = name {
        name.as_str().to_owned()
    } else if cam {
        "相机".to_owned()
    } else if light {
        "方向光".to_owned()
    } else if root {
        "WorldAssetRoot".to_owned()
    } else {
        "实体".to_owned()
    };
    let mut label = format!("{role} #{}", entity.index());
    if mesh {
        label.push_str("  [mesh]");
    }
    if mat {
        label.push_str("  [mat]");
    }
    label
}

/// 折叠头文本：选中项染色（叶子行走 selectable_label 自带高亮）。
fn header_text(label: &str, is_selected: bool) -> egui::RichText {
    if is_selected {
        egui::RichText::new(label).color(egui::Color32::from_rgb(0x82, 0xC4, 0xE8))
    } else {
        egui::RichText::new(label)
    }
}

/// 选中实体详情：柄、层级与 Transform 终值（3.11 编辑面板的数据底稿）。
fn show_details(ui: &mut egui::Ui, data: &EntityTreeData, selected: &mut SelectedEntity) {
    ui.separator();
    let Some(entity) = selected.0 else {
        ui.weak("点选节点查看详情（3.11 编辑目标从这里来）");
        return;
    };
    let Ok((_, name, children, _, _, _, mesh, mat, _, local, global)) =
        data.entities.get(entity)
    else {
        selected.0 = None;
        ui.weak("（选中实体已不存在，已清空选择）");
        return;
    };
    ui.strong(match name {
        Some(name) => format!("选中：{}", name.as_str()),
        None => format!("选中：实体 #{}", entity.index()),
    });
    if let Some(children) = children {
        ui.monospace(format!("子节点 {}", children.len()));
    }
    if let Some(mesh) = mesh {
        ui.monospace(format!("Mesh   {:?}", mesh.0.id()));
    }
    if let Some(mat) = mat {
        ui.monospace(format!("材质   {:?}", mat.0.id()));
    }
    if let Some(t) = local {
        let (yaw, pitch, roll) = t.rotation.to_euler(EulerRot::YXZ);
        ui.monospace(format!(
            "局部  平移 ({:+.3}, {:+.3}, {:+.3})",
            t.translation.x, t.translation.y, t.translation.z
        ));
        ui.monospace(format!(
            "      旋转 Y{:+.1}° X{:+.1}° Z{:+.1}°  缩放 ({:.2}, {:.2}, {:.2})",
            yaw.to_degrees(),
            pitch.to_degrees(),
            roll.to_degrees(),
            t.scale.x,
            t.scale.y,
            t.scale.z
        ));
    }
    if let Some(g) = global {
        ui.monospace(format!(
            "全局  平移 ({:+.3}, {:+.3}, {:+.3})",
            g.translation().x,
            g.translation().y,
            g.translation().z
        ));
    }
}
