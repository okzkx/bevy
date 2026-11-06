//! BRP 方法族公共标准件：请求参数解析与协议错误构造（[`params`]）、GPU 整段
//! 读回与小端字解析（[`readback`]）、读数对照的 CPU 参照相机矩阵与投屏钳制
//! （[`reference`]）。只被 remote 内各方法族消费，不注册任何 BRP 方法。

pub(crate) mod params;
pub(crate) mod readback;
pub(crate) mod reference;
