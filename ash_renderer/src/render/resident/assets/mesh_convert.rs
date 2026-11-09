//! 网格转换：`bevy::mesh::Mesh` → 交错 32B 顶点 + U32 索引。纯 CPU 字节整形，不调 Vulkan；
//! 产出由 [`super::pool`] 与 `core::device::transfer` 上传。
//!
//! 顶点布局（本模块是唯一生产点，与图形管线的 vertex-input 状态是同一份约定）：
//!
//! | 字段 | offset | 尺寸 | 来源 |
//! |---|---|---|---|
//! | POSITION | 0 | 12B(`[f32;3]`) | `Mesh::ATTRIBUTE_POSITION`,缺即拒收 |
//! | NORMAL | 12 | 12B(`[f32;3]`) | `Mesh::ATTRIBUTE_NORMAL`,缺则全 0 |
//! | UV_0 | 24 | 8B(`[f32;2]`) | `Mesh::ATTRIBUTE_UV_0`,缺则全 0 |
//!
//! 顶点数据只走 vertex-input，不进描述符；交错 32B 未经验证不得当作 WGSL storage struct 复用。
//!
//! 索引：U16/U32 一律输出 U32（`vk::IndexType::UINT32`），只留一条路径，不为省内存保留 U16；
//! 无索引网格输出顺序索引 0..n。
//!
//! 拒收：拓扑不是 TriangleList（管线只配了 TriangleList）、POSITION 缺失或不是 Float32x3、
//! 0 顶点。NORMAL/UV_0 缺失或不匹配时填 0 继续（有 warn 日志）。拒收返回
//! [`MeshConvertError`]，调用方 warn 后跳过该资产，帧循环继续。

use bevy::mesh::{Indices, Mesh, PrimitiveTopology, VertexAttributeValues};

/// 交错顶点的 stride（字节），上传与 vertex-input 共用。
pub const VERTEX_STRIDE: u64 = 32;

/// 转换拒收的原因。
#[derive(Debug, thiserror::Error)]
pub enum MeshConvertError {
    #[error("不支持的拓扑 {0:?}(只收 TriangleList)")]
    UnsupportedTopology(PrimitiveTopology),
    #[error("POSITION 缺失或不是 Float32x3")]
    MissingPosition,
    /// POSITION 长度与顶点数不符（畸形资产）。用运行时检查而非 debug_assert：release 下
    /// 越界 panic 会绕过统一错误通道。
    #[error("POSITION 长度 {0} 与顶点数 {1} 不符")]
    PositionLengthMismatch(usize, usize),
    #[error("网格为空(0 顶点)")]
    EmptyMesh,
}

/// 一个网格的转换产物，字节原样进 staging。
pub struct ConvertedMesh {
    /// 交错顶点数据，长度 = `vertex_count * VERTEX_STRIDE`。
    pub vertices: Vec<u8>,
    /// U32 索引数据，长度 = `index_count * 4`。
    pub indices: Vec<u8>,
    pub vertex_count: u32,
    pub index_count: u32,
    /// 局部包围盒（POSITION 逐点 min/max），写进对象表记录。
    pub local_aabb: ([f32; 3], [f32; 3]),
}

/// 转换一个网格；拒收条件见模块注释。
///
/// # Errors
/// [`MeshConvertError`] 列出的拒收原因。
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
    // 长度不符拒收，不让后面按下标取值时越界 panic
    if positions.len() != vertex_count {
        return Err(MeshConvertError::PositionLengthMismatch(
            positions.len(),
            vertex_count,
        ));
    }
    let normals = match mesh.try_attribute(Mesh::ATTRIBUTE_NORMAL) {
        Ok(VertexAttributeValues::Float32x3(n)) if n.len() == vertex_count => Some(n),
        Ok(VertexAttributeValues::Float32x3(n)) => {
            bevy::log::warn!(
                "NORMAL 长度 {} 与顶点数 {vertex_count} 不符,按全 0 填充",
                n.len()
            );
            None
        }
        Ok(other) => {
            bevy::log::warn!("NORMAL 是 {other:?} 而非 Float32x3,按全 0 填充");
            None
        }
        Err(_) => None,
    };
    let uvs = match mesh.try_attribute(Mesh::ATTRIBUTE_UV_0) {
        Ok(VertexAttributeValues::Float32x2(uv)) if uv.len() == vertex_count => Some(uv),
        Ok(VertexAttributeValues::Float32x2(uv)) => {
            bevy::log::warn!(
                "UV_0 长度 {} 与顶点数 {vertex_count} 不符,按全 0 填充",
                uv.len()
            );
            None
        }
        Ok(other) => {
            bevy::log::warn!("UV_0 是 {other:?} 而非 Float32x2,按全 0 填充");
            None
        }
        Err(_) => None,
    };

    // 逐顶点按 f32 小端展开（to_le_bytes 不依赖平台字节序），同一遍扫出包围盒
    let mut vertices = Vec::with_capacity(vertex_count * VERTEX_STRIDE as usize);
    let mut aabb_min = [f32::INFINITY; 3];
    let mut aabb_max = [f32::NEG_INFINITY; 3];
    for i in 0..vertex_count {
        for b in positions[i] {
            vertices.extend_from_slice(&b.to_le_bytes());
        }
        for (k, v) in positions[i].into_iter().enumerate() {
            aabb_min[k] = aabb_min[k].min(v);
            aabb_max[k] = aabb_max[k].max(v);
        }
        match normals {
            Some(n) => {
                for b in n[i] {
                    vertices.extend_from_slice(&b.to_le_bytes());
                }
            }
            None => vertices.extend_from_slice(&[0u8; 12]), // 无法线：12B 全 0
        }
        match uvs {
            Some(uv) => {
                for b in uv[i] {
                    vertices.extend_from_slice(&b.to_le_bytes());
                }
            }
            None => vertices.extend_from_slice(&[0u8; 8]), // 无 UV：8B 全 0
        }
    }

    // 索引统一 U32；无索引则顺序生成 0..n
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
        local_aabb: (aabb_min, aabb_max),
    })
}
