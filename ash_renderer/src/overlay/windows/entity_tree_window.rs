//! 实体层级树窗口（业务面板）：World 全部实体按 [`Children`]/[`ChildOf`]
//! 关系递归展示，节点带 Name（无名按组件角色命名）+ 组件标签；点选节点落
//! [`SelectedEntity`] 资源，面板下部是属性区——组件全清单、渲染语义
//! 行（Mesh/材质/贴图槽）与 Transform 编辑（DragValue，写经
//! [`crate::overlay::transform_edit`] 封装）。
//!
//! 取数分两层：树本身走 SystemParam 直查 World（参数束样式照 [`super::debug_window`]
//! 的 `RendererStats`），不另造快照通道；属性区走 [`SelectedDetails`] 纯数据
//! 快照——采集系统（只读 &World）产、egui pass 消费，与官方"后端收集/前端展示"
//! 分工同形（用户定案：学数据组织，不学 UI 表现）。机制与证据：
//! `.agents/docs/3-静态取数链路/3.10-egui实体层级树/`、`3.11-egui实体编辑/`。

use std::any::TypeId;
use std::collections::{HashMap, HashSet};

use bevy::{
    asset::AssetId,
    camera::Camera,
    ecs::{
        lifecycle::{DespawnEvent, InsertEvent, RemoveEvent},
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

use crate::overlay::transform_edit::TransformEdit;

/// 树内点选的实体。选中实体 despawn 后由详情区检空清账，
/// 不残留死引用；Transform 编辑以它为目标。
#[derive(Resource, Default)]
pub(in crate::overlay) struct SelectedEntity(pub(in crate::overlay) Option<Entity>);

/// 层级树一行的查询数据（11 项组合体抽型，免 clippy type_complexity）：
/// 层级关系（Children/ChildOf）+ 角色标记（相机/光/资产根）+ 呈现要素
/// （Name/Mesh/Material/两 Transform），逐项可缺。BRP `scene_tree`
/// 复用同一行形状——两侧看到同一棵树。
pub(crate) type EntityRow = (
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
/// 7.7 起只被树拍平维护系统消费（重建时全量走访）；egui pass 不再逐帧查询。
#[derive(SystemParam)]
pub(in crate::overlay) struct EntityTreeData<'w, 's> {
    entities: Query<'w, 's, EntityRow>,
}

/// 拍平树一行：等高行数组元素。`depth` 只用于缩进，`expanded` 在重建时
/// 从展开态账读出（行渲染不再自持折叠状态）。
pub(in crate::overlay) struct TreeRow {
    entity: Entity,
    depth: usize,
    label: String,
    /// 有子节点（渲染折叠形态；点击 = 展开/收起 + 选中，与历史一致）。
    has_children: bool,
    /// 展开态（has_children 时有意义）。
    expanded: bool,
}

/// 树拍平缓存（7.7）：展开状态自持 + 脏信号才重建。稳态每帧零重建，
/// egui pass 只读 [`EntityTreeState::rows`] 切片（`ScrollArea::show_rows`
/// 只构建视口内的行）——pass 成本与实体总数解耦。
///
/// 脏信号三观测器（Insert/Remove/Despawn 裸事件 = 实体增删/换柄/层级/标签
/// 形态任一变化）置 [`EntityTreeState::dirty`]；展开/收起由 UI 写展开态账
/// 后同样置脏。Name 的 Mut 改写不走 Insert，不触发重建（本工程 Name 只在
/// 进场/BRP spawn 时写入，均走 Insert）。
#[derive(Resource)]
pub(in crate::overlay) struct EntityTreeState {
    rows: Vec<TreeRow>,
    relevant: usize,
    total: usize,
    dirty: bool,
    /// 展开态账（按实体）：首次见到按历史默认（深度 < 2 展开）落账，
    /// 之后跨重建保持。死实体在重建时清扫。
    expanded: HashMap<Entity, bool>,
}

impl Default for EntityTreeState {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            relevant: 0,
            total: 0,
            dirty: true,
            expanded: HashMap::new(),
        }
    }
}

impl EntityTreeState {
    /// 重建拍平行数组：全量走访相关实体（形状同 BRP `scene_tree` 的
    /// relevant 表），只收展开节点成等高行。与 scene_tree 共用
    /// `is_relevant`/`node_label`——单实现，CLI 树与面板树同源。
    fn rebuild(&mut self, rows: &Query<EntityRow>) {
        // 相关实体表：entity → (节点文本, 子实体清单, 是否有父)
        let mut relevant: HashMap<Entity, (String, Vec<Entity>, bool)> = HashMap::new();
        let mut total = 0usize;
        for (entity, name, children, has_parent, cam, light, mesh, mat, root, local, global) in rows
        {
            total += 1;
            if is_relevant(name, children, has_parent, cam, light, mesh, mat, root, local, global)
            {
                relevant.insert(
                    entity,
                    (
                        node_label(entity, name, cam, light, mesh.is_some(), mat.is_some(), root),
                        children.map(|c| c.iter().collect()).unwrap_or_default(),
                        has_parent,
                    ),
                );
            }
        }
        let mut roots: Vec<Entity> = relevant
            .iter()
            .filter(|(_, (_, _, has_parent))| !*has_parent)
            .map(|(entity, _)| *entity)
            .collect();
        roots.sort_by_key(|e| e.index());
        self.rows.clear();
        let mut seen: HashSet<Entity> = HashSet::new();
        for root in &roots {
            self.flatten_row(*root, 0, &relevant, &mut seen);
        }
        self.expanded.retain(|e, _| seen.contains(e));
        self.relevant = relevant.len();
        self.total = total;
    }

    /// 递归拍平一个相关子树：只收展开节点；不相关子树整枝跳过（与
    /// egui 树/CLI 树同语义）。深度越限按病态深树拒绝续走（同 scene_tree
    /// 的栈防线理由）。
    fn flatten_row(
        &mut self,
        entity: Entity,
        depth: usize,
        relevant: &HashMap<Entity, (String, Vec<Entity>, bool)>,
        seen: &mut HashSet<Entity>,
    ) {
        if depth > TREE_MAX_DEPTH || !seen.insert(entity) {
            return;
        }
        let Some((label, children, _)) = relevant.get(&entity) else {
            return;
        };
        let has_children = !children.is_empty();
        let expanded = if has_children {
            *self.expanded.entry(entity).or_insert(depth < 2)
        } else {
            false
        };
        self.rows.push(TreeRow {
            entity,
            depth,
            label: label.clone(),
            has_children,
            expanded,
        });
        if expanded {
            for &child in children {
                self.flatten_row(child, depth + 1, relevant, seen);
            }
        }
    }
}

/// 递归深度上限：病态深树在栈溢出前显式停走（理由同 BRP `scene_tree`
/// 的 `TREE_MAX_DEPTH`，栈溢出走 abort 是"禁 panic"规则下唯一不可兜底路径）。
const TREE_MAX_DEPTH: usize = 1024;

/// 脏信号观测器（7.7）：任一组件 Insert（含实体 spawn 与换柄）/Remove/
/// Despawn 都可能改树形态（增删/层级/标签/相关性）——置脏即收，重建交给
/// 维护系统统一做。稳态（无事件）零成本。
pub(in crate::overlay) fn mark_tree_dirty_insert(_: On<InsertEvent>, st: Option<ResMut<EntityTreeState>>) {
    if let Some(mut st) = st {
        st.dirty = true;
    }
}

pub(in crate::overlay) fn mark_tree_dirty_remove(_: On<RemoveEvent>, st: Option<ResMut<EntityTreeState>>) {
    if let Some(mut st) = st {
        st.dirty = true;
    }
}

pub(in crate::overlay) fn mark_tree_dirty_despawn(_: On<DespawnEvent>, st: Option<ResMut<EntityTreeState>>) {
    if let Some(mut st) = st {
        st.dirty = true;
    }
}

/// 树拍平维护系统（Update，egui pass 前）：脏信号才全量重建，稳态直接
/// 返回——`Query` 不迭代、零分配，每帧成本与实体总数解耦。
#[system]
pub(in crate::overlay) fn maintain_entity_tree(
    mut st: ResMut<EntityTreeState>,
    rows: EntityTreeData,
) {
    if !st.dirty {
        return;
    }
    st.rebuild(&rows.entities);
    st.dirty = false;
}

/// 层级树窗口的一次构建输入：框架 pass 每帧组好（借用样式照 [`super::debug_window`]）。
pub(in crate::overlay) struct EntityTreeWindow<'a> {
    pub(in crate::overlay) ctx: &'a egui::Context,
    /// 显隐开关：总控 checkbox 与窗口 [×] 写同一字段（egui `Window::open` 借用）。
    pub(in crate::overlay) open: &'a mut bool,
    /// 点选状态（资源借用，点选就地落账）。
    pub(in crate::overlay) selected: &'a mut SelectedEntity,
    /// 树拍平缓存（7.7）：维护系统产，pass 只读行切片；展开/收起写回。
    pub(in crate::overlay) state: &'a mut EntityTreeState,
    /// 属性区快照（采集系统产）。
    pub(in crate::overlay) details: &'a SelectedDetails,
    /// 旋转拖拽缓存（拖拽期内冻结欧拉表示分支）。
    pub(in crate::overlay) drag: &'a mut TransformDragCache,
}

