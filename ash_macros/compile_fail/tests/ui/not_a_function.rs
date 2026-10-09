use ash_macros::system;

#[system]
struct NotAFunction;
//~^ ERROR: 只能标注函数
