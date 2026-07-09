pub mod tab_bar;
pub mod status_bar;
pub mod output;
pub mod input_line;
pub mod inspector;

use super::input::KeyEvent;
use super::screen::{Screen, Rect};

pub trait Panel {
    fn area(&self) -> Rect;
    fn render(&self, screen: &mut Screen);
    fn handle_key(&mut self, _key: KeyEvent) -> bool { false }
    fn set_active(&mut self, _active: bool) {}
    fn update(&mut self) {}
    fn title(&self) -> &str { "" }
}
