use core::fmt;
use super::connector::Connector;

pub fn clear_screen() -> &'static str {
    "\x1b[2J\x1b[H"
}

pub fn sgr_reset() -> &'static str {
    "\x1b[0m"
}

pub fn sgr_bold() -> &'static str {
    "\x1b[1m"
}

pub fn sgr_dim() -> &'static str {
    "\x1b[2m"
}

pub fn sgr_underline() -> &'static str {
    "\x1b[4m"
}

pub fn sgr_reverse() -> &'static str {
    "\x1b[7m"
}

#[allow(dead_code)]
pub fn sgr_fg_black() -> &'static str { "\x1b[30m" }
pub fn sgr_fg_red() -> &'static str { "\x1b[31m" }
pub fn sgr_fg_green() -> &'static str { "\x1b[32m" }
pub fn sgr_fg_yellow() -> &'static str { "\x1b[33m" }
pub fn sgr_fg_blue() -> &'static str { "\x1b[34m" }
pub fn sgr_fg_magenta() -> &'static str { "\x1b[35m" }
pub fn sgr_fg_cyan() -> &'static str { "\x1b[36m" }
pub fn sgr_fg_white() -> &'static str { "\x1b[37m" }

#[allow(dead_code)]
pub fn sgr_bg_black() -> &'static str { "\x1b[40m" }
pub fn sgr_bg_red() -> &'static str { "\x1b[41m" }
pub fn sgr_bg_green() -> &'static str { "\x1b[42m" }
pub fn sgr_bg_yellow() -> &'static str { "\x1b[43m" }
pub fn sgr_bg_blue() -> &'static str { "\x1b[44m" }
pub fn sgr_bg_magenta() -> &'static str { "\x1b[45m" }
pub fn sgr_bg_cyan() -> &'static str { "\x1b[46m" }
pub fn sgr_bg_white() -> &'static str { "\x1b[47m" }

pub fn cursor_up(n: u16) -> &'static str {
    if n == 1 { "\x1b[A" }
    else { "" }
}

pub fn cursor_down(n: u16) -> &'static str {
    if n == 1 { "\x1b[B" }
    else { "" }
}

pub fn cursor_forward(n: u16) -> &'static str {
    if n == 1 { "\x1b[C" }
    else { "" }
}

pub fn cursor_back(n: u16) -> &'static str {
    if n == 1 { "\x1b[D" }
    else { "" }
}

pub fn erase_display() -> &'static str {
    "\x1b[2J"
}

pub fn erase_line() -> &'static str {
    "\x1b[K"
}

/// Helper: write formatted output to the active connector.
pub struct ConnectorWriter<'a> {
    pub conn: &'a dyn Connector,
}

impl<'a> fmt::Write for ConnectorWriter<'a> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.conn.write_str(s);
        Ok(())
    }
}

/// Write a formatted string to a connector.
pub fn write_fmt(conn: &dyn Connector, args: fmt::Arguments<'_>) {
    let _ = fmt::write(&mut ConnectorWriter { conn }, args);
}

/// Convenience macro for writing formatted output.
#[macro_export]
macro_rules! kprintf {
    ($conn:expr, $fmt:literal $(, $arg:expr)*) => {
        $crate::katerm::vtparser::write_fmt($conn, format_args!($fmt $(, $arg)*))
    };
}
