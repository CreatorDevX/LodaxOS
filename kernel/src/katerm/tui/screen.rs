pub const WIDTH: usize = 80;
pub const HEIGHT: usize = 25;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u8);

impl Color {
    pub const BLACK: Color = Color(0);
    pub const RED: Color = Color(1);
    pub const GREEN: Color = Color(2);
    pub const YELLOW: Color = Color(3);
    pub const BLUE: Color = Color(4);
    pub const MAGENTA: Color = Color(5);
    pub const CYAN: Color = Color(6);
    pub const WHITE: Color = Color(7);
    pub const DEFAULT: Color = Color(9);

    pub fn ansi_fg(self) -> &'static str {
        match self.0 {
            0 => "\x1b[30m",
            1 => "\x1b[31m",
            2 => "\x1b[32m",
            3 => "\x1b[33m",
            4 => "\x1b[34m",
            5 => "\x1b[35m",
            6 => "\x1b[36m",
            7 => "\x1b[37m",
            _ => "\x1b[39m",
        }
    }

    pub fn ansi_bg(self) -> &'static str {
        match self.0 {
            0 => "\x1b[40m",
            1 => "\x1b[41m",
            2 => "\x1b[42m",
            3 => "\x1b[43m",
            4 => "\x1b[44m",
            5 => "\x1b[45m",
            6 => "\x1b[46m",
            7 => "\x1b[47m",
            _ => "\x1b[49m",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Style {
    pub const fn plain() -> Self {
        Self {
            fg: Color::WHITE,
            bg: Color::BLACK,
            bold: false,
            dim: false,
            underline: false,
            reverse: false,
        }
    }

    pub const fn bold() -> Self {
        Self {
            fg: Color::WHITE,
            bg: Color::BLACK,
            bold: true,
            dim: false,
            underline: false,
            reverse: false,
        }
    }

    pub const fn dim() -> Self {
        Self {
            fg: Color::WHITE,
            bg: Color::BLACK,
            bold: false,
            dim: true,
            underline: false,
            reverse: false,
        }
    }

    pub const fn colored(fg: Color, bg: Color) -> Self {
        Self {
            fg,
            bg,
            bold: false,
            dim: false,
            underline: false,
            reverse: false,
        }
    }

    pub const fn reverse(self) -> Self {
        Self { reverse: true, ..self }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

impl Cell {
    pub const fn empty() -> Self {
        Self {
            ch: ' ',
            style: Style::plain(),
        }
    }

    pub const fn new(ch: char, style: Style) -> Self {
        Self { ch, style }
    }
}

pub struct Screen {
    pub cells: [[Cell; WIDTH]; HEIGHT],
    pub prev_cells: [[Cell; WIDTH]; HEIGHT],
    pub cursor_x: u16,
    pub cursor_y: u16,
    pub cursor_visible: bool,
    dirty: bool,
}

impl Screen {
    pub const fn new() -> Self {
        Self {
            cells: [[Cell::empty(); WIDTH]; HEIGHT],
            prev_cells: [[Cell::empty(); WIDTH]; HEIGHT],
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: true,
            dirty: true,
        }
    }

    pub fn clear(&mut self) {
        self.cells = [[Cell::empty(); WIDTH]; HEIGHT];
        self.dirty = true;
    }

    pub fn fill(&mut self, area: Rect, ch: char, style: Style) {
        for y in area.y..area.y.saturating_add(area.h) {
            if y as usize >= HEIGHT {
                break;
            }
            for x in area.x..area.x.saturating_add(area.w) {
                if x as usize >= WIDTH {
                    break;
                }
                self.cells[y as usize][x as usize] = Cell::new(ch, style);
            }
        }
        self.dirty = true;
    }

    pub fn write_str(&mut self, x: u16, y: u16, s: &str, style: Style) {
        let mut cx = x as usize;
        let cy = y as usize;
        if cy >= HEIGHT {
            return;
        }
        for ch in s.chars() {
            if cx >= WIDTH {
                break;
            }
            self.cells[cy][cx] = Cell::new(ch, style);
            cx += 1;
        }
        self.dirty = true;
    }

    pub fn write_char(&mut self, x: u16, y: u16, ch: char, style: Style) {
        if (y as usize) < HEIGHT && (x as usize) < WIDTH {
            self.cells[y as usize][x as usize] = Cell::new(ch, style);
            self.dirty = true;
        }
    }

    pub fn set_cursor(&mut self, x: u16, y: u16) {
        self.cursor_x = x;
        self.cursor_y = y;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Render the diff between current and previous frame to the connector.
    pub fn render_diff(&mut self, conn: &dyn super::super::connector::Connector) {
        if !self.dirty {
            return;
        }

        use super::super::vtparser;

        // Hide cursor during render
        conn.write_str("\x1b[?25l");

        for y in 0..HEIGHT {
            let mut last_style = Style::plain();
            let mut x = 0;
            while x < WIDTH {
                let cell = self.cells[y][x];
                let prev = self.prev_cells[y][x];

                if cell == prev {
                    x += 1;
                    continue;
                }

                // Move cursor to start of changed region
                let _ = core::fmt::write(
                    &mut super::super::vtparser::ConnectorWriter { conn },
                    format_args!("\x1b[{};{}H", y + 1, x + 1),
                );

                // Write changed cells until we find an unchanged one
                while x < WIDTH {
                    let cell = self.cells[y][x];
                    let prev = self.prev_cells[y][x];
                    if cell == prev && cell.style == last_style {
                        break;
                    }

                    // Apply style changes
                    if cell.style != last_style {
                        self.write_style_diff(conn, last_style, cell.style);
                        last_style = cell.style;
                    }

                    conn.write_str(core::str::from_utf8(&[cell.ch as u8]).unwrap_or(" "));
                    self.prev_cells[y][x] = cell;
                    x += 1;
                }
            }
        }

        // Reset style at end
        conn.write_str(vtparser::sgr_reset());

        // Move cursor to desired position
        let _ = core::fmt::write(
            &mut super::super::vtparser::ConnectorWriter { conn },
            format_args!("\x1b[{};{}H", self.cursor_y + 1, self.cursor_x + 1),
        );

        // Show/hide cursor
        if self.cursor_visible {
            conn.write_str("\x1b[?25h");
        }

        self.dirty = false;
    }

    fn write_style_diff(&self, conn: &dyn super::super::connector::Connector, prev: Style, curr: Style) {
        use super::super::vtparser;

        // If any attribute changed, emit full SGR sequence
        if prev.fg != curr.fg || prev.bg != curr.bg
            || prev.bold != curr.bold || prev.dim != curr.dim
            || prev.underline != curr.underline || prev.reverse != curr.reverse
        {
            conn.write_str(vtparser::sgr_reset());
            if curr.bold {
                conn.write_str(vtparser::sgr_bold());
            }
            if curr.dim {
                conn.write_str(vtparser::sgr_dim());
            }
            if curr.underline {
                conn.write_str(vtparser::sgr_underline());
            }
            if curr.reverse {
                conn.write_str(vtparser::sgr_reverse());
            }
            conn.write_str(curr.fg.ansi_fg());
            conn.write_str(curr.bg.ansi_bg());
        }
    }

    /// Force a full redraw on next render
    pub fn force_redraw(&mut self) {
        self.prev_cells = [[Cell::empty(); WIDTH]; HEIGHT];
        self.dirty = true;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x.saturating_add(self.w)
            && y >= self.y && y < self.y.saturating_add(self.h)
    }
}
