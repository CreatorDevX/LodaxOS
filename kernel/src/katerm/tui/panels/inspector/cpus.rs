use alloc::format;
use crate::katerm::tui::screen::{Screen, Rect, Style, Color};
use crate::katerm::tui::input::KeyEvent;
use super::super::Panel;

pub struct CpusPanel {
    area: Rect,
    scroll: usize,
    output_buf: [u8; 4096],
    output_len: usize,
    dirty: bool,
}

impl CpusPanel {
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

impl Panel for CpusPanel {
    fn area(&self) -> Rect { self.area }
    fn title(&self) -> &str { "CPUs" }

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

        self.push_str(" CPU   APIC ID  Online  Tasks     Timer Fires   Role\n");

        for slot in 0..lodaxos_system::MAX_CPUS {
            let pc = &crate::percpu::PERCPU[slot];
            let online = pc.online.load(core::sync::atomic::Ordering::Relaxed);
            if !online && slot > 0 { continue; }
            let apic_id = pc.apic_id.load(core::sync::atomic::Ordering::Relaxed);
            let tasks = pc.task_count.load(core::sync::atomic::Ordering::Relaxed);
            let fires = pc.timer_fires.load(core::sync::atomic::Ordering::Relaxed);
            let role = if slot == 0 { "BSP" } else { "AP" };

            self.push_str(&format!(" {:>3}   {:>6}   {:>5}   {:>5}   {:>12}   {}\n",
                slot, apic_id, if online { "yes" } else { "no" }, tasks, fires, role));
        }

        self.dirty = false;
    }
}
