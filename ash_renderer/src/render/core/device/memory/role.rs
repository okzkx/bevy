//! buffer 用途角色：每种角色要什么 usage、内存类型必须有和最好有哪些属性，只在
//! [`BufferRole::spec`] 写一份，创建 buffer 和挑内存类型都从这里读。

use ash::vk;

/// 一个角色的规格。
#[derive(Clone, Copy, Debug)]
pub struct RoleSpec {
    pub usage: vk::BufferUsageFlags,
    /// 内存类型必须全含的属性；设备给不出就报错，不降级。
    pub required: vk::MemoryPropertyFlags,
    /// 必需之外的偏好：命中越多越优，不构成否决。
    pub preferred: vk::MemoryPropertyFlags,
}

/// buffer 用途角色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferRole {
    /// 顶点/索引大池：transfer 写入，图形阶段读。
    DevicePool,
    /// 上传暂存：宿主写，transfer 读。
    Staging,
    /// 回读：transfer 写，宿主读。
    Readback,
    /// 每帧 UBO（set1 的载体）：宿主每帧写，着色器读；每帧槽一个 buffer。
    FrameUniform,
    /// 宿主可见的 storage：帧槽环、探针参数表这类宿主直写/直读、GPU 按索引读写的数据。
    Storage,
    /// UI 顶点/索引：每帧整份重写，宿主直写不经 staging；容量固定，超容由调用方丢弃。
    DynamicDraw,
    /// 显存里的常驻参数表（对象表、可见标记等）：只经上传批次改写，从不覆盖仍在被
    /// 在飞 draw 读的记录（更新走"新槽 + 旧槽停尸房"，见 instance_table）。
    DeviceStorage,
    /// indirect 绘制命令环：compute 每帧全量重写，`cmd_draw_indexed_indirect` 读，宿主不碰。
    IndirectCommands,
}

impl BufferRole {
    /// 角色规格。三个 `TRANSFER_SRC` 是给内容抽查用的：池迁移/扩容、回读校验和 BRP 环
    /// 对账都要把这些 buffer 当拷贝源。
    #[must_use]
    pub fn spec(self) -> RoleSpec {
        use vk::BufferUsageFlags as U;
        use vk::MemoryPropertyFlags as M;
        let (usage, required, preferred) = match self {
            Self::DevicePool => (
                U::VERTEX_BUFFER | U::INDEX_BUFFER | U::TRANSFER_DST | U::TRANSFER_SRC,
                M::DEVICE_LOCAL,
                M::empty(),
            ),
            Self::Staging => (U::TRANSFER_SRC, M::HOST_VISIBLE, M::HOST_COHERENT),
            Self::Readback => (
                U::TRANSFER_DST,
                M::HOST_VISIBLE,
                M::HOST_CACHED | M::HOST_COHERENT,
            ),
            Self::FrameUniform => (
                U::UNIFORM_BUFFER,
                M::HOST_VISIBLE,
                M::HOST_CACHED | M::HOST_COHERENT,
            ),
            Self::Storage => (
                U::STORAGE_BUFFER | U::TRANSFER_DST | U::TRANSFER_SRC,
                M::HOST_VISIBLE,
                M::HOST_CACHED | M::HOST_COHERENT,
            ),
            Self::DynamicDraw => (
                U::VERTEX_BUFFER | U::INDEX_BUFFER,
                M::HOST_VISIBLE,
                M::HOST_CACHED | M::HOST_COHERENT,
            ),
            Self::DeviceStorage => (
                U::STORAGE_BUFFER | U::TRANSFER_DST | U::TRANSFER_SRC,
                M::DEVICE_LOCAL,
                M::empty(),
            ),
            Self::IndirectCommands => (
                U::STORAGE_BUFFER | U::INDIRECT_BUFFER | U::TRANSFER_SRC,
                M::DEVICE_LOCAL,
                M::empty(),
            ),
        };
        RoleSpec { usage, required, preferred }
    }
}