impl EntityTreeWindow<'_> {
    /// 面板本体：拍平行数组的视口裁剪渲染（`show_rows` 只构建可见行，
    /// 与实体总数解耦）+ 属性区。返回一次 Transform 编辑（有拖改时），
    /// 由 pass 落进编辑队列——本函数不碰写路径（展开态账与本缓存除外）。
    pub(in crate::overlay) fn show(self) -> Option<(Entity, TransformEdit)> {
        let Self { ctx, open, selected, state, details, drag } = self;
        let mut edit = None;
        egui::Window::new("实体层级树")
            .open(open)
            .default_pos(egui::pos2(250.0, 12.0))
            .default_width(360.0)
            .show(ctx, |ui| {
                ui.monospace(format!("相关 {} / 全部 {}", state.relevant, state.total));
                // 等高行高：可交互行（selectable_label）以 interact_size 为下限，
                // 取两者大者保证逐行 stride 与声明一致（滚动算术不失真）。
                let row_height = ui
                    .spacing()
                    .interact_size
                    .y
                    .max(ui.text_style_height(&egui::TextStyle::Monospace));
                // 本帧点中的折叠行（展开/收起）——闭包只读行切片，写回在
                // 闭包外做（展开态账 + 置脏，下帧重建生效）。
                let mut toggles: Vec<Entity> = Vec::new();
                egui::ScrollArea::vertical()
                    .id_salt("实体层级树")
                    .max_height(380.0)
                    .show_rows(ui, row_height, state.rows.len(), |ui, range| {
                        for i in range {
                            let TreeRow { entity, depth, label, has_children, expanded } =
                                &state.rows[i];
                            let is_selected = selected.0 == Some(*entity);
                            let indent = egui::vec2(*depth as f32 * ROW_INDENT, 0.0);
                            ui.horizontal(|ui| {
                                ui.set_height(row_height);
                                ui.add_space(indent.x);
                                if *has_children {
                                    let arrow = if *expanded { "▼ " } else { "▶ " };
                                    if ui
                                        .selectable_label(is_selected, format!("{arrow}{label}"))
                                        .clicked()
                                    {
                                        selected.0 = Some(*entity);
                                        toggles.push(*entity);
                                    }
                                } else if ui.selectable_label(is_selected, label).clicked() {
                                    selected.0 = Some(*entity);
                                }
                            });
                        }
                    });
                for entity in toggles {
                    let open = state.expanded.entry(entity).or_insert(true);
                    *open = !*open;
                    state.dirty = true;
                }
                edit = show_details(ui, details, selected, drag);
            });
        edit
    }
}

