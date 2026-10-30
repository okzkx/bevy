//! 层级树索引供 egui 与 BRP 共用。
//! 只收录带 Name、层级、相机、灯、mesh、材质、WorldAssetRoot 或 Transform 的实体；total 统计全部查询实体。
//! 根按实体号排序；递归时跳过未收录子实体所在的整枝。

use std::collections::HashMap;

use bevy::{
    camera::Camera,
    light::DirectionalLight,
    pbr::{MeshMaterial3d, StandardMaterial},
    prelude::*,
    world_serialization::WorldAssetRoot,
};

/// 层级树索引使用的查询字段。
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

/// 消费方递归时使用的深度上限，避免病态层级触发栈溢出。
pub(crate) const TREE_MAX_DEPTH: usize = 1024;

pub(crate) struct SceneTreeNode {
    pub(crate) label: String,
    /// 子实体清单（含不相关者，消费方按表过滤）。
    pub(crate) children: Vec<Entity>,
    /// 是否有父实体。
    has_parent: bool,
}

pub(crate) struct SceneTreeIndex {
    pub(crate) nodes: HashMap<Entity, SceneTreeNode>,
    /// 根（相关且无父），按实体号升序。
    pub(crate) roots: Vec<Entity>,
    /// 走访的实体总数（相关 + 被滤）。
    pub(crate) total: usize,
}

impl SceneTreeIndex {
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

#[expect(
    clippy::too_many_arguments,
    reason = "过滤参数与查询字段一一对应，保留参数表便于核对筛选条件"
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

/// 优先使用 Name；否则按实体角色命名，并显示实体号和组件标签。
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
