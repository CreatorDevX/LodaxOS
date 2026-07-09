use super::Panel;
use crate::katerm::tui::screen::{Screen, Rect, Style, Color};

const MAX_LINES: usize = 200;
const MAX_LINE_LEN: usize = 256;

pub struct OutputPane {
    area: Rect,
    lines: [[u8; MAX_LINE_LEN]; MAX_LINES],
    line_lens: [usize; MAX_LINES],
    line_styles: [Style; MAX_LINES],
    head: usize,
    count: usize,
    scroll: usize,
    dirty: bool,
}

impl OutputPane {
    pub fn new(area: Rect) -> Self {
        Self {
            area,
            lines: [[0u8; MAX_LINE_LEN]; MAX_LINES],
            line_lens: [0usize; MAX_LINES],
            line_styles: [Style::plain(); MAX_LINES],
            head: 0,
            count: 0,
            scroll: 0,
            dirty: true,
        }
    }

    pub fn push_line(&mut self, text: &str, style: Style) {
        let idx = (self.head + self.count) % MAX_LINES;
        let bytes = text.as_bytes();
        let len = bytes.len().min(MAX_LINE_LEN);
        self.lines[idx][..len].copy_from_slice(&bytes[..len]);
        self.line_lens[idx] = len;
        self.line_styles[idx] = style;

        if self.count < MAX_LINES {
            self.count += 1;
        } else {
            self.head = (self.head + 1) % MAX_LINES;
        }

        // Auto-scroll to bottom
        self.scroll = 0;
        self.dirty = true;
    }

    pub fn push_output(&mut self, text: &str) {
        for line in text.split('\n') {
            if !line.is_empty() || self.count == 0 {
                self.push_line(line, Style::plain());
            }
        }
    }

    pub fn push_error(&mut self, text: &str) {
        for line in text.split('\n') {
            self.push_line(line, Style::colored(Color::RED, Color::BLACK));
        }
    }

    pub fn push_info(&mut self, text: &str) {
        for line in text.split('\n') {
            self.push_line(line, Style::colored(Color::CYAN, Color::BLACK));
        }
    }

    pub fn scroll_up(&mut self, amount: usize) {
        let max_scroll = self.count.saturating_sub(self.area.h as usize);
        self.scroll = (self.scroll + amount).min(max_scroll);
        self.dirty = true;
    }

    pub fn scroll_down(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_sub(amount);
        self.dirty = true;
    }

    pub fn scroll_page_up(&mut self) {
        self.scroll_up(self.area.h as usize - 2);
    }

    pub fn scroll_page_down(&mut self) {
        self.scroll_down(self.area.h as usize - 2);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
        self.dirty = true;
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.count = 0;
        self.scroll = 0;
        self.dirty = true;
    }
}

impl Panel for OutputPane {
    fn area(&self) -> Rect {
        self.area
    }

    fn render(&self, screen: &mut Screen) {
        let area = self.area;
        screen.fill(area, ' ', Style::plain());

        let visible_lines = area.h as usize;
        let total_lines = self.count;
        let start = if total_lines > visible_lines + self.scroll {
            total_lines - visible_lines - self.scroll
        } else {
            0
        };

        for (row, i) in (start..start + visible_lines).enumerate() {
            if i >= total_lines {
                break;
            }
            let idx = (self.head + i) % MAX_LINES;
            let len = self.line_lens[idx];
            let line = core::str::from_utf8(&self.lines[idx][..len]).unwrap_or("");
            let y = area.y as usize + row;
            let style = self.line_styles[idx];

            let mut cx = area.x as usize;
            for ch in line.chars() {
                if cx < (area.x as usize + area.w as usize) && y < 25 {
                    screen.cells[y][cx] = super::super::screen::Cell::new(ch, style);
                    cx += 1;
                }
            }
        }

        // Scroll indicator
        if total_lines > visible_lines {
            let bar_len = visible_lines;
            let thumb_len = ((visible_lines as f32 / total_lines as f32) * bar_len as f32) as usize;
            let thumb_pos = if total_lines > visible_lines {
                let max_scroll = total_lines - visible_lines;
                let pos = if max_scroll > 0 { (self.scroll as f32 / max_scroll as f32) * (bar_len - thumb_len) as f32 } else { 0.0 };
                pos as usize
            } else {
                0
            };

            let bar_x = (area.x + area.w - 1) as usize;
            for row in 0..bar_len {
                let y = area.y as usize + row;
                if y < 25 {
                    let ch = if row >= thumb_pos && row < thumb_pos + thumb_len {
                        '|'
                    } else {
                        '.'
                    };
                    screen.cells[y][bar_x] = super::super::screen::Cell::new(ch, Style::dim());
                }
            }
        }
    }

    fn handle_key(&mut self, key: crate::katerm::tui::input::KeyEvent) -> bool {
        use crate::katerm::tui::input::KeyEvent;
        match key {
            KeyEvent::PageUp => { self.scroll_page_up(); true }
            KeyEvent::PageDown => { self.scroll_page_down(); true }
            KeyEvent::Up => { self.scroll_up(1); true }
            KeyEvent::Down => { self.scroll_down(1); true }
            _ => false,
        }
    }

    fn update(&mut self) {
        if self.dirty {
            self.dirty = false;
        }
    }
}
