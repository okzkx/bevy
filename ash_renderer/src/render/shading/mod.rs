//! BRDF 着色与光源数据（8.2/8.3 主体，步骤级施工计划 §5 提案形状）。

pub mod light_table;

pub use light_table::{
    LightRecord, LightTable, LIGHT_FLAG_SHADOW, LIGHT_RECORD_SIZE, LIGHT_TABLE_CAPACITY,
    LIGHT_TYPE_POINT,
};
// LIGHT_TYPE_SPOT 暂不出口：spot 本体归后续段（记录形状预留），出口即 dead_code
