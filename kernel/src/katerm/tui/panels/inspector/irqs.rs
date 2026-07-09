use alloc::format;
use crate::katerm::tui::screen::{Screen, Rect, Style, Color};
use crate::katerm::tui::input::KeyEvent;
use super::super::Panel;

pub struct IrqsPanel {
    area: Rect,
    scroll: usize,
    output_buf: [u8; 4096],
    output_len: usize,
    dirty: bool,
}

impl IrqsPanel {
    pub fn new(area: Rect) -> Self {
        Self {
            area,
            scroll: 0,
            output_buf: [0u8; 4096],
            output_len: 0,
            dirty: true,
        }
    }

    fn push_str(&mut self, s: &str) {
        let bytes = s.as_bytes();
        let remaining = self.output_buf.len() - self.output_len;
        let len = bytes.len().min(remaining);
        self.output_buf[self.output_len..self.output_len + len].copy_from_slice(&bytes[..len]);
        self.output_len += len;
    }
}

impl Panel for IrqsPanel {
    fn area(&self) -> Rect { self.area }
    fn title(&self) -> &str { "IRQs" }

    fn render(&self, screen: &mut Screen) {
        let area = self.area;
        screen.fill(area, ' ', Style::plain());

        let text = core::str::from_utf8(&self.output_buf[..self.output_len]).unwrap_or("");
        let mut line_iter = text.split('\n');
        let visible = area.h as usize;

        for row in 0..visible {
            let line = match line_iter.next() {
                Some(l) => l,
                None => break,
            };
            let y = area.y as usize + row;
            if y >= 25 { break; }
            let style = if row == 0 || (row > 2 && line.starts_with(' ')) {
                Style::colored(Color::YELLOW, Color::BLACK).reverse()
            } else {
                Style::plain()
            };
            let mut cx = area.x as usize;
            for ch in line.chars() {
                if cx < (area.x as usize + area.w as usize) {
                    screen.cells[y][cx] = super::super::super::screen::Cell::new(ch, style);
                    cx += 1;
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key {
            KeyEvent::Up => { self.scroll = self.scroll.saturating_sub(1); self.dirty = true; true }
            KeyEvent::Down => { self.scroll = self.scroll.saturating_add(1); self.dirty = true; true }
            KeyEvent::PageUp => { self.scroll = self.scroll.saturating_sub(10); self.dirty = true; true }
            KeyEvent::PageDown => { self.scroll = self.scroll.saturating_add(10); self.dirty = true; true }
            _ => false,
        }
    }

    fn update(&mut self) {
        if !self.dirty { return; }
        self.output_len = 0;

        self.push_str(" Exception Counts\n");

        let exceptions: &[(u8, &str)] = &[
            (13, "#GP"),
            (14, "#PF"),
            (6, "#UD"),
            (0, "#DE"),
        ];
        for &(vec, name) in exceptions {
            let count = crate::arch::idt::read_irq_count(vec);
            self.push_str(&format!("   {:>4} ({:<6}) {:>12}\n", vec, name, count));
        }

        self.push_str("\n Device IRQs\n");
        for vec in 32u8..48u8 {
            let count = crate::arch::idt::read_irq_count(vec);
            if count > 0 {
                let name = match vec {
                    32 => "LAPIC Timer",
                    33 => "PIT",
                    36 => "COM2",
                    44 => "PS/2",
                    _ => "device",
                };
                self.push_str(&format!("   {:>4} ({:<12}) {:>12}\n", vec, name, count));
            }
        }

        self.push_str("\n IPI Counts\n");
        for vec in 200u8..210u8 {
            let count = crate::arch::idt::read_irq_count(vec);
            if count > 0 {
                self.push_str(&format!("   {:>4}  {:>12}\n", vec, count));
            }
        }

        self.dirty = false;
    }
}
