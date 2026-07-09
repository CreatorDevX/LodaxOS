use alloc::format;
use crate::katerm::tui::screen::{Screen, Rect, Style, Color};
use crate::katerm::tui::input::KeyEvent;
use super::super::Panel;

pub struct TasksPanel {
    area: Rect,
    scroll: usize,
    output_buf: [u8; 4096],
    output_len: usize,
    dirty: bool,
}

impl TasksPanel {
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

impl Panel for TasksPanel {
    fn area(&self) -> Rect { self.area }
    fn title(&self) -> &str { "Tasks" }

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
            let style = if row == 0 {
                Style::colored(Color::YELLOW, Color::BLACK).reverse()
            } else if row % 2 == 0 {
                Style::plain()
            } else {
                Style::dim()
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

        self.push_str(" PID  Name                     State   VRuntime        vCPUs\n");

        let gt = crate::scheduler::GANG_TABLE.lock();
        for i in 0..gt.gangs.len() {
            if let Some(gang) = &gt.gangs[i] {
                let name = core::str::from_utf8(&gang.name).unwrap_or("???");
                let state_str = match gang.state {
                    crate::scheduler::GangState::Active => "Active ",
                    crate::scheduler::GangState::Halted => "Halted ",
                };
                self.push_str(&format!("{:4}  {:<24} {} {:>12}  {:>5}\n",
                    gang.id, name, state_str, gang.vruntime, gang.vcpu_count));
            }
        }

        self.dirty = false;
    }
}
