//! 实体层级树页：World 全部实体按 [`Children`]/[`ChildOf`]
//! 关系递归展示，节点带 Name（无名按组件角色命名）+ 组件标签；点选节点落
//! [`SelectedEntity`] 资源。左栏层级树，右栏属性区——组件全清单、渲染语义
//! 行（Mesh/材质/贴图槽）与 Transform 编辑（DragValue，写经
//! [`crate::scene`] 的 Transform 编辑封装）。
//!
//! 取数分两层：树本身由维护系统直查 World 重建拍平缓存，不另造快照通道；属性区走
//! [`SelectedDetails`] 纯数据快照——采集系统产、egui pass 消费（后端收集/前端展示）。
//! 机制与证据：`.agents/docs/开发步骤/3-静态取数链路/3.10-egui实体层级树/`、`3.11-egui实体编辑/`。

use std::any::TypeId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bevy::{
    asset::{AssetId, UntypedAssetId},
    ecs::{
        lifecycle::{DespawnEvent, InsertEvent, RemoveEvent},
        reflect::{AppTypeRegistry, ReflectComponent},
        world::EntityRef,
    },
    math::EulerRot,
    pbr::{MeshMaterial3d, StandardMaterial},
    prelude::*,
    reflect::{PartialReflect, ReflectRef},
};
use wgpu_types::TextureFormat;

use ash_macros::system;

use crate::scene::{EntityRow, SceneTreeIndex, SceneTreeNode, TransformEdit, TREE_MAX_DEPTH};

/// 树内点选的实体。选中实体 despawn 后由详情区检空清账，
/// 不残留死引用；Transform 编辑以它为目标。
#[derive(Resource, Default)]
pub(in crate::overlay) struct SelectedEntity(pub(in crate::overlay) Option<Entity>);

/// 拍平树一行：等高行数组元素。`depth` 只用于缩进，`expanded` 在重建时
/// 从展开态账读出（行渲染不自持折叠状态）。
struct TreeRow {
    entity: Entity,
    depth: usize,
    label: String,
    /// 有子节点（渲染折叠形态；点击 = 展开/收起 + 选中）。
    has_children: bool,
    /// 展开态（has_children 时有意义）。
    expanded: bool,
}

/// 树拍平缓存：展开状态自持 + 脏信号才重建。稳态每帧零重建，
/// egui pass 只读 [`EntityTreeState::rows`] 切片（`ScrollArea::show_rows`
/// 只构建视口内的行）——pass 成本与实体总数解耦。
///
/// 脏信号三观测器（Insert/Remove/Despawn = 实体增删/换柄/层级/标签形态任一变化）
/// 置 [`EntityTreeState::dirty`]；展开/收起由 UI 写展开态账后同样置脏。
/// 坑：Name 的原地改写不走 Insert，不触发重建（本工程 Name 只在进场/BRP spawn
/// 时写入，均走 Insert）。
#[derive(Resource)]
pub(in crate::overlay) struct EntityTreeState {
    rows: Vec<TreeRow>,
    relevant: usize,
    total: usize,
    dirty: bool,
    /// 展开态账（按实体）：首次见到按默认（深度 < 2 展开）落账，
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
    /// 重建拍平行数组：全量走访建 [`SceneTreeIndex`]（与 BRP `scene_tree`
    /// 同一索引——单实现，CLI 树与面板树同源），只收展开节点成等高行。
    fn rebuild(&mut self, rows: &Query<EntityRow>) {
        let index = SceneTreeIndex::build(rows);
        self.rows.clear();
        let mut seen: HashSet<Entity> = HashSet::new();
        for root in &index.roots {
            self.flatten_row(*root, 0, &index.nodes, &mut seen);
        }
        self.expanded.retain(|e, _| seen.contains(e));
        self.relevant = index.nodes.len();
        self.total = index.total;
    }

