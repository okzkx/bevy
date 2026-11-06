//! BRP 场景编辑写通道方法族：场景直控（[`scene`]）、对象生命周期（[`objects`]）、
//! 测试实体（[`shapes`]）、大场景生成（[`generate`]）、资产内容变更与运行期动态
//! 加载（[`assets`]）、虚拟鼠标输入（[`mouse`]）。操作本体在 scene 机制层封装
//! （BRP 与 egui 面板共用），本目录各文件只是协议薄壳。

pub(crate) mod assets;
pub(crate) mod generate;
pub(crate) mod mouse;
pub(crate) mod objects;
pub(crate) mod scene;
pub(crate) mod shapes;
