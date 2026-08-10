//! Spike-grade screen model: a scrollback of logical lines with a cursor
//! column, fed raw PTY bytes. Understands the control bytes a `TERM=dumb`
//! shell actually emits (\r \n \b \t BEL) plus enough of ESC/CSI/OSC to
//! swallow stray sequences and honor erase-line / clear-screen. This is a
//! placeholder for a real VT layer (alacritty_terminal) — deliberately no
//! grid, no colors, no cursor addressing.

const MAX_SCROLLBACK: usize = 5000;

enum EscState {
    Ground,
    Esc,
    /// ESC ( ) # % charset selectors: swallow exactly one following byte.
    EscTakeOne,
    Csi { params: String },
    Osc { esc_pending: bool },
}

pub struct Screen {
    /// Logical lines, oldest first; the last line is where the cursor lives.
    lines: Vec<Vec<char>>,
    pub cursor_col: usize,
    esc: EscState,
    /// Incomplete UTF-8 tail carried across feeds.
    pending: Vec<u8>,
}

impl Screen {
    pub fn new() -> Self {
        Screen { lines: vec![Vec::new()], cursor_col: 0, esc: EscState::Ground, pending: Vec::new() }
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// The last `rows` lines, ending `offset` lines above the bottom.
    pub fn visible(&self, rows: usize, offset: usize) -> &[Vec<char>] {
        let n = self.lines.len();
        let offset = offset.min(n.saturating_sub(1));
        let end = n - offset;
        let start = end.saturating_sub(rows);
        &self.lines[start..end]
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);
        let mut rest = buf.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    for c in s.chars() {
                        self.advance(c);
                    }
                    break;
                }
                Err(e) => {
                    let (valid, tail) = rest.split_at(e.valid_up_to());
                    // Unwrap is fine: split at valid_up_to is valid by construction.
                    for c in std::str::from_utf8(valid).unwrap().chars() {
                        self.advance(c);
                    }
                    match e.error_len() {
                        // Incomplete sequence at the end: keep for the next feed.
                        None => {
                            self.pending = tail.to_vec();
                            break;
                        }
                        // Invalid bytes mid-stream: emit U+FFFD and continue after.
                        Some(n) => {
                            self.advance('\u{fffd}');
                            rest = &tail[n..];
                        }
                    }
                }
            }
        }
    }

    fn advance(&mut self, c: char) {
        match &mut self.esc {
            EscState::Ground => match c {
                '\u{1b}' => self.esc = EscState::Esc,
                '\n' => self.newline(),
                '\r' => self.cursor_col = 0,
                '\u{8}' => self.cursor_col = self.cursor_col.saturating_sub(1),
                '\t' => self.cursor_col = (self.cursor_col / 8 + 1) * 8,
                c if (c as u32) < 0x20 || c == '\u{7f}' => {}
                c => self.put(c),
            },
            EscState::Esc => match c {
                '[' => self.esc = EscState::Csi { params: String::new() },
                ']' => self.esc = EscState::Osc { esc_pending: false },
                '(' | ')' | '#' | '%' => self.esc = EscState::EscTakeOne,
                _ => self.esc = EscState::Ground,
            },
            EscState::EscTakeOne => self.esc = EscState::Ground,
            EscState::Csi { params } => match c {
                '\u{20}'..='\u{3f}' => params.push(c),
                // Final byte: act on the few we honor, swallow the rest.
                '\u{40}'..='\u{7e}' => {
                    let params = std::mem::take(params);
                    self.esc = EscState::Ground;
                    match c {
                        'K' => {
                            let col = self.cursor_col;
                            let line = self.lines.last_mut().unwrap();
                            line.truncate(col);
                        }
                        'J' if params.starts_with('2') || params.starts_with('3') => {
                            self.lines = vec![Vec::new()];
                            self.cursor_col = 0;
                        }
                        _ => {}
                    }
                }
                _ => self.esc = EscState::Ground,
            },
            EscState::Osc { esc_pending } => match c {
                '\u{7}' => self.esc = EscState::Ground,
                '\u{1b}' => *esc_pending = true,
                '\\' if *esc_pending => self.esc = EscState::Ground,
                _ => *esc_pending = false,
            },
        }
    }

    fn newline(&mut self) {
        self.lines.push(Vec::new());
        if self.lines.len() > MAX_SCROLLBACK {
            let excess = self.lines.len() - MAX_SCROLLBACK;
            self.lines.drain(..excess);
        }
    }

    fn put(&mut self, c: char) {
        let line = self.lines.last_mut().unwrap();
        while line.len() < self.cursor_col {
            line.push(' ');
        }
        if self.cursor_col < line.len() {
            line[self.cursor_col] = c;
        } else {
            line.push(c);
        }
        self.cursor_col += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &Screen) -> Vec<String> {
        s.lines.iter().map(|l| l.iter().collect()).collect()
    }

    #[test]
    fn plain_lines() {
        let mut s = Screen::new();
        s.feed(b"hello\r\nworld");
        assert_eq!(text(&s), vec!["hello", "world"]);
        assert_eq!(s.cursor_col, 5);
    }

    #[test]
    fn carriage_return_overwrites() {
        let mut s = Screen::new();
        s.feed(b"abcdef\rXY");
        assert_eq!(text(&s), vec!["XYcdef"]);
    }

    #[test]
    fn csi_swallowed_and_erase_line() {
        let mut s = Screen::new();
        s.feed(b"ab\x1b[31mcd\x1b[0m");
        assert_eq!(text(&s), vec!["abcd"]);
        s.feed(b"\rZ\x1b[K");
        assert_eq!(text(&s), vec!["Z"]);
    }

    #[test]
    fn osc_swallowed() {
        let mut s = Screen::new();
        s.feed(b"\x1b]0;title\x07ok\x1b]2;t\x1b\\!");
        assert_eq!(text(&s), vec!["ok!"]);
    }

    #[test]
    fn split_utf8_across_feeds() {
        let mut s = Screen::new();
        let bytes = "héllo".as_bytes();
        s.feed(&bytes[..2]); // 'h' + first byte of é
        s.feed(&bytes[2..]);
        assert_eq!(text(&s), vec!["héllo"]);
    }

    #[test]
    fn backspace_and_tab() {
        let mut s = Screen::new();
        s.feed(b"ab\x08X\tY");
        // 'X' overwrites 'b' at col 1, tab jumps to col 8, 'Y' lands there.
        assert_eq!(text(&s), vec!["aX      Y"]);
    }
}
