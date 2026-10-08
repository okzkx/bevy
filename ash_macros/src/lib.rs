//! `#[system]`：Bevy 系统函数的透明标注宏。
//!
//! 给会被 `add_systems` 注册的函数打阅读路标，并在编译期钉住"系统"的语法铁律。
//! 函数本体原样保留，`add_systems` 调用点与运行时行为一概不变。
//!
//! 校验项（语法级铁律）：
//! - 只能标注函数；
//! - 不能带 `self`——Bevy 的 `System` 无业务 self，状态走 `Local`/`Resource`；
//!   带 self 的函数永远变不成 system，但编译器要到 `add_systems` 调用点才报
//!   trait 不匹配且信息难读，这里提前到标注点拒绝。
//!
//! 边界：不做参数白名单校验——`SystemParam` 是开放集合（可自定义 derive），
//! 参数签名合法性仍由 `add_systems` 调用点的 trait 检查裁决。

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::ToTokens;
use syn::{ImplItemFn, ItemFn, Signature};

#[proc_macro_attribute]
pub fn system(attr: TokenStream, item: TokenStream) -> TokenStream {
    system_impl(attr.into(), item.into())
}

fn system_impl(attr: TokenStream2, item: TokenStream2) -> TokenStream {
    // 参数位预留作元数据扩展；静默吞掉会变成"看着生效、没人读取"的失真标注
    if !attr.is_empty() {
        return err(attr, "#[system] 暂不支持参数");
    }
    let sig = match parse_signature(item.clone()) {
        Ok(sig) => sig,
        Err(_) => return err(item, "#[system] 只能标注函数"),
    };
    if sig.receiver().is_some() {
        return err(
            item,
            "#[system] 不能标注带 self 的方法：Bevy 的 System 无业务 self，状态走 Local/Resource",
        );
    }
    item.into()
}

// 自由函数与 impl 块内的关联函数走不同 parse 路径；两者都不成立时按"非函数"拒绝
fn parse_signature(item: TokenStream2) -> syn::Result<Signature> {
    if let Ok(f) = syn::parse2::<ItemFn>(item.clone()) {
        return Ok(f.sig);
    }
    syn::parse2::<ImplItemFn>(item).map(|f| f.sig)
}

fn err(span_source: impl ToTokens, message: &str) -> TokenStream {
    syn::Error::new_spanned(span_source, message)
        .to_compile_error()
        .into()
}