/// 每层缩进宽度（points 域，照历史 CollapsingHeader 的视觉层级感）。
const ROW_INDENT: f32 = 14.0;

/// 相关性过滤：至少带一个被关注组件（名字/层级/相机/灯/mesh/材质/变换）才进树。
/// DefaultPlugins 注册的数百个 observer 等内部裸实体会淹没场景树，不显示；
/// 计数行"相关 X / 全部 Y"如实报出被滤掉的量。
#[expect(
    clippy::too_many_arguments,
    reason = "过滤谓词逐项对应 EntityRow 查询元组的字段，收拢成结构体只是换个地方数数"
)]
pub(crate) fn is_relevant(
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

/// 节点行文本：Name 优先，无名按组件角色命名（相机/方向光/资产根），实体号
/// 恒显（glTF 同名节点可区分），尾缀组件标签。BRP `scene_tree`
/// 复用——CLI 树的行文本与 egui 树一致。
pub(crate) fn node_label(
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

/// 选中实体属性快照：采集系统每帧对选中实体产一份纯数据，属性区
/// 只渲染快照（官方"后端收集/前端展示"分工：bevy_inspector 数据层同形）。
/// 选中实体 despawn 后 `exists = false`，属性区借它清账。
#[derive(Resource, Default)]
pub(in crate::overlay) struct SelectedDetails {
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
pub(in crate::overlay) struct TransformVals {
    pub(in crate::overlay) translation: [f32; 3],
    /// YXZ 欧拉角（度），与展示同口径。
    pub(in crate::overlay) euler_yxz_deg: [f32; 3],
    pub(in crate::overlay) scale: [f32; 3],
}

/// 旋转拖拽缓存：按住的整段拖拽期内欧拉三元组冻结在按下帧的表示分支，
/// 拖拽框只改被拖的那位。若每帧从快照重建，拖拽分量跨欧拉分解分支边界
/// （±90°/±180°）时 to_euler 会翻转另两位（±180°），与拖拽框的累加值
/// 混分支合成出错误旋转——物体姿态逐帧跳变（闪烁）。松手即作废。
#[derive(Resource, Default)]
pub(in crate::overlay) struct TransformDragCache(Option<(Entity, [f32; 3])>);

/// 组件清单一行：短名 + 已注册反射组件的字段只读行（None = 名字-only）。
pub(in crate::overlay) struct ComponentRow {
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

/// 属性快照采集：每帧对选中实体产一份纯数据（单实体量级，全帧
/// 成本可忽略）。**必须独占系统（&mut World）直接写资源**——只读 &World +
/// Commands 落账的 deferred 命令在本链上从不生效（快照恒为默认值），
/// 直接写零不确定性；链在 egui pass 之前。
#[system]
pub(in crate::overlay) fn collect_selected_details(world: &mut World) {
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

/// 属性区：快照渲染（头部/渲染语义/全局平移/组件清单）+ Transform
/// 编辑（DragValue 读快照副本）。返回一次编辑（有拖改时），写路径归
/// [`crate::overlay::transform_edit`]——本函数零 World 访问，纯渲染。
fn show_details(
    ui: &mut egui::Ui,
    details: &SelectedDetails,
    selected: &mut SelectedEntity,
    drag: &mut TransformDragCache,
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
    // Transform 编辑：读快照值进副本，任一量被拖改即整组回写（未动
    // 的组发原值，覆盖等价于原地——旋转组例外，见下方缓存接管）。
    // 平移/缩放分量相互独立，快照原值回写恒等价；旋转是"三元组表达一个
    // 四元数"，三元必须同分支（见 TransformDragCache）。
    let mut edit = None;
    if let Some(t) = details.local {
        // 缓存按实体挂靠：换选中即作废；无进行中拖拽则对齐快照（当前表示分支）
        if drag.0.as_ref().is_some_and(|(e, _)| *e != entity) {
            drag.0 = None;
        }
        if drag.0.is_none() {
            drag.0 = Some((entity, t.euler_yxz_deg));
        }
        let (mut translation, mut scale) = (t.translation, t.scale);
        let mut rot_dragged = [false; 3];
        {
            let drag_euler: &mut [f32; 3] = &mut drag.0.as_mut().expect("上方已保证有缓存").1;
            egui::Grid::new("transform_edit").num_columns(4).show(ui, |ui| {
                ui.monospace("平移");
                drag_vec3(ui, &mut translation, 0.05, "");
                ui.end_row();
                ui.monospace("旋转");
                rot_dragged = drag_vec3(ui, drag_euler, 1.0, "°");
                ui.end_row();
                ui.monospace("缩放");
                drag_vec3(ui, &mut scale, 0.02, "");
                ui.end_row();
            });
        }
        // 回写取拖拽框落在本帧的值（缓存内持续累加）；本帧无人按住旋转 =
        // 拖拽结束，缓存作废，下帧从快照重建（显示回落当前表示分支）
        let euler_written = drag.0.as_ref().map_or(t.euler_yxz_deg, |(_, e)| *e);
        if !rot_dragged.iter().any(|&d| d) {
            drag.0 = None;
        }
        if translation != t.translation || euler_written != t.euler_yxz_deg || scale != t.scale {
            edit = Some((
                entity,
                TransformEdit {
                    translation: Some(Vec3::from_array(translation)),
                    rotation_euler_deg: Some(Vec3::from_array(euler_written)),
                    scale: Some(Vec3::from_array(scale)),
                },
            ));
        }
    }
    if let Some(g) = details.global_translation {
        ui.monospace(format!("全局  平移 ({:+.3}, {:+.3}, {:+.3})", g.x, g.y, g.z));
    }
    // 组件全清单：archetype 口径；注册的折叠出字段行，未注册只报名字
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
                        // id_salt 混实体号:同短名组件(如多个 Transform 行)
                        // 的折叠状态不跨实体互串
                        .id_salt(format!("{}/{}", entity.index(), row.name))
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
/// 角度标。返回逐位"本帧被按住拖拽"（旋转缓存据此判定拖拽期起止）。
fn drag_vec3(ui: &mut egui::Ui, v: &mut [f32; 3], speed: f32, suffix: &str) -> [bool; 3] {
    let mut dragged = [false; 3];
    ui.horizontal(|ui| {
        for (i, x) in v.iter_mut().enumerate() {
            let resp = ui.add(
                egui::DragValue::new(x)
                    .speed(speed)
                    .suffix(suffix)
                    .max_decimals(if suffix.is_empty() { 3 } else { 1 }),
            );
            dragged[i] = resp.dragged() || resp.is_pointer_button_down_on();
        }
    });
    dragged
}