    /// 递归拍平一个相关子树：只收展开节点；不相关子树整枝跳过（与
    /// CLI 树同语义）。深度越限按病态深树拒绝续走（[`TREE_MAX_DEPTH`]）。
    fn flatten_row(
        &mut self,
        entity: Entity,
        depth: usize,
        nodes: &HashMap<Entity, SceneTreeNode>,
        seen: &mut HashSet<Entity>,
    ) {
        if depth > TREE_MAX_DEPTH || !seen.insert(entity) {
            return;
        }
        let Some(SceneTreeNode {
            label, children, ..
        }) = nodes.get(&entity)
        else {
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
                self.flatten_row(child, depth + 1, nodes, seen);
            }
        }
    }
}

/// 脏信号观测器：任一组件 Insert（含实体 spawn 与换柄）/Remove/Despawn 都
/// 可能改树形态（增删/层级/标签/相关性）——只置脏，重建交给维护系统统一做。
fn mark_tree_dirty<E: Event>(_: On<E>, st: Option<ResMut<EntityTreeState>>) {
    if let Some(mut st) = st {
        st.dirty = true;
    }
}

/// 注册树脏信号观测器：Insert/Remove/Despawn 三类生命周期事件各一份。
pub(in crate::overlay) fn add_tree_dirty_observers(app: &mut App) -> &mut App {
    app.add_observer(mark_tree_dirty::<InsertEvent>)
        .add_observer(mark_tree_dirty::<RemoveEvent>)
        .add_observer(mark_tree_dirty::<DespawnEvent>)
}

/// 树拍平维护系统（Update，egui pass 前）：脏信号才全量重建，稳态直接
/// 返回——`Query` 不迭代、零分配，每帧成本与实体总数解耦。
#[system]
pub(in crate::overlay) fn maintain_entity_tree(
    mut st: ResMut<EntityTreeState>,
    rows: Query<EntityRow>,
) {
    if !st.dirty {
        return;
    }
    st.rebuild(&rows);
    st.dirty = false;
}

/// 层级树页的一次构建输入：框架 pass 每帧组好。
pub(in crate::overlay) struct EntityTreePage<'a> {
    /// 点选状态（资源借用，点选就地落账）。
    pub(in crate::overlay) selected: &'a mut SelectedEntity,
    /// 树拍平缓存：维护系统产，pass 只读行切片；展开/收起写回。
    pub(in crate::overlay) state: &'a mut EntityTreeState,
    /// 属性区快照（采集系统产）。
    pub(in crate::overlay) details: &'a SelectedDetails,
    /// 贴图预览句柄缓存（UI 侧首见建句柄，采集侧按快照收缩）。
    pub(in crate::overlay) previews: &'a mut TexturePreviewHandles,
    /// 旋转拖拽缓存（拖拽期内冻结欧拉表示分支）。
    pub(in crate::overlay) drag: &'a mut TransformDragCache,
}

impl EntityTreePage<'_> {
    /// 面板本体：左树右属性两栏。返回一次 Transform 编辑（有拖改时），
    /// 由 pass 落进编辑队列——本函数不碰写路径（展开态账与本缓存除外）。
    pub(in crate::overlay) fn show(self, ui: &mut egui::Ui) -> Option<(Entity, TransformEdit)> {
        let Self {
            selected,
            state,
            details,
            previews,
            drag,
        } = self;
        let height = ui.available_height();
        ui.horizontal_top(|ui| {
            // 左栏：层级树（定宽）
            ui.allocate_ui_with_layout(
                egui::vec2(TREE_WIDTH, height),
                egui::Layout::top_down_justified(egui::Align::Min),
                |ui| show_tree(ui, selected, state),
            );
            ui.separator();
            // 右栏：属性区占满剩余宽高
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), height),
                egui::Layout::top_down_justified(egui::Align::Min),
                |ui| show_details_panel(ui, details, previews, selected, drag),
            )
            .inner
        })
        .inner
    }
}

/// 左栏层级树宽度（points）。
const TREE_WIDTH: f32 = 240.0;

