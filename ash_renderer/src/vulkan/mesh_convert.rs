//! 网格转换(3.2.3):`bevy::mesh::Mesh` → 3.2/3.4 定案的交错 32B vertex-input 布局 +
//! 统一 U32 索引。纯 CPU 侧字节整形,零 Vulkan 调用——Vulkan 只在 [`super::pool`] /
//! [`super::uploader`] 消费产出的字节。
//!
//! **定案**(施工计划 §2 关键决策表:这是我们的布局选择,不是 Vulkan 禁止 SoA):
//!
//! | 字段 | offset | 尺寸 | 来源 |
//! |---|---|---|---|
//! | POSITION | 0 | 12B(`[f32;3]`) | `Mesh::ATTRIBUTE_POSITION`,缺即拒收 |
//! | NORMAL | 12 | 12B(`[f32;3]`) | `Mesh::ATTRIBUTE_NORMAL`,缺则全 0 |
//! | UV_0 | 24 | 8B(`[f32;2]`) | `Mesh::ATTRIBUTE_UV_0`,缺则全 0 |
//!
//! stride 32B,与 3.4 的 vertex-input 状态是同一份契约(本模块是唯一生产点)。
//! "交错 32B 不得未经验证复用为 WGSL storage struct"——M2 的 vertex 数据只走
//! vertex-input 路径,不进描述符( Bindless 表只装贴图/采样器,见施工计划 §2)。
//!
//! **索引**:bevy 的 `Indices::U16/U32` 均收,统一输出 U32(`vk::IndexType::UINT32`)。
//! 放弃"U16 输入保 U16 输出"的省内存分支:一字节序/firstIndex 步长/绑定对齐三处
//! 都只留一条路,验收面减半;FlightHelmet 284166 索引按 U32 = 1.1MB,容量余量覆盖。
//! 无索引网格输出顺序索引 0..n(显式策略,不跳过)。
//!
//! **缺字段/拓扑策略**(任务口径:明确处理,不静默):POSITION 缺失或格式不是
//! Float32x3 → 拒绝该资产(没有位置的网格无法构成 3.4 的绘制);NORMAL/UV_0
//! 缺失 → 0 填充继续(3.4 调试着色允许无光/无贴图,信息缺口记录在日志);拓扑非
//! TriangleList → 拒绝(M2 管线只配 TriangleList,strip/line 的绕向与索引语义另议)。
//! 拒绝是 [`MeshConvertError`],由上传编排层按 Tier① warn 后跳过,帧循环继续。

use bevy::mesh::{Indices, Mesh, PrimitiveTopology, VertexAttributeValues};

/// 交错顶点布局的 stride,3.2 上传与 3.4 vertex-input 共用(字节)。
pub const VERTEX_STRIDE: u64 = 32;

/// 转换拒绝的原因(编排层按它分类 warn,一次一资产,不刷屏)。
#[derive(Debug, thiserror::Error)]
pub enum MeshConvertError {
    /// 拓扑不在本步支持范围(只收 TriangleList)。
    #[error("不支持的拓扑 {0:?}(本步只收 TriangleList)")]
    UnsupportedTopology(PrimitiveTopology),
    /// POSITION 缺失或不是 Float32x3——没有位置就无法绘制,拒绝。
    #[error("POSITION 缺失或不是 Float32x3")]
    MissingPosition,
    /// 空网格(0 顶点):没有可上传的内容。
    #[error("网格为空(0 顶点)")]
    EmptyMesh,
}

/// 一个网格的转换产物:字节已在调用方排好池内偏移后原样进 staging。
pub struct ConvertedMesh {
    /// 交错顶点数据,长度 = `vertex_count * VERTEX_STRIDE`。
    pub vertices: Vec<u8>,
    /// U32 索引数据,长度 = `index_count * 4`。
    pub indices: Vec<u8>,
    pub vertex_count: u32,
    pub index_count: u32,
}

/// 转换一个网格;拒绝条件见模块文档(缺失 POSITION / 非 TriangleList / 空)。
///
/// # Errors
/// [`MeshConvertError`] 列出的三种拒绝;本函数不做任何分配以外的副作用。
pub fn convert_mesh(mesh: &Mesh) -> Result<ConvertedMesh, MeshConvertError> {
    if mesh.primitive_topology() != PrimitiveTopology::TriangleList {
        return Err(MeshConvertError::UnsupportedTopology(
            mesh.primitive_topology(),
        ));
    }
    let vertex_count = mesh.count_vertices();
    if vertex_count == 0 {
        return Err(MeshConvertError::EmptyMesh);
    }
    let VertexAttributeValues::Float32x3(positions) = mesh
        .try_attribute(Mesh::ATTRIBUTE_POSITION)
        .map_err(|_| MeshConvertError::MissingPosition)?
    else {
        return Err(MeshConvertError::MissingPosition);
    };
    debug_assert_eq!(positions.len(), vertex_count);
    let normals = match mesh.try_attribute(Mesh::ATTRIBUTE_NORMAL) {
        Ok(VertexAttributeValues::Float32x3(n)) => Some(n),
        Ok(other) => {
            bevy::log::warn!("NORMAL 是 {other:?} 而非 Float32x3,按全 0 填充");
            None
        }
        Err(_) => None,
    };
    let uvs = match mesh.try_attribute(Mesh::ATTRIBUTE_UV_0) {
        Ok(VertexAttributeValues::Float32x2(uv)) => Some(uv),
        Ok(other) => {
            bevy::log::warn!("UV_0 是 {other:?} 而非 Float32x2,按全 0 填充");
            None
        }
        Err(_) => None,
    };

    // 顶点交错:32B/顶点,字段内按 f32 小端逐分量展开(x86/主流 GPU 均小端;
    // to_le_bytes 让布局不依赖平台内建序的巧合)。逐顶点三次 extend——55392 顶点
    // 级别的量,可读性优先,不为微优化引入 bytemuck 依赖。
    let mut vertices = Vec::with_capacity(vertex_count * VERTEX_STRIDE as usize);
    for i in 0..vertex_count {
        for b in positions[i] {
            vertices.extend_from_slice(&b.to_le_bytes());
        }
        match normals {
            Some(n) => {
                for b in n[i] {
                    vertices.extend_from_slice(&b.to_le_bytes());
                }
            }
            None => vertices.extend_from_slice(&[0u8; 12]),
        }
        match uvs {
            Some(uv) => {
                for b in uv[i] {
                    vertices.extend_from_slice(&b.to_le_bytes());
                }
            }
            None => vertices.extend_from_slice(&[0u8; 8]),
        }
    }

    // 索引:统一 U32;无索引则 0..n 顺序生成(显式策略)。
    let index_count = mesh.indices().map_or(vertex_count, Indices::len);
    let mut indices = Vec::with_capacity(index_count * 4);
    match mesh.indices() {
        Some(Indices::U16(list)) => {
            for &v in list {
                indices.extend_from_slice(&(u32::from(v)).to_le_bytes());
            }
        }
        Some(Indices::U32(list)) => {
            for &v in list {
                indices.extend_from_slice(&v.to_le_bytes());
            }
        }
        None => {
            for v in 0..vertex_count as u32 {
                indices.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    Ok(ConvertedMesh {
        vertices,
        indices,
        vertex_count: vertex_count as u32,
        index_count: index_count as u32,
    })
}
