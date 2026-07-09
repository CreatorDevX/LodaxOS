use super::Panel;
use crate::katerm::tui::screen::{Screen, Rect, Style, Color};

pub struct StatusBar {
    area: Rect,
    left_text: [u8; 40],
    left_len: usize,
    right_text: [u8; 40],
    right_len: usize,
}

impl StatusBar {
    pub fn new(y: u16) -> Self {
        Self {
            area: Rect::new(0, y, 80, 1),
            left_text: [0u8; 40],
            left_len: 0,
            right_text: [0u8; 40],
            right_len: 0,
        }
    }

    pub fn set_left(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let len = bytes.len().min(40);
        self.left_text[..len].copy_from_slice(&bytes[..len]);
        self.left_len = len;
    }

    pub fn set_right(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let len = bytes.len().min(40);
        self.right_text[..len].copy_from_slice(&bytes[..len]);
        self.right_len = len;
    }
}

impl Panel for StatusBar {
    fn area(&self) -> Rect {
        self.area
    }

    fn render(&self, screen: &mut Screen) {
        let area = self.area;
        screen.fill(area, ' ', Style::colored(Color::BLACK, Color::WHITE));

        let y = area.y as usize;

        // Left side
        let left = core::str::from_utf8(&self.left_text[..self.left_len]).unwrap_or("");
        for (i, ch) in left.chars().enumerate() {
            if i < 79 {
                screen.cells[y][i] = super::super::screen::Cell::new(ch, Style::colored(Color::BLACK, Color::WHITE));
            }
        }

        // Right side
        let right = core::str::from_utf8(&self.right_text[..self.right_len]).unwrap_or("");
        let start = 80usize.saturating_sub(right.len());
        for (i, ch) in right.chars().enumerate() {
            let x = start + i;
            if x < 80 {
                screen.cells[y][x] = super::super::screen::Cell::new(ch, Style::colored(Color::BLACK, Color::WHITE));
            }
        }

        // Separator line
        let sep_style = Style::colored(Color::BLACK, Color::CYAN);
        if self.left_len > 0 && self.right_len > 0 {
            let sep_x = self.left_len + 2;
            if sep_x < 80 {
                screen.cells[y][sep_x] = super::super::screen::Cell::new('|', sep_style);
            }
        }
    }
}