/// 左栏：计数行 + 拍平行数组的视口裁剪渲染（`show_rows` 只构建可见行，
/// 与实体总数解耦），展开/收起写回展开态账。
fn show_tree(ui: &mut egui::Ui, selected: &mut SelectedEntity, state: &mut EntityTreeState) {
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
        .max_height(ui.available_height())
        .show_rows(ui, row_height, state.rows.len(), |ui, range| {
            for TreeRow {
                entity,
                depth,
                label,
                has_children,
                expanded,
            } in &state.rows[range]
            {
                let is_selected = selected.0 == Some(*entity);
                ui.horizontal(|ui| {
                    ui.set_height(row_height);
                    ui.add_space(*depth as f32 * ROW_INDENT);
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
}

/// 每层缩进宽度（points）。
const ROW_INDENT: f32 = 14.0;

/// 选中实体属性快照：采集系统每帧对选中实体产一份纯数据，属性区只渲染快照。
/// 选中实体 despawn 后 `exists = false`，属性区借它清账。
#[derive(Resource, Default)]
pub(in crate::overlay) struct SelectedDetails {
    /// 当前选中（None = 无选中，快照为空壳）。
    entity: Option<Entity>,
    /// false = 已选中但实体不在 World（despawn 过）。
    exists: bool,
    name: Option<String>,
    children: Option<usize>,
    /// 网格资产名（None = 无 Mesh 或查不到路径）。
    mesh_name: Option<String>,
    /// 材质资产名（None = 无材质或查不到路径）。
    material_name: Option<String>,
    /// 材质五贴图槽里非空的：槽名 + 资产名 + 预览缩略图——比官方反射展示更贴本渲染器。
    textures: Vec<TextureSlotRow>,
    local: Option<TransformVals>,
    global_translation: Option<Vec3>,
    /// 组件全清单（archetype 口径，零反射全量）。
    component_rows: Vec<ComponentRow>,
}

/// 贴图槽快照行：槽名 + 资产 id（预览句柄缓存的键）+ 资产名 + 预览缩略图
///（None = 格式不支持/CPU 数据缺席）。
struct TextureSlotRow {
    label: &'static str,
    id: AssetId<Image>,
    name: Option<String>,
    thumb: Option<Arc<TextureThumb>>,
}

/// 预览缩略图：降采样后的 RGBA8 行主序字节（gamma 域原样，egui Color32 同域）。
pub(in crate::overlay) struct TextureThumb {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// 缩略图边长（降采样目标，正方形贴图即该边长；非正方形按比例缩）。
const THUMB_EDGE: u32 = 64;

/// 缩略图显示边长（points）。
const THUMB_PT: f32 = 48.0;

/// 缩略图采集缓存（采集系统侧）：AssetId → 降采样产物（None = 不支持）。
/// 资产内容对给定 id 不可变，缓存按当前快照的 id 清单保留，换选中即收缩。
#[derive(Resource, Default)]
pub(in crate::overlay) struct TextureThumbnails(HashMap<AssetId<Image>, Option<Arc<TextureThumb>>>);

/// 缩略图 egui 纹理句柄缓存（UI 侧）：AssetId → 句柄。`load_texture` 只在
/// 首见发生（TexturesDelta 整图增量一次），采集系统按当前 id 清单 retain，
/// 句柄丢弃的 free 增量由绘制段销毁 GPU 资源。
#[derive(Resource, Default)]
pub(in crate::overlay) struct TexturePreviewHandles(HashMap<AssetId<Image>, egui::TextureHandle>);

/// Transform 三组量的纯数值形态：编辑控件直接喂快照副本。
#[derive(Clone, Copy)]
struct TransformVals {
    translation: [f32; 3],
    /// YXZ 欧拉角（度），与展示同口径。
    euler_yxz_deg: [f32; 3],
    scale: [f32; 3],
}

/// 旋转拖拽缓存：按住的整段拖拽期内欧拉三元组冻结在按下帧的表示分支，
/// 拖拽框只改被拖的那位。松手即作废。
/// 坑：若每帧从快照重建，拖拽分量跨欧拉分解分支边界（±90°/±180°）时 to_euler
/// 会翻转另两位（±180°），与拖拽框累加值混分支合成错误旋转——物体姿态逐帧闪烁。
#[derive(Resource, Default)]
pub(in crate::overlay) struct TransformDragCache(Option<(Entity, [f32; 3])>);

/// 组件清单一行：短名 + 已注册反射组件的字段只读行（None = 名字-only）。
struct ComponentRow {
    name: String,
    fields: Option<Vec<(String, String)>>,
}

/// 从材质取一个可选图柄（贴图槽表第二列的函数形状，抽型免 type_complexity）。
type TextureSlotGet = fn(&StandardMaterial) -> &Option<Handle<Image>>;

/// StandardMaterial 的五个贴图槽：(展示名, 取柄)。
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

/// 属性快照采集：每帧对选中实体产一份纯数据（单实体量级，成本可忽略）；链在
/// egui pass 之前。
/// 坑：必须独占系统（&mut World）直接写资源——只读 &World + Commands 落账的
/// deferred 命令在本链上从不生效（快照恒为默认值）。
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
    let Ok(er) = world.get_entity(entity) else {
        details.exists = false;
        world.insert_resource(details);
        return;
    };
    // —— 采样段（全程只读）：快照纯数据 + 资产 id 清单 ——
    details.name = er.get::<Name>().map(|n| n.as_str().to_owned());
    details.children = er.get::<Children>().map(|c| c.len());
    let mesh_id = er.get::<Mesh3d>().map(|m| m.0.id());
    let material_id = er
        .get::<MeshMaterial3d<StandardMaterial>>()
        .map(|m| m.0.id());
    let texture_ids: Vec<(&'static str, AssetId<Image>)> = material_id
        .and_then(|id| {
            world
                .get_resource::<Assets<StandardMaterial>>()
                .and_then(|assets| assets.get(id))
        })
        .map(|mat| {
            TEXTURE_SLOTS
                .iter()
                .filter_map(|(label, get)| get(mat).as_ref().map(|h| (*label, h.id())))
                .collect()
        })
        .unwrap_or_default();
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
    // —— 落名段（可动缓存账）：资产名 + 缩略图缓存回填 + 句柄账按当前清单收缩 ——
    details.mesh_name = mesh_id.and_then(|id| asset_display_name(world, id));
    details.material_name = material_id.and_then(|id| asset_display_name(world, id));
    let keep: HashSet<AssetId<Image>> = texture_ids.iter().map(|(_, id)| *id).collect();
    // 相位一（只读）：缓存命中的直接取，未命中的现算（本帧落账，下帧起命中）
    let mut thumbs: HashMap<AssetId<Image>, Option<Arc<TextureThumb>>> = HashMap::new();
    {
        let cache = world.resource::<TextureThumbnails>();
        let images = world.get_resource::<Assets<Image>>();
        for &(_, id) in &texture_ids {
            let hit = cache.0.get(&id).cloned().flatten();
            thumbs.insert(
                id,
                hit.or_else(|| {
                    images
                        .and_then(|assets| assets.get(id))
                        .and_then(make_thumbnail)
                        .map(Arc::new)
                }),
            );
        }
    }
    details.textures = texture_ids
        .into_iter()
        .map(|(label, id)| TextureSlotRow {
            label,
            id,
            name: asset_display_name(world, id),
            thumb: thumbs.get(&id).cloned().flatten(),
        })
        .collect();
    // 相位二（写）：缓存回填 + 收缩到当前清单（句柄丢弃的 free 增量由绘制段销毁）
    {
        let mut cache = world.resource_mut::<TextureThumbnails>();
        for (id, thumb) in thumbs {
            cache.0.insert(id, thumb);
        }
        cache.0.retain(|id, _| keep.contains(id));
    }
    world
        .resource_mut::<TexturePreviewHandles>()
        .0
        .retain(|id, _| keep.contains(id));
    world.insert_resource(details);
}

/// 资产显示名：AssetServer 路径账里的标签（glTF 子资产如 "Mesh12/Primitive3"）
/// 或文件名；查不到路径（进程内造的资产）返回 None。
fn asset_display_name(world: &World, id: impl Into<UntypedAssetId>) -> Option<String> {
    let server = world.get_resource::<AssetServer>()?;
    let path = server.get_path(id)?;
    match path.label() {
        Some(label) => Some(label.to_owned()),
        None => path
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned()),
    }
}

/// 降采样成最长边 [`THUMB_EDGE`] 的缩略图（块内 4×4 采样取平均，成本与源尺寸
/// 无关）。仅支持 8bit RGBA 与单层 2D；压缩/浮点/数组纹理返回 None，属性区
/// 只显示名字。
fn make_thumbnail(image: &Image) -> Option<TextureThumb> {
    if !matches!(
        image.texture_descriptor.format,
        TextureFormat::Rgba8Unorm | TextureFormat::Rgba8UnormSrgb
    ) {
        return None;
    }
    let size = image.texture_descriptor.size;
    if size.depth_or_array_layers != 1 {
        return None;
    }
    let (w, h) = (size.width.max(1), size.height.max(1));
    // mip0 在最前（LayerThenMips），按字节量截取
    let mip0 = image.data.as_ref()?.get(..w as usize * h as usize * 4)?;
    let longest = w.max(h) as usize;
    let out_w = ((w as usize * THUMB_EDGE as usize).div_ceil(longest)).min(THUMB_EDGE as usize);
    let out_h = ((h as usize * THUMB_EDGE as usize).div_ceil(longest)).min(THUMB_EDGE as usize);
    const SAMPLES: usize = 4; // 块内每轴采样数
    let mut rgba = vec![0u8; out_w * out_h * 4];
    for (oy, row) in rgba.chunks_mut(out_w * 4).enumerate() {
        let by0 = oy * h as usize / out_h;
        let by1 = (((oy + 1) * h as usize) / out_h).clamp(by0 + 1, h as usize);
        for (ox, px) in row.chunks_mut(4).enumerate() {
            let bx0 = ox * w as usize / out_w;
            let bx1 = (((ox + 1) * w as usize) / out_w).clamp(bx0 + 1, w as usize);
            let mut acc = [0u32; 4];
            for iy in 0..SAMPLES {
                let y = (by0 + (by1 - by0) * iy / SAMPLES).min(h as usize - 1);
                for ix in 0..SAMPLES {
                    let x = (bx0 + (bx1 - bx0) * ix / SAMPLES).min(w as usize - 1);
                    let src = (y * w as usize + x) * 4;
                    for (c, a) in acc.iter_mut().enumerate() {
                        *a += u32::from(mip0[src + c]);
                    }
                }
            }
            for (o, a) in px.iter_mut().zip(acc) {
                *o = (a / (SAMPLES * SAMPLES) as u32) as u8;
            }
        }
    }
    Some(TextureThumb {
        width: out_w as u32,
        height: out_h as u32,
        rgba,
    })
}

/// 组件全清单：archetype 口径（`EntityRef::archetype().components()`），零反射
/// 全量；已注册反射且无专行的组件附字段只读行。
fn component_rows(world: &World, er: &EntityRef) -> Vec<ComponentRow> {
    let components = world.components();
    // 注册表缺席（不该发生：register_type 已懒插）时退化只报名字
    let registry = world.get_resource::<AppTypeRegistry>().map(|r| r.read());
    let reflected_fields = |tid: TypeId| {
        if SKIP_REFLECT.contains(&tid) {
            return None; // 已有精确专行，不上反射行
        }
        let rc = registry.as_ref()?.get(tid)?.data::<ReflectComponent>()?;
        Some(reflect_field_rows(rc.reflect(*er)?))
    };
    er.archetype()
        .components()
        .iter()
        .filter_map(|cid| components.get_info(*cid))
        .map(|info| ComponentRow {
            name: info.name().shortname().to_string(),
            fields: info.type_id().and_then(&reflected_fields),
        })
        .collect()
}

/// 反射字段只读行（ReflectRef 分发）：Struct 拆逐字段行，其余整值一行（Debug 格式）。
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

/// 栏属性区外壳：整栏一滚——树独立滚、属性区独立滚，组件清单不再自挂
/// ScrollArea，随属性区一起滚。返回一次编辑（有拖改时）。
fn show_details_panel(
    ui: &mut egui::Ui,
    details: &SelectedDetails,
    previews: &mut TexturePreviewHandles,
    selected: &mut SelectedEntity,
    drag: &mut TransformDragCache,
) -> Option<(Entity, TransformEdit)> {
    egui::ScrollArea::vertical()
        .id_salt("实体属性")
        .auto_shrink([false, false])
        .show(ui, |ui| show_details(ui, details, previews, selected, drag))
        .inner
}

/// 属性区：快照渲染（头部/渲染语义/全局平移/组件清单）+ Transform
/// 编辑（DragValue 读快照副本）。返回一次编辑（有拖改时），写路径归
/// [`crate::scene`] 的 Transform 编辑封装——本函数零 World 访问，纯渲染。
fn show_details(
    ui: &mut egui::Ui,
    details: &SelectedDetails,
    previews: &mut TexturePreviewHandles,
    selected: &mut SelectedEntity,
    drag: &mut TransformDragCache,
) -> Option<(Entity, TransformEdit)> {
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
    if let Some(name) = &details.mesh_name {
        ui.monospace(format!("网格   {name}"));
    }
    if let Some(name) = &details.material_name {
        ui.monospace(format!("材质   {name}"));
    }
    for row in &details.textures {
        ui.horizontal(|ui| {
            if let Some(thumb) = &row.thumb {
                show_thumb(ui, previews, row.id, thumb);
            }
            ui.monospace(match &row.name {
                Some(name) => format!("贴图 {} {name}", row.label),
                None => format!("贴图 {}", row.label),
            });
        });
    }
    // Transform 编辑：读快照值进副本，任一量被拖改即整组回写（未动的分量发原值，
    // 等价于原地）；旋转三元组须同表示分支，由 TransformDragCache 接管
    let mut edit = None;
    if let Some(t) = details.local {
        // 缓存按实体挂靠：换选中即作废；无进行中拖拽则对齐快照（当前表示分支）
        if drag.0.as_ref().is_some_and(|(e, _)| *e != entity) {
            drag.0 = None;
        }
        let (mut translation, mut scale) = (t.translation, t.scale);
        let mut rot_dragged = [false; 3];
        {
            let drag_euler: &mut [f32; 3] = &mut drag.0.get_or_insert((entity, t.euler_yxz_deg)).1;
            egui::Grid::new("transform_edit")
                .num_columns(4)
                .show(ui, |ui| {
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
        ui.monospace(format!(
            "全局  平移 ({:+.3}, {:+.3}, {:+.3})",
            g.x, g.y, g.z
        ));
    }
    // 组件全清单：archetype 口径；注册的折叠出字段行，未注册只报名字
    ui.separator();
    ui.strong(format!("组件 {}", details.component_rows.len()));
    for row in &details.component_rows {
        match &row.fields {
            Some(fields) => {
                egui::CollapsingHeader::new(egui::RichText::new(&row.name).monospace())
                    // id_salt 混实体号：同短名组件的折叠状态不跨实体互串
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

/// 贴图预览小图：缩略图字节 → egui 纹理（句柄按 AssetId 缓存，首见才
/// `load_texture`——整图增量一次，绘制段随后发布成独立 bindless 槽位）。
fn show_thumb(
    ui: &mut egui::Ui,
    previews: &mut TexturePreviewHandles,
    id: AssetId<Image>,
    thumb: &TextureThumb,
) {
    let handle = previews.0.entry(id).or_insert_with(|| {
        ui.ctx().load_texture(
            format!("贴图预览 {id:?}"),
            egui::ColorImage::from_rgba_unmultiplied(
                [thumb.width as usize, thumb.height as usize],
                &thumb.rgba,
            ),
            egui::TextureOptions::LINEAR,
        )
    });
    ui.add(egui::Image::new((
        handle.id(),
        egui::vec2(THUMB_PT, THUMB_PT),
    )));
}
