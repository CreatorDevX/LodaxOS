use alloc::format;
use crate::katerm::tui::screen::{Screen, Rect, Style, Color};
use crate::katerm::tui::input::KeyEvent;
use super::super::Panel;

pub struct MemoryPanel {
    area: Rect,
    scroll: usize,
    output_buf: [u8; 4096],
    output_len: usize,
    dirty: bool,
}

impl MemoryPanel {
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

impl Panel for MemoryPanel {
    fn area(&self) -> Rect { self.area }
    fn title(&self) -> &str { "Memory" }

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

        // Physical memory
        let total = crate::mm::phys::total_pages() * 4096;
        let free = crate::mm::phys::free_pages_count() * 4096;
        let used = total - free;

        self.push_str(" Physical Memory\n");
        self.push_str(&format!("   Total:  {} MB ({} pages)\n", total / 1024 / 1024, total / 4096));
        self.push_str(&format!("   Used:   {} MB ({} pages)\n", used / 1024 / 1024, used / 4096));
        self.push_str(&format!("   Free:   {} MB ({} pages)\n", free / 1024 / 1024, free / 4096));

        // Page allocator
        self.push_str("\n Page Allocator (per order)\n");
        let free_counts = crate::mm::phys::free_counts_per_order();
        for (order, count) in free_counts.iter().enumerate() {
            let block_size = 4096usize << order;
            let mb = (block_size as u64 * *count as u64) / 1024 / 1024;
            self.push_str(&format!("   Order {}: {:>8} free ({} MB)\n", order, count, mb));
        }

        // Slab allocator
        self.push_str("\n Slab Allocator\n");
        let stats = crate::mm::heap::slab_stats();
        let mut total_alloc = 0usize;
        let mut total_active = 0usize;
        for stat in stats.iter() {
            let active = stat.total_objs - stat.free_objs;
            total_alloc += active * stat.obj_size;
            total_active += active;
        }
        self.push_str(&format!("   {} caches, {} objects, {} bytes active\n",
            stats.len(), total_active, total_alloc));

        self.dirty = false;
    }
}
