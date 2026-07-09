#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    Char(char),
    Enter,
    Backspace,
    Tab,
    ShiftTab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
    Ctrl(char),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Csi,
    CsiParam,
    Osc,
}

pub struct InputParser {
    state: State,
    param: [u8; 8],
    param_len: usize,
}

impl InputParser {
    pub const fn new() -> Self {
        Self {
            state: State::Ground,
            param: [0u8; 8],
            param_len: 0,
        }
    }

    pub fn feed(&mut self, byte: u8) -> Option<KeyEvent> {
        match self.state {
            State::Ground => {
                if byte == 0x1B {
                    self.state = State::Esc;
                    return None;
                }
                Some(self.decode_ground(byte))
            }
            State::Esc => {
                match byte {
                    b'[' => {
                        self.state = State::Csi;
                        self.param_len = 0;
                        self.param = [0u8; 8];
                        None
                    }
                    b'O' => {
                        self.state = State::Osc;
                        None
                    }
                    _ => {
                        self.state = State::Ground;
                        None
                    }
                }
            }
            State::Csi => {
                if byte.is_ascii_digit() || byte == b';' {
                    self.state = State::CsiParam;
                    self.param_len = 0;
                    self.param = [0u8; 8];
                    if byte.is_ascii_digit() {
                        self.param[0] = byte - b'0';
                        self.param_len = 1;
                    }
                    None
                } else {
                    let key = self.decode_csi(b'\0', byte);
                    self.state = State::Ground;
                    key
                }
            }
            State::CsiParam => {
                if byte.is_ascii_digit() {
                    let idx = self.param_len.min(7);
                    self.param[idx] = self.param[idx].saturating_mul(10).saturating_add(byte - b'0');
                    None
                } else if byte == b';' {
                    self.param_len = (self.param_len + 1).min(7);
                    None
                } else {
                    let key = self.decode_csi(self.param[0], byte);
                    self.state = State::Ground;
                    key
                }
            }
            State::Osc => {
                self.state = State::Ground;
                None
            }
        }
    }

    fn decode_ground(&self, byte: u8) -> KeyEvent {
        match byte {
            0x09 => KeyEvent::Tab,
            0x0D | 0x0A => KeyEvent::Enter,
            0x08 | 0x7F => KeyEvent::Backspace,
            0x03 => KeyEvent::Ctrl('c'),
            0x04 => KeyEvent::Ctrl('d'),
            0x0C => KeyEvent::Ctrl('l'),
            0x01 => KeyEvent::Home,
            0x05 => KeyEvent::End,
            0x0B => KeyEvent::Up,
            b @ 0x20..=0x7E => KeyEvent::Char(b as char),
            _ => KeyEvent::Char(byte as char),
        }
    }

    fn decode_csi(&self, param: u8, code: u8) -> Option<KeyEvent> {
        match code {
            b'A' => Some(KeyEvent::Up),
            b'B' => Some(KeyEvent::Down),
            b'C' => Some(KeyEvent::Right),
            b'D' => Some(KeyEvent::Left),
            b'H' => Some(KeyEvent::Home),
            b'F' => Some(KeyEvent::End),
            b'Z' => Some(KeyEvent::ShiftTab),
            b'~' => match param {
                1 => Some(KeyEvent::Home),
                2 => Some(KeyEvent::Insert),
                3 => Some(KeyEvent::Delete),
                4 => Some(KeyEvent::End),
                5 => Some(KeyEvent::PageUp),
                6 => Some(KeyEvent::PageDown),
                11..=24 => Some(KeyEvent::F(param - 10)),
                _ => None,
            },
            b'P' => Some(KeyEvent::F(1)),
            b'Q' => Some(KeyEvent::F(2)),
            b'R' => Some(KeyEvent::F(3)),
            b'S' => Some(KeyEvent::F(4)),
            _ => None,
        }
    }

    pub fn reset(&mut self) {
        self.state = State::Ground;
        self.param_len = 0;
    }
}
