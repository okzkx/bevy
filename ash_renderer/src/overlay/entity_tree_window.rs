//! 实体层级树窗口（业务面板，3.10/3.11）：World 全部实体按 [`Children`]/[`ChildOf`]
//! 关系递归展示，节点带 Name（无名按组件角色命名）+ 组件标签；点选节点落
//! [`SelectedEntity`] 资源，面板下部是属性区（3.11）——组件全清单、渲染语义
//! 行（Mesh/材质/贴图槽）与 Transform 编辑（3.11.2 DragValue，写经
//! [`super::transform_edit`] 封装）。
//!
//! 取数分两层：树本身走 SystemParam 直查 World（参数束样式照 [`super::debug_window`]
//! 的 `RendererStats`），不另造快照通道；属性区走 [`SelectedDetails`] 纯数据
//! 快照——采集系统（只读 &World）产、egui pass 消费，与官方"后端收集/前端展示"
//! 分工同形（用户定案：学数据组织，不学 UI 表现）。机制与证据：
//! `.agents/docs/3-静态取数链路/3.10-egui实体层级树/`、`3.11-egui实体编辑/`。

use std::any::TypeId;

use bevy::{
    asset::AssetId,
    camera::Camera,
    ecs::{
        reflect::{AppTypeRegistry, ReflectComponent},
        system::SystemParam,
        world::EntityRef,
    },
    light::DirectionalLight,
    math::EulerRot,
    pbr::{MeshMaterial3d, StandardMaterial},
    prelude::*,
    reflect::{PartialReflect, ReflectRef},
    world_serialization::WorldAssetRoot,
};

use ash_macros::system;

use super::transform_edit::TransformEdit;

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
    /// 属性区快照（3.11，采集系统产）。
    pub(super) details: &'a SelectedDetails,
}

