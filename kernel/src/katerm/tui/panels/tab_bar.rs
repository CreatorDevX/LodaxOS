use super::Panel;
use crate::katerm::tui::screen::{Screen, Rect, Style, Color};

pub struct TabBar {
    area: Rect,
    tabs: &'static [&'static str],
    active: usize,
}

impl TabBar {
    pub fn new(y: u16, tabs: &'static [&'static str]) -> Self {
        Self {
            area: Rect::new(0, y, 80, 1),
            tabs,
            active: 0,
        }
    }

    pub fn set_active(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active = idx;
        }
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }
}

impl Panel for TabBar {
    fn area(&self) -> Rect {
        self.area
    }

    fn render(&self, screen: &mut Screen) {
        let area = self.area;
        screen.fill(area, ' ', Style::colored(Color::BLACK, Color::BLUE));

        let mut x = 1u16;
        for (i, tab) in self.tabs.iter().enumerate() {
            let style = if i == self.active {
                Style::colored(Color::BLACK, Color::WHITE).reverse()
            } else {
                Style::colored(Color::WHITE, Color::BLUE)
            };

            let mut cx = x as usize;
            let y = area.y as usize;

            // Write " " prefix for active tab
            if i == self.active {
                if cx < 80 {
                    screen.cells[y][cx] = super::super::screen::Cell::new(' ', style);
                    cx += 1;
                }
            }

            for ch in tab.chars() {
                if cx < 80 {
                    screen.cells[y][cx] = super::super::screen::Cell::new(ch, style);
                    cx += 1;
                }
            }

            // Write " " suffix for active tab
            if i == self.active {
                if cx < 80 {
                    screen.cells[y][cx] = super::super::screen::Cell::new(' ', style);
                    cx += 1;
                }
            }

            // Write separator space
            if cx < 80 {
                screen.cells[y][cx] = super::super::screen::Cell::new(' ', Style::colored(Color::BLACK, Color::BLUE));
                cx += 1;
            }

            x = cx as u16;
        }
    }
}
