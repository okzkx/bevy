//! 光源分簇的帧侧单元：取用束（ClusterInput）、cluster 均匀数据与光源常驻表的
//! 帧槽写入、cluster_build 录制输入准备。Cluster 光照本体在
//! [`crate::render::passes::lighting::cluster`]。

use bevy::{ecs::system::SystemParam, prelude::*};

use ash::vk;

use crate::{
    common::syntax::ResultTierExt,
    render::{
        pack_cluster_uniforms, ClusterBuildDraw, ClusterBuildPass, LightClusters, LightTable,
    },
};

use super::scene::FrameView;

/// 光源分簇：cluster 资源、cluster_build 管线和光源常驻表。合成与透明前向共用。
#[derive(SystemParam)]
pub(super) struct ClusterInput<'w> {
    pub(super) clusters: ResMut<'w, LightClusters>,
    pub(super) build_pass: Res<'w, ClusterBuildPass>,
    pub(super) lights: ResMut<'w, LightTable>,
}

impl ClusterInput<'_> {
    /// 写本帧槽的 cluster 均匀数据和光源常驻表脏行。调用时该槽 fence 已等过。
    pub(super) fn write_slot(
        &mut self,
        slot: usize,
        view: &FrameView,
        extent: vk::Extent2D,
    ) -> Result<(), BevyError> {
        let cluster_uniform = pack_cluster_uniforms(
            &view.view_proj,
            &view.inv_view_proj,
            extent,
            self.clusters.grid(),
            self.lights.light_count(),
        );
        self.clusters
            .uniform(slot)
            .write(0, &cluster_uniform)
            .or_fatal("cluster 均匀数据写失败，渲染链无法继续")?;
        // 只写脏行；一行要在两个帧槽都写过才算清
        self.lights
            .write_dirty(slot)
            .or_fatal("光源常驻表脏行写失败，渲染链无法继续")
    }
}

/// cluster_build 录制输入：在场行 > 0 才建光源列表（合成与透明前向共用）。
pub(super) fn create_cluster_build_draw(
    cluster: &ClusterInput,
    count: u32,
) -> Option<ClusterBuildDraw> {
    (count > 0).then(|| cluster.build_pass.draw(cluster.clusters.grid()))
}
