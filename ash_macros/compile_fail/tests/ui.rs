//! `#[system]` 的编译期拒绝用例（负例）；通过用例由 ash_renderer 宿主标注承担。

fn main() -> compile_fail_utils::ui_test::Result<()> {
    compile_fail_utils::test("ash_macros_ui", "tests/ui")
}
