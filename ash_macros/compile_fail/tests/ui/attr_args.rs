use ash_macros::system;

// 参数位已预留：写入参数应当在编译期被拒绝，而不是静默吞掉
//~v ERROR: 暂不支持参数
#[system(set = "Input")]
fn foo() {}

fn main() {}
