//! BRDF 着色与光源数据：光源常驻表、CSM 宿主半边、着色模式。

pub mod light_table;
pub mod render_mode;
pub mod shadow;

pub use light_table::{LightRecord, LightTable, LIGHT_FLAG_SHADOW, LIGHT_TYPE_POINT};
pub use render_mode::RenderMode;
// LIGHT_TYPE_SPOT 不出口：spot 本体未实现（记录形状预留），出口即 dead_code
