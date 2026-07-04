use super::termcmds;
use super::vtparser;

/// Execute a complete command line.
pub fn execute(line: &str) {
    let conn = match super::connector::get_active() {
        Some(c) => c,
        None => return,
    };

    let line = line.trim();
    if line.is_empty() {
        return;
    }

    let paren_open = match line.find('(') {
        Some(i) => i,
        None => {
            conn.write_str(vtparser::sgr_fg_red());
            conn.write_str("error:");
            conn.write_str(vtparser::sgr_reset());
            conn.write_str(" expected '(' after command name, try help()\n");
            return;
        }
    };

    let paren_close = match line.rfind(')') {
        Some(i) => i,
        None => {
            conn.write_str(vtparser::sgr_fg_red());
            conn.write_str("error:");
            conn.write_str(vtparser::sgr_reset());
            conn.write_str(" expected ')'\n");
            return;
        }
    };

    let cmd_name = line[..paren_open].trim();
    let args_str = &line[paren_open + 1..paren_close];

    for cmd in termcmds::COMMANDS {
        if cmd.name == cmd_name {
            (cmd.exec)(args_str, conn);
            return;
        }
    }

    conn.write_str(vtparser::sgr_fg_red());
    conn.write_str("error:");
    conn.write_str(vtparser::sgr_reset());
    conn.write_str(" unknown command '");
    conn.write_str(cmd_name);
    conn.write_str("', try help()\n");
}

/// Simple argument parser for "cmd(arg1, arg2, arg3)" style commands.
pub struct Args<'a> {
    inner: &'a str,
}

impl<'a> Args<'a> {
    pub fn new(s: &'a str) -> Self {
        Self { inner: s.trim() }
    }

    pub fn parse_u64(&mut self) -> Option<u64> {
        self.inner = self.inner.trim();
        if self.inner.is_empty() {
            return None;
        }

        let end = self.inner.find(|c: char| c == ',' || c == ')' || c == ' ');
        let (token, rest) = match end {
            Some(i) => (&self.inner[..i], &self.inner[i..]),
            None => (self.inner, ""),
        };

        let token = token.trim();
        let val = if token.starts_with("0x") || token.starts_with("0X") {
            u64::from_str_radix(&token[2..], 16).ok()
        } else {
            u64::from_str_radix(token, 10).ok()
        };

        self.inner = rest.trim_start_matches(',').trim_start_matches(')').trim();
        if self.inner.is_empty() {
            self.inner = "";
        }
        val
    }

    pub fn parse_str(&mut self) -> Option<&'a str> {
        self.inner = self.inner.trim();
        if self.inner.is_empty() {
            return None;
        }

        let end = self.inner.find(|c: char| c == ',' || c == ')');
        let (token, rest) = match end {
            Some(i) => (&self.inner[..i], &self.inner[i..]),
            None => (self.inner, ""),
        };

        self.inner = rest.trim_start_matches(',').trim_start_matches(')').trim();
        if self.inner.is_empty() {
            self.inner = "";
        }
        Some(token.trim())
    }

    pub fn is_empty(&self) -> bool {
        self.inner.trim().is_empty()
    }
}
