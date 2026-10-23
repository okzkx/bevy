//! 场景层级树索引：实体按 [`Children`]/[`ChildOf`] 组织的"相关实体"表，egui
//! 层级树页与 BRP `scene_tree` 共用——两侧看到同一棵树、同一套"相关"语义
//! 与节点命名，不养两份逻辑。
//!
//! 相关性：至少带一个被关注组件（名字/层级/相机/灯/mesh/材质/变换）才进树；
//! DefaultPlugins 注册的数百个 observer 等内部裸实体被滤掉，`total` 如实报出
//! 全量。根 = 相关且无父的实体（按实体号排序）；消费方递归子实体时跳过不在
//! 表内的整枝。

use std::collections::HashMap;

use bevy::{
    camera::Camera,
    light::DirectionalLight,
    pbr::{MeshMaterial3d, StandardMaterial},
    prelude::*,
    world_serialization::WorldAssetRoot,
};

/// 层级树一行的查询数据（11 项组合体抽型，免 clippy type_complexity）：
/// 层级关系（Children/ChildOf）+ 角色标记（相机/光/资产根）+ 呈现要素
/// （Name/Mesh/Material/两 Transform），逐项可缺。
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

/// 递归深度上限：病态深树在栈溢出前显式停走——栈溢出走 abort 是"禁 panic"
/// 规则下唯一不可兜底路径。上限 1024 远超真实场景层级，同时远低于典型线程栈
/// 的溢出深度。消费方递归时自行检查。
pub(crate) const TREE_MAX_DEPTH: usize = 1024;

/// 相关实体的一个节点。
pub(crate) struct SceneTreeNode {
    /// 节点行文本（[`node_label`]）。
    pub(crate) label: String,
    /// 子实体清单（含不相关者，消费方按表过滤）。
    pub(crate) children: Vec<Entity>,
    /// 带 ChildOf（非根）。
    has_parent: bool,
}

/// 一次全量走访产出的层级树索引。
pub(crate) struct SceneTreeIndex {
    /// 相关实体表。
    pub(crate) nodes: HashMap<Entity, SceneTreeNode>,
    /// 根（相关且无父），按实体号升序。
    pub(crate) roots: Vec<Entity>,
    /// 走访的实体总数（相关 + 被滤）。
    pub(crate) total: usize,
}

impl SceneTreeIndex {
    /// 全量走访查询建索引。
    pub(crate) fn build(rows: &Query<EntityRow>) -> Self {
        let mut nodes: HashMap<Entity, SceneTreeNode> = HashMap::new();
        let mut total = 0usize;
        for (entity, name, children, has_parent, cam, light, mesh, mat, root, local, global) in rows
        {
            total += 1;
            if is_relevant(name, children, has_parent, cam, light, mesh, mat, root, local, global)
            {
                nodes.insert(
                    entity,
                    SceneTreeNode {
                        label: node_label(
                            entity,
                            name,
                            cam,
                            light,
                            mesh.is_some(),
                            mat.is_some(),
                            root,
                        ),
                        children: children.map(|c| c.iter().collect()).unwrap_or_default(),
                        has_parent,
                    },
                );
            }
        }
        let mut roots: Vec<Entity> = nodes
            .iter()
            .filter(|(_, node)| !node.has_parent)
            .map(|(entity, _)| *entity)
            .collect();
        roots.sort_by_key(|e| e.index());
        Self { nodes, roots, total }
    }
}

/// 相关性过滤：至少带一个被关注组件才进树（见模块文档）。
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
