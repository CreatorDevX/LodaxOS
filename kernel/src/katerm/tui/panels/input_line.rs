use super::Panel;
use crate::katerm::tui::screen::{Screen, Rect, Style, Color};
use crate::katerm::tui::input::KeyEvent;

const MAX_INPUT: usize = 256;

pub struct InputLine {
    area: Rect,
    buf: [u8; MAX_INPUT],
    len: usize,
    cursor: usize,
    prompt: &'static str,
    history: [[u8; MAX_INPUT]; 32],
    history_lens: [usize; 32],
    history_count: usize,
    history_pos: usize,
    submit_callback: fn(&str),
    dirty: bool,
}

impl InputLine {
    pub fn new(y: u16, prompt: &'static str, submit_callback: fn(&str)) -> Self {
        Self {
            area: Rect::new(0, y, 80, 1),
            buf: [0u8; MAX_INPUT],
            len: 0,
            cursor: 0,
            prompt,
            history: [[0u8; MAX_INPUT]; 32],
            history_lens: [0usize; 32],
            history_count: 0,
            history_pos: 0,
            submit_callback,
            dirty: true,
        }
    }

    pub fn get_input(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_input(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let len = bytes.len().min(MAX_INPUT);
        self.buf[..len].copy_from_slice(&bytes[..len]);
        self.len = len;
        self.cursor = len;
        self.dirty = true;
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.cursor = 0;
        self.dirty = true;
    }

    fn push_history(&mut self) {
        if self.len == 0 {
            return;
        }
        let idx = (self.history_count) % 32;
        self.history[idx][..self.len].copy_from_slice(&self.buf[..self.len]);
        self.history_lens[idx] = self.len;
        if self.history_count < 32 {
            self.history_count += 1;
        }
        self.history_pos = self.history_count;
    }

    fn history_up(&mut self) {
        if self.history_count == 0 {
            return;
        }
        if self.history_pos > 0 {
            self.history_pos -= 1;
        }
        let idx = self.history_pos % 32;
        let len = self.history_lens[idx];
        self.buf[..len].copy_from_slice(&self.history[idx][..len]);
        self.len = len;
        self.cursor = len;
        self.dirty = true;
    }

    fn history_down(&mut self) {
        if self.history_pos < self.history_count {
            self.history_pos += 1;
        }
        if self.history_pos >= self.history_count {
            self.len = 0;
            self.cursor = 0;
        } else {
            let idx = self.history_pos % 32;
            let len = self.history_lens[idx];
            self.buf[..len].copy_from_slice(&self.history[idx][..len]);
            self.len = len;
            self.cursor = len;
        }
        self.dirty = true;
    }
}

impl Panel for InputLine {
    fn area(&self) -> Rect {
        self.area
    }

    fn render(&self, screen: &mut Screen) {
        let area = self.area;
        let y = area.y as usize;

        // Fill line
        for x in 0..80 {
            screen.cells[y][x] = super::super::screen::Cell::new(' ', Style::colored(Color::BLACK, Color::WHITE));
        }

        // Write prompt
        let mut cx = 0usize;
        let prompt_style = Style::colored(Color::BLACK, Color::GREEN);
        for ch in self.prompt.chars() {
            if cx < 80 {
                screen.cells[y][cx] = super::super::screen::Cell::new(ch, prompt_style);
                cx += 1;
            }
        }

        // Write input text
        let input_style = Style::colored(Color::BLACK, Color::WHITE);
        for i in 0..self.len {
            if cx < 79 {
                let ch = self.buf[i] as char;
                screen.cells[y][cx] = super::super::screen::Cell::new(ch, input_style);
                cx += 1;
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key {
            KeyEvent::Char(ch) => {
                if self.len < MAX_INPUT {
                    // Shift cursor position and insert
                    for i in (self.cursor..self.len).rev() {
                        self.buf[i + 1] = self.buf[i];
                    }
                    self.buf[self.cursor] = ch as u8;
                    self.len += 1;
                    self.cursor += 1;
                    self.dirty = true;
                }
                true
            }
            KeyEvent::Enter => {
                // Copy input to a local buffer before clearing
                let mut input_buf = [0u8; 256];
                let len = self.len.min(256);
                input_buf[..len].copy_from_slice(&self.buf[..len]);
                let input = core::str::from_utf8(&input_buf[..len]).unwrap_or("");

                self.push_history();
                self.len = 0;
                self.cursor = 0;
                self.dirty = true;
                (self.submit_callback)(input);
                true
            }
            KeyEvent::Backspace => {
                if self.cursor > 0 {
                    for i in self.cursor..self.len {
                        self.buf[i - 1] = self.buf[i];
                    }
                    self.cursor -= 1;
                    self.len -= 1;
                    self.dirty = true;
                }
                true
            }
            KeyEvent::Delete => {
                if self.cursor < self.len {
                    for i in self.cursor..self.len - 1 {
                        self.buf[i] = self.buf[i + 1];
                    }
                    self.len -= 1;
                    self.dirty = true;
                }
                true
            }
            KeyEvent::Left => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.dirty = true;
                }
                true
            }
            KeyEvent::Right => {
                if self.cursor < self.len {
                    self.cursor += 1;
                    self.dirty = true;
                }
                true
            }
            KeyEvent::Home => {
                self.cursor = 0;
                self.dirty = true;
                true
            }
            KeyEvent::End => {
                self.cursor = self.len;
                self.dirty = true;
                true
            }
            KeyEvent::Up => {
                self.history_up();
                true
            }
            KeyEvent::Down => {
                self.history_down();
                true
            }
            _ => false,
        }
    }

    fn set_active(&mut self, _active: bool) {}

    fn update(&mut self) {
        if self.dirty {
            self.dirty = false;
        }
    }
}
