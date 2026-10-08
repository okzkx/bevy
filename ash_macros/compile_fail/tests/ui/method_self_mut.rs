use ash_macros::system;

struct Player;

impl Player {
    #[system]
    fn tick(&mut self) {}
    //~^ ERROR: 不能标注带 self 的方法
}