impl EntityTreeWindow<'_> {
    /// 面板本体：递归层级树（滚动区）+ 属性区。返回一次 Transform 编辑
    /// （3.11.2，有拖改时），由 pass 落进编辑队列——本函数不碰写路径。
    pub(super) fn show(self) -> Option<(Entity, TransformEdit)> {
        let Self { ctx, open, selected, data, details } = self;
        let mut edit = None;
        egui::Window::new("实体层级树")
            .open(open)
            .default_pos(egui::pos2(250.0, 12.0))
            .default_width(360.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("实体层级树")
                    .max_height(380.0)
                    .show(ui, |ui| {
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
                edit = show_details(ui, details, selected);
            });
        edit
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

/// 选中实体属性快照（3.11.1）：采集系统每帧对选中实体产一份纯数据，属性区
/// 只渲染快照（官方"后端收集/前端展示"分工：bevy_inspector 数据层同形）。
/// 选中实体 despawn 后 `exists = false`，属性区借它清账。
#[derive(Resource, Default)]
pub(super) struct SelectedDetails {
    /// 当前选中（None = 无选中，快照为空壳）。
    entity: Option<Entity>,
    /// false = 已选中但实体不在 World（despawn 过）。
    exists: bool,
    name: Option<String>,
    children: Option<usize>,
    mesh: Option<AssetId<Mesh>>,
    material: Option<AssetId<StandardMaterial>>,
    /// 材质五贴图槽里非空的：槽名 + 图 AssetId——比官方反射展示更贴本渲染器。
    textures: Vec<(&'static str, AssetId<Image>)>,
    local: Option<TransformVals>,
    global_translation: Option<Vec3>,
    /// 组件全清单（archetype 口径，零反射全量）。
    component_rows: Vec<ComponentRow>,
}

/// Transform 三组量的纯数值形态：编辑控件直接喂快照副本。
#[derive(Clone, Copy)]
pub(super) struct TransformVals {
    pub(super) translation: [f32; 3],
    /// YXZ 欧拉角（度），与展示同口径。
    pub(super) euler_yxz_deg: [f32; 3],
    pub(super) scale: [f32; 3],
}

/// 组件清单一行：短名 + 已注册反射组件的字段只读行（None = 名字-only）。
pub(super) struct ComponentRow {
    name: String,
    fields: Option<Vec<(String, String)>>,
}

/// 从材质取一个可选图柄（贴图槽表第二列的函数形状，抽型免 type_complexity）。
type TextureSlotGet = fn(&StandardMaterial) -> &Option<Handle<Image>>;

/// 材质五贴图槽（bevy_pbr/src/pbr_material.rs 的 StandardMaterial 字段）：
/// (展示名, 取柄)。
const TEXTURE_SLOTS: &[(&str, TextureSlotGet)] = &[
    ("base_color", |m| &m.base_color_texture),
    ("emissive", |m| &m.emissive_texture),
    ("metallic_roughness", |m| &m.metallic_roughness_texture),
    ("normal", |m| &m.normal_map_texture),
    ("occlusion", |m| &m.occlusion_texture),
];

/// 反射行跳过清单：这些组件已有精确专行（Transform 编辑/Name 头/Mesh/材质/
/// 贴图槽/树内层级），重复上反射行只添噪。
const SKIP_REFLECT: &[TypeId] = &[
    TypeId::of::<Transform>(),
    TypeId::of::<GlobalTransform>(),
    TypeId::of::<Name>(),
    TypeId::of::<Mesh3d>(),
    TypeId::of::<MeshMaterial3d<StandardMaterial>>(),
    TypeId::of::<Children>(),
    TypeId::of::<ChildOf>(),
];

/// 属性快照采集（3.11.1）：每帧对选中实体产一份纯数据（单实体量级，全帧
/// 成本可忽略）。**独占系统（&mut World）直接写资源**——首版用只读 &World +
/// Commands 落账，实跑发现 deferred 命令在本链上从不生效（快照恒为默认值，
/// 无人验证轮日志实抓），改独占后直接写、零不确定性；链在 egui pass 之前。
#[system]
pub(super) fn collect_selected_details(world: &mut World) {
    let Some(entity) = world.resource::<SelectedEntity>().0 else {
        world.insert_resource(SelectedDetails::default());
        return;
    };
    let mut details = SelectedDetails {
        entity: Some(entity),
        exists: true,
        ..Default::default()
    };
    let Some(er) = world.get_entity(entity).ok() else {
        details.exists = false;
        world.insert_resource(details);
        return;
    };
    details.name = er.get::<Name>().map(|n| n.as_str().to_owned());
    details.children = er.get::<Children>().map(|c| c.len());
    details.mesh = er.get::<Mesh3d>().map(|m| m.0.id());
    let mat_id = er.get::<MeshMaterial3d<StandardMaterial>>().map(|m| m.0.id());
    details.material = mat_id;
    let mat = mat_id.and_then(|id| {
        world
            .get_resource::<Assets<StandardMaterial>>()
            .and_then(|assets| assets.get(id))
    });
    if let Some(mat) = mat {
        details.textures = TEXTURE_SLOTS
            .iter()
            .filter_map(|(label, get)| get(mat).as_ref().map(|h| (*label, h.id())))
            .collect();
    }
    if let Some(t) = er.get::<Transform>() {
        let (yaw, pitch, roll) = t.rotation.to_euler(EulerRot::YXZ);
        details.local = Some(TransformVals {
            translation: t.translation.to_array(),
            euler_yxz_deg: [yaw, pitch, roll].map(|r| r.to_degrees()),
            scale: t.scale.to_array(),
        });
    }
    details.global_translation = er.get::<GlobalTransform>().map(|g| g.translation());
    details.component_rows = component_rows(world, &er);
    world.insert_resource(details);
}

/// 组件全清单：archetype 口径（`EntityRef::archetype().components()`），零反射
/// 全量；已注册反射的组件附字段只读行。
fn component_rows(world: &World, er: &EntityRef) -> Vec<ComponentRow> {
    let components = world.components();
    let Some(registry) = world.get_resource::<AppTypeRegistry>() else {
        // 注册表缺席（不该发生：register_type 已懒插）——退化只报名字（Tier①）
        return er
            .archetype()
            .components()
            .iter()
            .filter_map(|cid| components.get_info(*cid))
            .map(|info| ComponentRow {
                name: info.name().shortname().to_string(),
                fields: None,
            })
            .collect();
    };
    let registry = registry.read();
    er.archetype()
        .components()
        .iter()
        .filter_map(|cid| components.get_info(*cid))
        .map(|info| {
            let mut row = ComponentRow {
                name: info.name().shortname().to_string(),
                fields: None,
            };
            let Some(tid) = info.type_id() else {
                return row;
            };
            if SKIP_REFLECT.contains(&tid) {
                return row; // 已有精确专行，不上反射行
            }
            let Some(entry) = registry.get(tid) else {
                return row;
            };
            let Some(rc) = entry.data::<ReflectComponent>() else {
                return row;
            };
            if let Some(reflected) = rc.reflect(*er) {
                row.fields = Some(reflect_field_rows(reflected));
            }
            row
        })
        .collect()
}

/// 反射字段只读行（ReflectRef 分发）：Struct 拆逐字段行，其余整值一行。
/// dyn PartialReflect 自带 Debug（bevy_reflect/src/reflect.rs:517），直接格式化。
fn reflect_field_rows(v: &dyn PartialReflect) -> Vec<(String, String)> {
    match v.reflect_ref() {
        ReflectRef::Struct(s) => (0..s.field_len())
            .filter_map(|i| {
                let field = s.field_at(i)?;
                Some((s.name_at(i)?.to_owned(), format!("{field:?}")))
            })
            .collect(),
        _ => vec![("值".to_owned(), format!("{v:?}"))],
    }
}

/// 属性区（3.11）：快照渲染（头部/渲染语义/全局平移/组件清单）+ Transform
/// 编辑（3.11.2 DragValue 读快照副本）。返回一次编辑（有拖改时），写路径归
/// [`super::transform_edit`]——本函数零 World 访问，纯渲染。
fn show_details(
    ui: &mut egui::Ui,
    details: &SelectedDetails,
    selected: &mut SelectedEntity,
) -> Option<(Entity, TransformEdit)> {
    ui.separator();
    let Some(entity) = details.entity else {
        ui.weak("点选节点查看属性（编辑目标从这里来）");
        return None;
    };
    if !details.exists {
        selected.0 = None;
        ui.weak("（选中实体已不存在，已清空选择）");
        return None;
    }
    ui.strong(match &details.name {
        Some(name) => format!("选中：{name}"),
        None => format!("选中：实体 #{}", entity.index()),
    });
    if let Some(children) = details.children {
        ui.monospace(format!("子节点 {children}"));
    }
    if let Some(mesh) = details.mesh {
        ui.monospace(format!("Mesh   {mesh:?}"));
    }
    if let Some(mat) = details.material {
        ui.monospace(format!("材质   {mat:?}"));
    }
    for (label, image) in &details.textures {
        ui.monospace(format!("贴图   {label}  {image:?}"));
    }
    // Transform 编辑（3.11.2）：读快照值进副本，任一量被拖改即整组回写（未动
    // 的组发原值，覆盖等价于原地）
    let mut edit = None;
    if let Some(t) = details.local {
        let (mut translation, mut euler, mut scale) = (t.translation, t.euler_yxz_deg, t.scale);
        egui::Grid::new("transform_edit").num_columns(4).show(ui, |ui| {
            ui.monospace("平移");
            drag_vec3(ui, &mut translation, 0.05, "");
            ui.end_row();
            ui.monospace("旋转");
            drag_vec3(ui, &mut euler, 1.0, "°");
            ui.end_row();
            ui.monospace("缩放");
            drag_vec3(ui, &mut scale, 0.02, "");
            ui.end_row();
        });
        if translation != t.translation || euler != t.euler_yxz_deg || scale != t.scale {
            edit = Some((
                entity,
                TransformEdit {
                    translation: Some(Vec3::from_array(translation)),
                    rotation_euler_deg: Some(Vec3::from_array(euler)),
                    scale: Some(Vec3::from_array(scale)),
                },
            ));
        }
    }
    if let Some(g) = details.global_translation {
        ui.monospace(format!("全局  平移 ({:+.3}, {:+.3}, {:+.3})", g.x, g.y, g.z));
    }
    // 组件全清单（3.11.1）：archetype 口径；注册的折叠出字段行，未注册只报名字
    ui.separator();
    ui.strong(format!("组件 {}", details.component_rows.len()));
    egui::ScrollArea::vertical()
        .id_salt("组件清单")
        .max_height(260.0)
        .show(ui, |ui| {
        for row in &details.component_rows {
            match &row.fields {
                Some(fields) => {
                    egui::CollapsingHeader::new(egui::RichText::new(&row.name).monospace())
                        .id_salt(row.name.as_str())
                        .default_open(false)
                        .show(ui, |ui| {
                            for (fname, fvalue) in fields {
                                ui.horizontal(|ui| {
                                    ui.monospace(fname.as_str());
                                    ui.weak(fvalue.as_str());
                                });
                            }
                        });
                }
                None => {
                    ui.monospace(row.name.as_str());
                }
            }
        }
    });
    edit
}

/// 三分量 DragValue 行（平移/旋转/缩放共用）：speed 为拖拽灵敏度，suffix 为
/// 角度标。
fn drag_vec3(ui: &mut egui::Ui, v: &mut [f32; 3], speed: f32, suffix: &str) {
    ui.horizontal(|ui| {
        for x in v {
            ui.add(
                egui::DragValue::new(x)
                    .speed(speed)
                    .suffix(suffix)
                    .max_decimals(if suffix.is_empty() { 3 } else { 1 }),
            );
        }
    });
}
