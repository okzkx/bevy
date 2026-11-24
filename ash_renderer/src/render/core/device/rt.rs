//! 硬件光线求交的设备能力门：三项 feature（BDA / 加速结构 / ray query）与三个设备扩展
//! 齐备才算支持。纯函数 [`RtSupport::resolve`] 只读宿主从 `vkGetPhysicalDeviceFeatures2`
//! 与扩展枚举里读到的布尔量，不碰设备句柄——"无 RT 设备"分支可以在单测里用假 caps 覆盖。

/// 建设备前查到的 RT 相关能力快照（`context.rs` 填充，只在这里参与判定）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtCaps {
    /// `VkPhysicalDeviceBufferDeviceAddressFeatures::bufferDeviceAddress`。
    pub buffer_device_address: bool,
    /// `VkPhysicalDeviceAccelerationStructureFeaturesKHR::accelerationStructure`。
    pub acceleration_structure: bool,
    /// `VkPhysicalDeviceRayQueryFeaturesKHR::rayQuery`。
    pub ray_query: bool,
    /// 设备扩展 `VK_KHR_acceleration_structure`。
    pub ext_acceleration_structure: bool,
    /// 设备扩展 `VK_KHR_ray_query`。
    pub ext_ray_query: bool,
    /// 设备扩展 `VK_KHR_deferred_host_operations`（AS 扩展的依赖，本身没有 feature）。
    pub ext_deferred_host_operations: bool,
}

/// RT 能力判定结果。`enabled` = 三项 feature 与三个扩展全部齐备。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtSupport {
    pub caps: RtCaps,
}

impl RtSupport {
    /// 纯判定：不查设备，只用调用方读到的能力位。缺任一项都判不支持（RT 关）。
    #[must_use]
    pub fn resolve(caps: RtCaps) -> Self {
        Self { caps }
    }

    /// 三项 feature + 三个扩展齐备。
    #[must_use]
    pub fn enabled(self) -> bool {
        let c = self.caps;
        c.buffer_device_address
            && c.acceleration_structure
            && c.ray_query
            && c.ext_acceleration_structure
            && c.ext_ray_query
            && c.ext_deferred_host_operations
    }

    /// 缺项的日志文案（全齐返回空串）。
    #[must_use]
    pub fn missing(self) -> String {
        let c = self.caps;
        let items = [
            ("bufferDeviceAddress", c.buffer_device_address),
            ("accelerationStructure", c.acceleration_structure),
            ("rayQuery", c.ray_query),
            (
                "VK_KHR_acceleration_structure",
                c.ext_acceleration_structure,
            ),
            ("VK_KHR_ray_query", c.ext_ray_query),
            (
                "VK_KHR_deferred_host_operations",
                c.ext_deferred_host_operations,
            ),
        ];
        items
            .iter()
            .filter(|(_, ok)| !ok)
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(" / ")
    }
}

#[cfg(test)]
mod tests {
    use super::{RtCaps, RtSupport};

    fn full() -> RtCaps {
        RtCaps {
            buffer_device_address: true,
            acceleration_structure: true,
            ray_query: true,
            ext_acceleration_structure: true,
            ext_ray_query: true,
            ext_deferred_host_operations: true,
        }
    }

    /// 无 RT 设备（全缺）判不支持，不 panic、不回退半开。
    #[test]
    fn no_rt_device_is_disabled() {
        let support = RtSupport::resolve(RtCaps::default());
        assert!(!support.enabled());
        assert!(support.missing().contains("rayQuery"));
    }

    /// 一条"清掉某位"的假设备用例：(missing 文案里的名字, 清位函数)。
    type Case = (&'static str, fn(&mut RtCaps));

    /// 六项全齐才开；缺任一项（逐项验证）都关。
    #[test]
    fn any_missing_bit_disables() {
        assert!(RtSupport::resolve(full()).enabled());
        // 名字与 missing() 的日志文案同源（缺项要能直接读日志定位）
        let cases: [Case; 6] = [
            ("bufferDeviceAddress", |c| c.buffer_device_address = false),
            ("accelerationStructure", |c| {
                c.acceleration_structure = false
            }),
            ("rayQuery", |c| c.ray_query = false),
            ("VK_KHR_acceleration_structure", |c| {
                c.ext_acceleration_structure = false
            }),
            ("VK_KHR_ray_query", |c| c.ext_ray_query = false),
            ("VK_KHR_deferred_host_operations", |c| {
                c.ext_deferred_host_operations = false
            }),
        ];
        for (name, clear) in cases {
            let mut caps = full();
            clear(&mut caps);
            let support = RtSupport::resolve(caps);
            assert!(!support.enabled(), "缺 {name} 仍判支持");
            assert!(
                support.missing().contains(name),
                "缺 {name} 的日志文案没点名"
            );
        }
    }

    /// 全齐时缺项文案为空。
    #[test]
    fn full_support_has_no_missing() {
        assert!(RtSupport::resolve(full()).missing().is_empty());
    }
}
