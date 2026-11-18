//! POSIX Pseudo-Terminal (PTY) session manager and VT100/ANSI/TrueColor screen emulator.
//!
//! Provides high-performance native terminal emulation for Lava extensions (`@lava_term`),
//! keeping full interactive shell state, alternate screen switching, SGR 16/256/TrueColor spans,
//! scrollback history, window resize (`TIOCSWINSZ`), and live `/proc/<pid>/cwd` inspection.

#[cfg(unix)]
use std::collections::BTreeMap;
#[cfg(unix)]
use std::ffi::CStr;
#[cfg(unix)]
use std::fs::{self, File};
use std::io::Write;
#[cfg(unix)]
use std::io::{self, Read};
#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::thread;

/// Packed `0xRRGGBBAA` helper matching `lava_proto::ColorRgba` and `@lava_sys.rgba`.
#[inline]
pub const fn pack_rgba(r: u8, g: u8, b: u8, a: u8) -> i64 {
    (((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | (a as u32)) as i64
}

/// Contiguous styled run on a rendered terminal line (`0` = default theme fg/bg).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermColorSpan {
    pub row: i64,
    pub start_col: i64,
    pub end_col: i64,
    pub fg_rgba: i64,
    pub bg_rgba: i64,
    pub bold: bool,
}

/// Immutable terminal snapshot exported across the host bridge to Nessa.
#[derive(Debug, Clone)]
pub struct TermSnapshot {
    pub lines: Vec<String>,
    pub spans: Vec<TermColorSpan>,
    pub cursor_row: i64,
    pub cursor_col: i64,
    pub cursor_visible: bool,
    pub alt_screen: bool,
    pub app_cursor: bool,
    pub cwd: String,
    pub title: String,
    pub alive: bool,
    pub exit_code: i64,
    pub rows: usize,
    pub cols: usize,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CellStyle {
    fg: i64,
    bg: i64,
    bold: bool,
    reverse: bool,
}

impl CellStyle {
    fn effective_colors(self) -> (i64, i64) {
        if self.reverse {
            let fg = if self.bg != 0 {
                self.bg
            } else {
                pack_rgba(30, 30, 46, 255)
            };
            let bg = if self.fg != 0 {
                self.fg
            } else {
                pack_rgba(205, 214, 244, 255)
            };
            (fg, bg)
        } else {
            (self.fg, self.bg)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VtCell {
    ch: char,
    fg: i64,
    bg: i64,
    bold: bool,
}

impl Default for VtCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: 0,
            bg: 0,
            bold: false,
        }
    }
}

#[derive(Debug, Clone)]
struct ScrollbackLine {
    text: String,
    spans: Vec<TermColorSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserState {
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    OscEsc,
    Dcs,
    DcsEsc,
    Apc,
    ApcEsc,
    Charset,
}

fn char_cell_width(ch: char) -> usize {
    let cp = ch as u32;
    if cp < 0x80 {
        return 1;
    }
    if matches!(cp, 0x0300..=0x036F | 0x200B..=0x200D | 0xFE00..=0xFE0F) {
        return 0;
    }
    if matches!(
        cp,
        0x1100..=0x115F
            | 0x2329..=0x232A
            | 0x2E80..=0x303E
            | 0x3040..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0xA4CF
            | 0xA960..=0xA97F
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF01..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
            | 0x20000..=0x3FFFD
    ) {
        return 2;
    }
    1
}

/// Stateful VT100 / xterm-256color / TrueColor terminal screen & scrollback parser.
pub struct VtScreen {
    rows: usize,
    cols: usize,
    grid: Vec<Vec<VtCell>>,
    alt_grid: Vec<Vec<VtCell>>,
    in_alt_screen: bool,
    cursor_r: usize,
    cursor_c: usize,
    saved_cursor: (usize, usize),
    alt_saved_cursor: (usize, usize),
    scroll_top: usize,
    scroll_bottom: usize,
    style: CellStyle,
    cursor_visible: bool,
    app_cursor: bool,
    bracketed_paste: bool,
    pending_wrap: bool,
    scrollback: Vec<ScrollbackLine>,
    max_scrollback: usize,
    title: String,
    osc_cwd: Option<String>,
    state: ParserState,
    csi_buf: String,
    osc_buf: Vec<u8>,
    dcs_buf: Vec<u8>,
    utf8_buf: [u8; 4],
    utf8_len: usize,
    utf8_need: usize,
}

impl VtScreen {
    pub fn new(rows: usize, cols: usize, max_scrollback: usize) -> Self {
        let r = rows.clamp(2, 300);
        let c = cols.clamp(4, 500);
        Self {
            rows: r,
            cols: c,
            grid: vec![vec![VtCell::default(); c]; r],
            alt_grid: vec![vec![VtCell::default(); c]; r],
            in_alt_screen: false,
            cursor_r: 0,
            cursor_c: 0,
            saved_cursor: (0, 0),
            alt_saved_cursor: (0, 0),
            scroll_top: 0,
            scroll_bottom: r - 1,
            style: CellStyle::default(),
            cursor_visible: true,
            app_cursor: false,
            bracketed_paste: false,
            pending_wrap: false,
            scrollback: Vec::new(),
            max_scrollback: max_scrollback.max(64),
            title: String::new(),
            osc_cwd: None,
            state: ParserState::Ground,
            csi_buf: String::with_capacity(32),
            osc_buf: Vec::with_capacity(64),
            dcs_buf: Vec::with_capacity(64),
            utf8_buf: [0; 4],
            utf8_len: 0,
            utf8_need: 0,
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn resize(&mut self, new_rows: usize, new_cols: usize) {
        let r = new_rows.clamp(2, 300);
        let c = new_cols.clamp(4, 500);
        if r == self.rows && c == self.cols {
            return;
        }
        Self::resize_grid(&mut self.grid, r, c);
        Self::resize_grid(&mut self.alt_grid, r, c);
        self.rows = r;
        self.cols = c;
        self.scroll_top = 0;
        self.scroll_bottom = r - 1;
        self.cursor_r = self.cursor_r.min(r - 1);
        self.cursor_c = self.cursor_c.min(c - 1);
        self.pending_wrap = false;
    }

    fn resize_grid(grid: &mut Vec<Vec<VtCell>>, new_rows: usize, new_cols: usize) {
        for row in grid.iter_mut() {
            row.resize(new_cols, VtCell::default());
        }
        grid.resize(new_rows, vec![VtCell::default(); new_cols]);
    }

    fn active_grid_mut(&mut self) -> &mut Vec<Vec<VtCell>> {
        if self.in_alt_screen {
            &mut self.alt_grid
        } else {
            &mut self.grid
        }
    }

    fn active_grid(&self) -> &Vec<Vec<VtCell>> {
        if self.in_alt_screen {
            &self.alt_grid
        } else {
            &self.grid
        }
    }

    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        let mut discarded_replies = Vec::new();
        self.feed_bytes_with_replies(bytes, &mut discarded_replies);
    }

    pub fn feed_bytes_with_replies(&mut self, bytes: &[u8], replies: &mut Vec<u8>) {
        for &b in bytes {
            match self.state {
                ParserState::Osc => {
                    if b == 0x07 {
                        self.finish_osc(b"\x07", replies);
                        self.state = ParserState::Ground;
                    } else if b == 0x1b {
                        self.state = ParserState::OscEsc;
                    } else if b == 0x18 || b == 0x1a {
                        self.osc_buf.clear();
                        self.state = ParserState::Ground;
                    } else if self.osc_buf.len() < 4096 {
                        self.osc_buf.push(b);
                    }
                    continue;
                }
                ParserState::OscEsc => {
                    if b == b'\\' {
                        self.finish_osc(b"\x1b\\", replies);
                    }
                    self.state = ParserState::Ground;
                    continue;
                }
                ParserState::Dcs => {
                    if b == 0x07 {
                        self.finish_dcs(replies);
                        self.state = ParserState::Ground;
                    } else if b == 0x1b {
                        self.state = ParserState::DcsEsc;
                    } else if b == 0x18 || b == 0x1a {
                        self.dcs_buf.clear();
                        self.state = ParserState::Ground;
                    } else if self.dcs_buf.len() < 4096 {
                        self.dcs_buf.push(b);
                    }
                    continue;
                }
                ParserState::DcsEsc => {
                    if b == b'\\' {
                        self.finish_dcs(replies);
                    }
                    self.state = ParserState::Ground;
                    continue;
                }
                ParserState::Apc => {
                    if b == 0x07 || b == 0x18 || b == 0x1a {
                        self.state = ParserState::Ground;
                    } else if b == 0x1b {
                        self.state = ParserState::ApcEsc;
                    }
                    continue;
                }
                ParserState::ApcEsc => {
                    if b == b'\\' || b == 0x07 || b == 0x18 || b == 0x1a {
                        self.state = ParserState::Ground;
                    } else if b != 0x1b {
                        self.state = ParserState::Apc;
                    }
                    continue;
                }
                _ => {}
            }

            // Streaming UTF-8 multi-byte sequence assembly in Ground state
            if self.state == ParserState::Ground && self.utf8_need > 0 {
                if (b & 0b1100_0000) == 0b1000_0000 {
                    self.utf8_buf[self.utf8_len] = b;
                    self.utf8_len += 1;
                    if self.utf8_len == self.utf8_need {
                        let ch = std::str::from_utf8(&self.utf8_buf[..self.utf8_len])
                            .ok()
                            .and_then(|s| s.chars().next())
                            .unwrap_or('\u{FFFD}');
                        self.utf8_len = 0;
                        self.utf8_need = 0;
                        self.put_char(ch);
                    }
                    continue;
                } else {
                    self.utf8_len = 0;
                    self.utf8_need = 0;
                }
            }

            match self.state {
                ParserState::Ground => match b {
                    0x1b => {
                        self.state = ParserState::Escape;
                    }
                    b'\r' => {
                        self.cursor_c = 0;
                        self.pending_wrap = false;
                    }
                    b'\n' | 0x0b | 0x0c => {
                        self.line_feed();
                    }
                    0x08 => {
                        self.pending_wrap = false;
                        self.cursor_c = self.cursor_c.saturating_sub(1);
                    }
                    b'\t' => {
                        self.pending_wrap = false;
                        let next_stop = ((self.cursor_c / 8) + 1) * 8;
                        self.cursor_c = next_stop.min(self.cols - 1);
                    }
                    0x00..=0x1f | 0x7f => {}
                    0x20..=0x7e => {
                        self.put_char(b as char);
                    }
                    _ => {
                        let need = if (b & 0b1110_0000) == 0b1100_0000 {
                            2
                        } else if (b & 0b1111_0000) == 0b1110_0000 {
                            3
                        } else if (b & 0b1111_1000) == 0b1111_0000 {
                            4
                        } else {
                            0
                        };
                        if need >= 2 {
                            self.utf8_buf[0] = b;
                            self.utf8_len = 1;
                            self.utf8_need = need;
                        }
                    }
                },
                ParserState::Escape => match b {
                    b'[' => {
                        self.csi_buf.clear();
                        self.state = ParserState::Csi;
                    }
                    b']' => {
                        self.osc_buf.clear();
                        self.state = ParserState::Osc;
                    }
                    b'P' => {
                        self.dcs_buf.clear();
                        self.state = ParserState::Dcs;
                    }
                    b'k' => {
                        // GNU Screen / tmux window title (`ESC k <title> ESC \`)
                        self.osc_buf.clear();
                        self.osc_buf.extend_from_slice(b"2;");
                        self.state = ParserState::Osc;
                    }
                    b'_' | b'^' | b'X' => {
                        // APC (`ESC _`), PM (`ESC ^`), SOS (`ESC X`)
                        self.state = ParserState::Apc;
                    }
                    b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => {
                        self.state = ParserState::Charset;
                    }
                    0x20..=0x2f => {
                        // Intermediate ESC sequences such as `ESC # 8`, `ESC % G`, `ESC SP F`
                        self.state = ParserState::EscapeIntermediate;
                    }
                    b'7' => {
                        self.saved_cursor = (self.cursor_r, self.cursor_c);
                        self.state = ParserState::Ground;
                    }
                    b'8' => {
                        let (sr, sc) = self.saved_cursor;
                        self.cursor_r = sr.min(self.rows - 1);
                        self.cursor_c = sc.min(self.cols - 1);
                        self.pending_wrap = false;
                        self.state = ParserState::Ground;
                    }
                    b'M' => {
                        // Reverse index (RI)
                        self.pending_wrap = false;
                        if self.cursor_r == self.scroll_top {
                            self.scroll_down(1);
                        } else {
                            self.cursor_r = self.cursor_r.saturating_sub(1);
                        }
                        self.state = ParserState::Ground;
                    }
                    b'D' => {
                        self.line_feed();
                        self.state = ParserState::Ground;
                    }
                    b'E' => {
                        self.cursor_c = 0;
                        self.line_feed();
                        self.state = ParserState::Ground;
                    }
                    b'c' => {
                        // Full terminal reset (RIS)
                        let r = self.rows;
                        let c = self.cols;
                        *self = Self::new(r, c, self.max_scrollback);
                    }
                    b'=' | b'>' | b'\\' => {
                        self.state = ParserState::Ground;
                    }
                    _ => {
                        self.state = ParserState::Ground;
                    }
                },
                ParserState::EscapeIntermediate => {
                    if !(0x20..=0x2f).contains(&b) {
                        self.state = ParserState::Ground;
                    }
                }
                ParserState::Charset => {
                    self.state = ParserState::Ground;
                }
                ParserState::Csi => {
                    if b == 0x1b {
                        self.csi_buf.clear();
                        self.state = ParserState::Escape;
                    } else if b == 0x18 || b == 0x1a {
                        self.csi_buf.clear();
                        self.state = ParserState::Ground;
                    } else if (0x40..=0x7e).contains(&b) {
                        self.dispatch_csi(b as char, replies);
                        self.state = ParserState::Ground;
                    } else if (0x20..=0x3f).contains(&b) {
                        if self.csi_buf.len() < 128 {
                            self.csi_buf.push(b as char);
                        } else {
                            self.state = ParserState::Ground;
                        }
                    }
                }
                ParserState::Osc
                | ParserState::OscEsc
                | ParserState::Dcs
                | ParserState::DcsEsc
                | ParserState::Apc
                | ParserState::ApcEsc => {}
            }
        }
    }

    fn put_char(&mut self, ch: char) {
        let w = char_cell_width(ch);
        if w == 0 {
            return;
        }
        if self.pending_wrap || (w == 2 && self.cursor_c + 1 >= self.cols) {
            if self.cursor_c < self.cols && !self.pending_wrap {
                let r = self.cursor_r.min(self.rows - 1);
                let c = self.cursor_c;
                self.active_grid_mut()[r][c] = VtCell::default();
            }
            self.cursor_c = 0;
            self.pending_wrap = false;
            self.line_feed();
        }
        let (fg, bg) = self.style.effective_colors();
        let bold = self.style.bold;
        let r = self.cursor_r.min(self.rows - 1);
        let c = self.cursor_c.min(self.cols - 1);
        let cols = self.cols;
        let grid = self.active_grid_mut();

        // Clean up half-overwritten wide character cells
        if grid[r][c].ch == '\0' && c > 0 {
            grid[r][c - 1].ch = ' ';
        }
        if c + 1 < cols && grid[r][c + 1].ch == '\0' {
            grid[r][c + 1] = VtCell::default();
        }
        if w == 2 && c + 1 < cols {
            if c + 2 < cols && grid[r][c + 2].ch == '\0' {
                grid[r][c + 2] = VtCell::default();
            }
            grid[r][c] = VtCell { ch, fg, bg, bold };
            grid[r][c + 1] = VtCell {
                ch: '\0',
                fg,
                bg,
                bold,
            };
            if c + 2 >= cols {
                self.cursor_c = cols - 1;
                self.pending_wrap = true;
            } else {
                self.cursor_c = c + 2;
            }
        } else {
            grid[r][c] = VtCell { ch, fg, bg, bold };
            if c + 1 >= cols {
                self.pending_wrap = true;
            } else {
                self.cursor_c = c + 1;
            }
        }
    }

    fn line_feed(&mut self) {
        self.pending_wrap = false;
        if self.cursor_r == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor_r + 1 < self.rows {
            self.cursor_r += 1;
        }
    }

    fn scroll_up(&mut self, count: usize) {
        let top = self.scroll_top.min(self.rows - 1);
        let bot = self.scroll_bottom.min(self.rows - 1);
        if top > bot {
            return;
        }
        let cols = self.cols;
        let record_sb = !self.in_alt_screen && top == 0;
        for _ in 0..count {
            if record_sb {
                let (text, spans) = Self::extract_row(&self.grid[top], 0);
                self.scrollback.push(ScrollbackLine { text, spans });
                if self.scrollback.len() > self.max_scrollback {
                    self.scrollback.remove(0);
                }
            }
            let grid = self.active_grid_mut();
            for r in top..bot {
                grid[r] = grid[r + 1].clone();
            }
            grid[bot] = vec![VtCell::default(); cols];
        }
    }

    fn scroll_down(&mut self, count: usize) {
        let top = self.scroll_top.min(self.rows - 1);
        let bot = self.scroll_bottom.min(self.rows - 1);
        if top > bot {
            return;
        }
        let cols = self.cols;
        let grid = self.active_grid_mut();
        for _ in 0..count {
            for r in ((top + 1)..=bot).rev() {
                grid[r] = grid[r - 1].clone();
            }
            grid[top] = vec![VtCell::default(); cols];
        }
    }

    fn finish_dcs(&mut self, replies: &mut Vec<u8>) {
        if self.dcs_buf.starts_with(b">|") {
            // XTVERSION (`DCS > | ... ST`)
            replies.extend_from_slice(b"\x1bP>|lava-term(0.1)\x1b\\");
        } else if self.dcs_buf.starts_with(b"+q") {
            // XTGETTCAP (`DCS + q <hex> ST`): reply `0+r` (capability not overridden) immediately
            replies.extend_from_slice(b"\x1bP0+r\x1b\\");
        } else if let Some(rest) = self.dcs_buf.strip_prefix(b"$q") {
            // DECRQSS (`DCS $ q <pt> ST`)
            if rest == b"m" {
                replies.extend_from_slice(b"\x1bP1$r0m\x1b\\");
            } else {
                replies.extend_from_slice(b"\x1bP0$r\x1b\\");
            }
        }
        self.dcs_buf.clear();
    }

    fn finish_osc(&mut self, terminator: &[u8], replies: &mut Vec<u8>) {
        let Ok(s) = std::str::from_utf8(&self.osc_buf) else {
            self.osc_buf.clear();
            return;
        };
        let Some((code, rest)) = s.split_once(';') else {
            self.osc_buf.clear();
            return;
        };
        match code {
            "0" | "2" => {
                self.title = rest.to_string();
            }
            "4" => {
                // Palette query: `OSC 4 ; <idx> ; ?`
                if let Some((idx_str, q)) = rest.split_once(';')
                    && q.trim() == "?"
                    && let Ok(idx) = idx_str.trim().parse::<usize>()
                    && idx < 256
                {
                    let packed = xterm_256_color(idx) as u32;
                    let r = (packed >> 24) & 0xff;
                    let g = (packed >> 16) & 0xff;
                    let b = (packed >> 8) & 0xff;
                    let _ = write!(
                        replies,
                        "\x1b]4;{idx};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}"
                    );
                    replies.extend_from_slice(terminator);
                }
            }
            "7" => {
                // OSC 7: `file://hostname/path/to/dir`
                if let Some(after_scheme) = rest.strip_prefix("file://")
                    && let Some(slash_pos) = after_scheme.find('/')
                {
                    let raw_path = &after_scheme[slash_pos..];
                    if !raw_path.is_empty() {
                        self.osc_cwd = Some(percent_decode(raw_path));
                    }
                }
            }
            "10" => {
                // Default foreground color query (`OSC 10 ; ?`) — Catppuccin Mocha `#cdd6f4`
                if rest.trim() == "?" {
                    replies.extend_from_slice(b"\x1b]10;rgb:cdcd/d6d6/f4f4");
                    replies.extend_from_slice(terminator);
                }
            }
            "11" => {
                // Default background color query (`OSC 11 ; ?`) — Catppuccin Mocha `#1e1e2e`
                if rest.trim() == "?" {
                    replies.extend_from_slice(b"\x1b]11;rgb:1e1e/1e1e/2e2e");
                    replies.extend_from_slice(terminator);
                }
            }
            "12" if rest.trim() == "?" => {
                // Cursor color query (`OSC 12 ; ?`) — Catppuccin Rosewater `#f5e0dc`
                replies.extend_from_slice(b"\x1b]12;rgb:f5f5/e0e0/dcdc");
                replies.extend_from_slice(terminator);
            }
            _ => {}
        }
        self.osc_buf.clear();
    }

    fn split_csi_parts(raw: &str) -> (&str, &str, &str) {
        let bytes = raw.as_bytes();
        let mut p_end = 0;
        while p_end < bytes.len() && (0x3c..=0x3f).contains(&bytes[p_end]) {
            p_end += 1;
        }
        let mut i_start = bytes.len();
        while i_start > p_end && (0x20..=0x2f).contains(&bytes[i_start - 1]) {
            i_start -= 1;
        }
        (&raw[..p_end], &raw[p_end..i_start], &raw[i_start..])
    }

    fn parse_csi_params(params_raw: &str) -> Vec<usize> {
        if params_raw.is_empty() {
            return Vec::new();
        }
        params_raw
            .split(';')
            .map(|p| {
                let primary = p.split(':').next().unwrap_or("");
                primary.parse::<usize>().unwrap_or(0)
            })
            .collect()
    }

    fn dispatch_csi(&mut self, final_ch: char, replies: &mut Vec<u8>) {
        let buf = self.csi_buf.clone();
        let (prefix, params_raw, intermediate) = Self::split_csi_parts(&buf);
        let params = Self::parse_csi_params(params_raw);
        let p0 = |def: usize| params.first().copied().filter(|&v| v > 0).unwrap_or(def);
        let p1 = |def: usize| params.get(1).copied().filter(|&v| v > 0).unwrap_or(def);

        // 1. DECRQM mode status queries (`CSI ? Ps $ p` and `CSI Ps $ p`)
        if intermediate == "$" && final_ch == 'p' {
            let mode = params.first().copied().unwrap_or(0);
            if prefix == "?" {
                let pm = match mode {
                    1 => {
                        if self.app_cursor {
                            1
                        } else {
                            2
                        }
                    }
                    25 => {
                        if self.cursor_visible {
                            1
                        } else {
                            2
                        }
                    }
                    47 | 1047 | 1049 => {
                        if self.in_alt_screen {
                            1
                        } else {
                            2
                        }
                    }
                    2004 => {
                        if self.bracketed_paste {
                            1
                        } else {
                            2
                        }
                    }
                    1004 | 2026 => 2,
                    _ => 0,
                };
                let _ = write!(replies, "\x1b[?{mode};{pm}$y");
            } else if prefix.is_empty() {
                let _ = write!(replies, "\x1b[{mode};0$y");
            }
            return;
        }

        // Ignore any other CSI sequence carrying intermediate bytes (e.g., `CSI Ps SP q` cursor shape)
        if !intermediate.is_empty() {
            return;
        }

        // 2. Private / prefixed CSI sequences (`?`, `>`, `<`, `=`, `!`)
        match prefix {
            "?" => {
                match final_ch {
                    'h' | 'l' => {
                        let set_mode = final_ch == 'h';
                        for &mode in &params {
                            match mode {
                                1 => self.app_cursor = set_mode,
                                25 => self.cursor_visible = set_mode,
                                2004 => self.bracketed_paste = set_mode,
                                47 | 1047 | 1049 => {
                                    if set_mode && !self.in_alt_screen {
                                        self.alt_saved_cursor = (self.cursor_r, self.cursor_c);
                                        self.in_alt_screen = true;
                                        self.alt_grid =
                                            vec![vec![VtCell::default(); self.cols]; self.rows];
                                        self.cursor_r = 0;
                                        self.cursor_c = 0;
                                        self.pending_wrap = false;
                                    } else if !set_mode && self.in_alt_screen {
                                        self.in_alt_screen = false;
                                        let (sr, sc) = self.alt_saved_cursor;
                                        self.cursor_r = sr.min(self.rows - 1);
                                        self.cursor_c = sc.min(self.cols - 1);
                                        self.pending_wrap = false;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    'n' => {
                        // DECXCPR (`CSI ? 6 n`) extended cursor position report
                        match params.first().copied().unwrap_or(0) {
                            5 => replies.extend_from_slice(b"\x1b[?0n"),
                            6 => {
                                let _ = write!(
                                    replies,
                                    "\x1b[?{};{}R",
                                    self.cursor_r + 1,
                                    self.cursor_c + 1
                                );
                            }
                            _ => {}
                        }
                    }
                    'u' => {
                        // Kitty keyboard protocol query (`CSI ? u`)
                        replies.extend_from_slice(b"\x1b[?0u");
                    }
                    'c' => {
                        replies.extend_from_slice(b"\x1b[?62;22c");
                    }
                    _ => {}
                }
                return;
            }
            ">" => {
                // Secondary Device Attributes (`CSI > c` / `CSI > 0 c`), ignoring `CSI > 4 ; m` / `CSI > u`
                if final_ch == 'c' && params.first().copied().unwrap_or(0) == 0 {
                    replies.extend_from_slice(b"\x1b[>1;10;0c");
                }
                return;
            }
            "=" => {
                // Tertiary Device Attributes (`CSI = c`) or Kitty keyboard `CSI = flags u` (no cursor jump!)
                if final_ch == 'c' && params.first().copied().unwrap_or(0) == 0 {
                    replies.extend_from_slice(b"\x1bP!|00000000\x1b\\");
                }
                return;
            }
            "<" | "!" => {
                // Kitty keyboard pop (`CSI < u`) or soft reset (`CSI ! p`): consume cleanly
                if prefix == "!" && final_ch == 'p' {
                    self.style = CellStyle::default();
                    self.app_cursor = false;
                    self.cursor_visible = true;
                    self.pending_wrap = false;
                }
                return;
            }
            "" => {}
            _ => return,
        }

        // 3. Standard unprefixed CSI sequences
        match final_ch {
            'c' => {
                // Primary Device Attributes (DA1: `CSI c` / `CSI 0 c`) — critical for `fish` instant startup!
                if params.first().copied().unwrap_or(0) == 0 {
                    replies.extend_from_slice(b"\x1b[?62;22c");
                }
            }
            'n' => {
                // DSR (`CSI 5 n`) & CPR (`CSI 6 n`)
                match params.first().copied().unwrap_or(0) {
                    5 => replies.extend_from_slice(b"\x1b[0n"),
                    6 => {
                        let _ =
                            write!(replies, "\x1b[{};{}R", self.cursor_r + 1, self.cursor_c + 1);
                    }
                    _ => {}
                }
            }
            't' => {
                // XTWINOPS window size queries (`CSI 18 t`, `CSI 14 t`, `CSI 16 t`)
                match params.first().copied().unwrap_or(0) {
                    18 => {
                        let _ = write!(replies, "\x1b[8;{};{}t", self.rows, self.cols);
                    }
                    14 => {
                        let _ = write!(replies, "\x1b[4;{};{}t", self.rows * 19, self.cols * 9);
                    }
                    16 => {
                        replies.extend_from_slice(b"\x1b[6;19;9t");
                    }
                    _ => {}
                }
            }
            'A' => {
                self.pending_wrap = false;
                self.cursor_r = self.cursor_r.saturating_sub(p0(1));
            }
            'B' => {
                self.pending_wrap = false;
                self.cursor_r = (self.cursor_r + p0(1)).min(self.rows - 1);
            }
            'C' => {
                self.pending_wrap = false;
                self.cursor_c = (self.cursor_c + p0(1)).min(self.cols - 1);
            }
            'D' => {
                self.pending_wrap = false;
                self.cursor_c = self.cursor_c.saturating_sub(p0(1));
            }
            'E' => {
                self.pending_wrap = false;
                self.cursor_r = (self.cursor_r + p0(1)).min(self.rows - 1);
                self.cursor_c = 0;
            }
            'F' => {
                self.pending_wrap = false;
                self.cursor_r = self.cursor_r.saturating_sub(p0(1));
                self.cursor_c = 0;
            }
            'G' => {
                self.pending_wrap = false;
                self.cursor_c = p0(1).saturating_sub(1).min(self.cols - 1);
            }
            'd' => {
                self.pending_wrap = false;
                self.cursor_r = p0(1).saturating_sub(1).min(self.rows - 1);
            }
            'H' | 'f' => {
                self.pending_wrap = false;
                self.cursor_r = p0(1).saturating_sub(1).min(self.rows - 1);
                self.cursor_c = p1(1).saturating_sub(1).min(self.cols - 1);
            }
            'J' => {
                self.pending_wrap = false;
                let mode = params.first().copied().unwrap_or(0);
                let cr = self.cursor_r;
                let cc = self.cursor_c;
                let rows = self.rows;
                let cols = self.cols;
                if mode == 3 && !self.in_alt_screen {
                    self.scrollback.clear();
                }
                let grid = self.active_grid_mut();
                match mode {
                    0 => {
                        for cell in &mut grid[cr][cc..cols] {
                            *cell = VtCell::default();
                        }
                        for row in &mut grid[(cr + 1)..rows] {
                            row.fill(VtCell::default());
                        }
                    }
                    1 => {
                        for row in &mut grid[0..cr] {
                            row.fill(VtCell::default());
                        }
                        for cell in &mut grid[cr][0..=cc.min(cols - 1)] {
                            *cell = VtCell::default();
                        }
                    }
                    2 | 3 => {
                        for row in grid.iter_mut() {
                            row.fill(VtCell::default());
                        }
                    }
                    _ => {}
                }
            }
            'K' => {
                self.pending_wrap = false;
                let mode = params.first().copied().unwrap_or(0);
                let cr = self.cursor_r;
                let cc = self.cursor_c;
                let cols = self.cols;
                let row = &mut self.active_grid_mut()[cr];
                match mode {
                    0 => {
                        for cell in &mut row[cc..cols] {
                            *cell = VtCell::default();
                        }
                    }
                    1 => {
                        for cell in &mut row[0..=cc.min(cols - 1)] {
                            *cell = VtCell::default();
                        }
                    }
                    2 => {
                        row.fill(VtCell::default());
                    }
                    _ => {}
                }
            }
            'P' => {
                // Delete Characters (DCH)
                let n = p0(1);
                let cr = self.cursor_r;
                let cc = self.cursor_c;
                let cols = self.cols;
                let row = &mut self.active_grid_mut()[cr];
                let count = n.min(cols.saturating_sub(cc));
                for _ in 0..count {
                    row.remove(cc);
                    row.push(VtCell::default());
                }
            }
            '@' => {
                // Insert Blank Characters (ICH)
                let n = p0(1);
                let cr = self.cursor_r;
                let cc = self.cursor_c;
                let cols = self.cols;
                let row = &mut self.active_grid_mut()[cr];
                let count = n.min(cols.saturating_sub(cc));
                for _ in 0..count {
                    row.insert(cc, VtCell::default());
                    row.pop();
                }
            }
            'X' => {
                // Erase Characters (ECH)
                let n = p0(1);
                let cr = self.cursor_r;
                let cc = self.cursor_c;
                let end = (cc + n).min(self.cols);
                let row = &mut self.active_grid_mut()[cr];
                for cell in &mut row[cc..end] {
                    *cell = VtCell::default();
                }
            }
            'L' => {
                // Insert Lines (IL)
                let n = p0(1);
                let cr = self.cursor_r;
                if cr >= self.scroll_top && cr <= self.scroll_bottom {
                    let saved_top = self.scroll_top;
                    self.scroll_top = cr;
                    self.scroll_down(n);
                    self.scroll_top = saved_top;
                }
            }
            'M' => {
                // Delete Lines (DL)
                let n = p0(1);
                let cr = self.cursor_r;
                if cr >= self.scroll_top && cr <= self.scroll_bottom {
                    let saved_top = self.scroll_top;
                    self.scroll_top = cr;
                    self.scroll_up(n);
                    self.scroll_top = saved_top;
                }
            }
            'S' => {
                self.scroll_up(p0(1));
            }
            'T' => {
                self.scroll_down(p0(1));
            }
            'r' => {
                // Set Top and Bottom Margins (DECSTBM)
                let top = p0(1).saturating_sub(1).min(self.rows - 1);
                let bot = p1(self.rows).saturating_sub(1).min(self.rows - 1);
                if top < bot {
                    self.scroll_top = top;
                    self.scroll_bottom = bot;
                } else {
                    self.scroll_top = 0;
                    self.scroll_bottom = self.rows - 1;
                }
                self.cursor_r = 0;
                self.cursor_c = 0;
                self.pending_wrap = false;
            }
            's' => {
                if params.is_empty() {
                    self.saved_cursor = (self.cursor_r, self.cursor_c);
                }
            }
            'u' => {
                if params.is_empty() {
                    let (sr, sc) = self.saved_cursor;
                    self.cursor_r = sr.min(self.rows - 1);
                    self.cursor_c = sc.min(self.cols - 1);
                    self.pending_wrap = false;
                }
            }
            'm' => {
                self.apply_sgr_raw(params_raw);
            }
            _ => {}
        }
    }

    fn apply_sgr_raw(&mut self, params_raw: &str) {
        if params_raw.is_empty() {
            self.style = CellStyle::default();
            return;
        }
        // Normalize both semicolon (`38;2;R;G;B`) and ISO-8613-6 colon (`38:2::R:G:B`, `4:3`) SGR tokens
        let mut flat: Vec<usize> = Vec::new();
        for seg in params_raw.split(';') {
            if let Some((head, rest)) = seg.split_once(':') {
                let h = head.parse::<usize>().unwrap_or(0);
                if h == 38 || h == 48 {
                    let subs: Vec<&str> = rest.split(':').collect();
                    let kind = subs
                        .first()
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    if kind == 5 && subs.len() >= 2 {
                        flat.push(h);
                        flat.push(5);
                        flat.push(subs[1].parse::<usize>().unwrap_or(0));
                        continue;
                    } else if kind == 2 && subs.len() >= 4 {
                        // Either `38:2:R:G:B` (4 subs) or `38:2:CS:R:G:B` (5+ subs)
                        let base = if subs.len() >= 5 { 2 } else { 1 };
                        flat.push(h);
                        flat.push(2);
                        flat.push(subs[base].parse::<usize>().unwrap_or(0));
                        flat.push(subs[base + 1].parse::<usize>().unwrap_or(0));
                        flat.push(subs[base + 2].parse::<usize>().unwrap_or(0));
                        continue;
                    }
                } else if h == 58 {
                    continue;
                }
                flat.push(h);
            } else {
                flat.push(seg.parse::<usize>().unwrap_or(0));
            }
        }
        self.apply_sgr(&flat);
    }

    fn apply_sgr(&mut self, params: &[usize]) {
        if params.is_empty() {
            self.style = CellStyle::default();
            return;
        }
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => self.style = CellStyle::default(),
                1 => self.style.bold = true,
                22 => self.style.bold = false,
                7 => self.style.reverse = true,
                27 => self.style.reverse = false,
                30..=37 => self.style.fg = ansi_16_color(params[i] - 30),
                39 => self.style.fg = 0,
                40..=47 => self.style.bg = ansi_16_color(params[i] - 40),
                49 => self.style.bg = 0,
                90..=97 => self.style.fg = ansi_16_color(params[i] - 90 + 8),
                100..=107 => self.style.bg = ansi_16_color(params[i] - 100 + 8),
                38 | 48 => {
                    let is_fg = params[i] == 38;
                    if i + 2 < params.len() && params[i + 1] == 5 {
                        let col = xterm_256_color(params[i + 2]);
                        if is_fg {
                            self.style.fg = col;
                        } else {
                            self.style.bg = col;
                        }
                        i += 2;
                    } else if i + 4 < params.len() && params[i + 1] == 2 {
                        let r = params[i + 2].min(255) as u8;
                        let g = params[i + 3].min(255) as u8;
                        let b = params[i + 4].min(255) as u8;
                        let col = pack_rgba(r, g, b, 255);
                        if is_fg {
                            self.style.fg = col;
                        } else {
                            self.style.bg = col;
                        }
                        i += 4;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn grid_col_to_char_col(cells: &[VtCell], grid_col: usize) -> i64 {
        let limit = grid_col.min(cells.len());
        let mut char_col = 0i64;
        for cell in cells.iter().take(limit) {
            if cell.ch != '\0' {
                char_col += 1;
            }
        }
        char_col
    }

    fn extract_row(cells: &[VtCell], target_row: i64) -> (String, Vec<TermColorSpan>) {
        let mut last_non_blank = 0usize;
        for (idx, cell) in cells.iter().enumerate() {
            if (cell.ch != ' ' && cell.ch != '\0') || cell.bg != 0 {
                last_non_blank = idx + 1;
            }
        }
        let mut text = String::with_capacity(last_non_blank);
        let mut spans: Vec<TermColorSpan> = Vec::new();
        let mut char_col = 0i64;

        for cell in cells.iter().take(last_non_blank) {
            if cell.ch == '\0' {
                continue;
            }
            text.push(cell.ch);
            if cell.fg != 0 || cell.bg != 0 || cell.bold {
                if let Some(last) = spans.last_mut()
                    && last.end_col == char_col
                    && last.fg_rgba == cell.fg
                    && last.bg_rgba == cell.bg
                    && last.bold == cell.bold
                {
                    last.end_col = char_col + 1;
                } else {
                    spans.push(TermColorSpan {
                        row: target_row,
                        start_col: char_col,
                        end_col: char_col + 1,
                        fg_rgba: cell.fg,
                        bg_rgba: cell.bg,
                        bold: cell.bold,
                    });
                }
            }
            char_col += 1;
        }
        (text, spans)
    }

    pub fn snapshot(
        &self,
        include_scrollback: bool,
        fallback_cwd: &str,
        alive: bool,
        exit_code: i64,
    ) -> TermSnapshot {
        let sb_count = if include_scrollback && !self.in_alt_screen {
            self.scrollback.len()
        } else {
            0
        };

        let mut lines = Vec::with_capacity(sb_count + self.rows);
        let mut spans = Vec::new();

        if sb_count > 0 {
            for (idx, sb) in self.scrollback.iter().enumerate() {
                let r = idx as i64;
                lines.push(sb.text.clone());
                for sp in &sb.spans {
                    spans.push(TermColorSpan {
                        row: r,
                        start_col: sp.start_col,
                        end_col: sp.end_col,
                        fg_rgba: sp.fg_rgba,
                        bg_rgba: sp.bg_rgba,
                        bold: sp.bold,
                    });
                }
            }
        }

        let grid = self.active_grid();
        for (r_idx, row_cells) in grid.iter().enumerate() {
            let abs_row = (sb_count + r_idx) as i64;
            let (txt, row_spans) = Self::extract_row(row_cells, abs_row);
            lines.push(txt);
            spans.extend(row_spans);
        }

        // Trim trailing completely blank screen rows below the cursor when rendering scrollback
        // buffers, while always keeping the cursor row valid.
        let cursor_abs_row = (sb_count + self.cursor_r) as i64;
        while lines.len() > (cursor_abs_row as usize + 1)
            && lines.last().is_some_and(|s| s.is_empty())
        {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }

        let cursor_char_col = if self.cursor_r < grid.len() {
            Self::grid_col_to_char_col(&grid[self.cursor_r], self.cursor_c)
        } else {
            self.cursor_c as i64
        };

        TermSnapshot {
            lines,
            spans,
            cursor_row: cursor_abs_row,
            cursor_col: cursor_char_col,
            cursor_visible: self.cursor_visible,
            alt_screen: self.in_alt_screen,
            app_cursor: self.app_cursor,
            cwd: self
                .osc_cwd
                .clone()
                .unwrap_or_else(|| fallback_cwd.to_string()),
            title: self.title.clone(),
            alive,
            exit_code,
            rows: self.rows,
            cols: self.cols,
        }
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex_str) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(val) = u8::from_str_radix(hex_str, 16)
        {
            out.push(val);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Crisp modern Catppuccin/xterm 16-color ANSI palette.
fn ansi_16_color(idx: usize) -> i64 {
    match idx & 15 {
        0 => pack_rgba(69, 71, 90, 255),    // Black
        1 => pack_rgba(243, 139, 168, 255), // Red
        2 => pack_rgba(166, 227, 161, 255), // Green
        3 => pack_rgba(249, 226, 175, 255), // Yellow
        4 => pack_rgba(137, 180, 250, 255), // Blue
        5 => pack_rgba(245, 194, 231, 255), // Magenta
        6 => pack_rgba(148, 226, 213, 255), // Cyan
        7 => pack_rgba(186, 194, 222, 255), // White
        8 => pack_rgba(108, 112, 134, 255), // Bright Black
        9 => pack_rgba(243, 139, 168, 255), // Bright Red
        10 => pack_rgba(166, 227, 161, 255),
        11 => pack_rgba(249, 226, 175, 255),
        12 => pack_rgba(137, 180, 250, 255),
        13 => pack_rgba(203, 166, 247, 255),
        14 => pack_rgba(137, 220, 235, 255),
        _ => pack_rgba(245, 224, 220, 255),
    }
}

fn xterm_256_color(idx: usize) -> i64 {
    if idx < 16 {
        return ansi_16_color(idx);
    }
    if idx < 232 {
        let n = idx - 16;
        let r_idx = n / 36;
        let g_idx = (n / 6) % 6;
        let b_idx = n % 6;
        let level = |v: usize| if v == 0 { 0u8 } else { (55 + v * 40) as u8 };
        return pack_rgba(level(r_idx), level(g_idx), level(b_idx), 255);
    }
    if idx < 256 {
        let gray = (8 + (idx - 232) * 10).min(255) as u8;
        return pack_rgba(gray, gray, gray, 255);
    }
    0
}

/// Encode a canonical Lava key event into VT100/xterm terminal input bytes.
pub fn encode_key_input(
    key: &str,
    text: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    app_cursor: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    if alt {
        out.push(0x1b);
    }

    if ctrl {
        if key == "Space" || key == "@" {
            out.push(0x00);
            return out;
        }
        if key.len() == 1 {
            let b = key.as_bytes()[0];
            if b.is_ascii_alphabetic() {
                out.push(b.to_ascii_lowercase() - b'a' + 1);
                return out;
            }
            match b {
                b'[' => {
                    out.push(0x1b);
                    return out;
                }
                b'\\' => {
                    out.push(0x1c);
                    return out;
                }
                b']' => {
                    out.push(0x1d);
                    return out;
                }
                b'^' => {
                    out.push(0x1e);
                    return out;
                }
                b'_' | b'/' => {
                    out.push(0x1f);
                    return out;
                }
                _ => {}
            }
        }
    }

    let ss3_or_csi = if app_cursor { b'O' } else { b'[' };
    match key {
        "Return" | "KP_Enter" => out.push(b'\r'),
        "BackSpace" => out.push(0x7f),
        "Tab" => {
            if shift {
                out.extend_from_slice(b"\x1b[Z");
            } else {
                out.push(b'\t');
            }
        }
        "BackTab" | "ISO_Left_Tab" => out.extend_from_slice(b"\x1b[Z"),
        "Escape" => out.push(0x1b),
        "Space" => out.push(b' '),
        "Up" => out.extend_from_slice(&[0x1b, ss3_or_csi, b'A']),
        "Down" => out.extend_from_slice(&[0x1b, ss3_or_csi, b'B']),
        "Right" => out.extend_from_slice(&[0x1b, ss3_or_csi, b'C']),
        "Left" => out.extend_from_slice(&[0x1b, ss3_or_csi, b'D']),
        "Home" => out.extend_from_slice(&[0x1b, ss3_or_csi, b'H']),
        "End" => out.extend_from_slice(&[0x1b, ss3_or_csi, b'F']),
        "Delete" => out.extend_from_slice(b"\x1b[3~"),
        "Insert" => out.extend_from_slice(b"\x1b[2~"),
        "Page_Up" | "Prior" => out.extend_from_slice(b"\x1b[5~"),
        "Page_Down" | "Next" => out.extend_from_slice(b"\x1b[6~"),
        "F1" => out.extend_from_slice(b"\x1bOP"),
        "F2" => out.extend_from_slice(b"\x1bOQ"),
        "F3" => out.extend_from_slice(b"\x1bOR"),
        "F4" => out.extend_from_slice(b"\x1bOS"),
        _ => {
            if !text.is_empty() {
                out.extend_from_slice(text.as_bytes());
            } else if key.chars().count() == 1 {
                out.extend_from_slice(key.as_bytes());
            }
        }
    }
    out
}

#[cfg(unix)]
struct PtyInstance {
    writer: File,
    master_fd: libc::c_int,
    child: Child,
    child_pid: u32,
    initial_cwd: String,
    cached_cwd: String,
    last_cwd_gen: u64,
    output_gen: Arc<AtomicU64>,
    screen: Arc<Mutex<VtScreen>>,
    eof_reached: Arc<AtomicBool>,
    cached_exit_code: Option<i32>,
}

#[cfg(unix)]
impl PtyInstance {
    fn current_cwd(&mut self, screen_osc: &str) -> String {
        if !screen_osc.is_empty() {
            if self.cached_cwd != screen_osc {
                self.cached_cwd = screen_osc.to_string();
            }
            return self.cached_cwd.clone();
        }
        let cur_gen = self.output_gen.load(Ordering::Acquire);
        if cur_gen != self.last_cwd_gen || self.cached_cwd.is_empty() {
            self.last_cwd_gen = cur_gen;
            let proc_link = format!("/proc/{}/cwd", self.child_pid);
            if let Ok(target) = fs::read_link(&proc_link) {
                let s = target.to_string_lossy().into_owned();
                if !s.is_empty() {
                    self.cached_cwd = s;
                    return self.cached_cwd.clone();
                }
            }
            if self.cached_cwd.is_empty() {
                self.cached_cwd = self.initial_cwd.clone();
            }
        }
        self.cached_cwd.clone()
    }

    fn poll_alive(&mut self) -> (bool, i64) {
        if let Some(code) = self.cached_exit_code {
            return (false, code as i64);
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            let code = status.code().unwrap_or(0);
            self.cached_exit_code = Some(code);
            return (false, code as i64);
        }
        if self.eof_reached.load(Ordering::Acquire) {
            let code = self.child.wait().ok().and_then(|s| s.code()).unwrap_or(0);
            self.cached_exit_code = Some(code);
            return (false, code as i64);
        }
        (true, 0)
    }
}

#[cfg(unix)]
impl Drop for PtyInstance {
    fn drop(&mut self) {
        if self.cached_exit_code.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Callback invoked from a background PTY reader thread when new output bytes arrive.
pub type PtyWakeFn = Arc<dyn Fn() + Send + Sync>;

/// Thread-safe manager owning all live POSIX PTY sessions for a Lava VM instance.
#[cfg(unix)]
pub struct PtyManager {
    next_id: AtomicU64,
    sessions: Mutex<BTreeMap<u64, PtyInstance>>,
    dirty: Arc<AtomicBool>,
    waker: Mutex<Option<PtyWakeFn>>,
}

/// Stub PTY manager on Windows (Windows 10 1709 baseline predates ConPTY; terminal extension is disabled).
#[cfg(not(unix))]
pub struct PtyManager {
    dirty: AtomicBool,
}

impl Default for PtyManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(unix))]
impl PtyManager {
    pub fn new() -> Self {
        Self {
            dirty: AtomicBool::new(false),
        }
    }

    pub fn set_waker(&self, _waker: PtyWakeFn) {}

    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    pub fn has_active_sessions(&self) -> bool {
        false
    }

    pub fn spawn(
        &self,
        _shell: &str,
        _cwd: &str,
        _rows: usize,
        _cols: usize,
    ) -> Result<u64, String> {
        Err("PTY terminal is disabled on Windows (Windows 10 1709 lacks ConPTY)".to_string())
    }

    pub fn write_bytes(&self, id: u64, _data: &[u8]) -> Result<(), String> {
        Err(format!("unknown PTY session id {id}"))
    }

    pub fn send_key(
        &self,
        id: u64,
        _key: &str,
        _text: &str,
        _ctrl: bool,
        _alt: bool,
        _shift: bool,
    ) -> Result<(), String> {
        let _ = encode_key_input("", "", false, false, false, false);
        Err(format!("unknown PTY session id {id}"))
    }

    pub fn resize(&self, id: u64, _rows: usize, _cols: usize) -> Result<(), String> {
        Err(format!("unknown PTY session id {id}"))
    }

    pub fn snapshot(&self, id: u64, _include_scrollback: bool) -> Result<TermSnapshot, String> {
        Err(format!("unknown PTY session id {id}"))
    }

    pub fn kill(&self, _id: u64) -> bool {
        false
    }

    pub fn kill_all(&self) {}
}

#[cfg(unix)]
impl PtyManager {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(BTreeMap::new()),
            dirty: Arc::new(AtomicBool::new(false)),
            waker: Mutex::new(None),
        }
    }

    pub fn set_waker(&self, waker: PtyWakeFn) {
        let mut guard = self
            .waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(waker);
    }

    /// Atomically take and clear the dirty output flag.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    pub fn has_active_sessions(&self) -> bool {
        let mut guard = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.values_mut().any(|inst| inst.poll_alive().0)
    }

    pub fn spawn(&self, shell: &str, cwd: &str, rows: usize, cols: usize) -> Result<u64, String> {
        let r = rows.clamp(2, 300);
        let c = cols.clamp(4, 500);

        let (master_writer, reader_file, slave_file, master_raw_fd) = open_posix_pty(r, c)?;
        let reply_writer = master_writer
            .try_clone()
            .map_err(|e| format!("clone PTY reply writer failed: {e}"))?;

        let chosen_shell = resolve_shell_program(shell);
        let resolved_cwd = if !cwd.trim().is_empty() && Path::new(cwd.trim()).is_dir() {
            cwd.trim().to_string()
        } else {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".to_string())
        };

        let slave_in = slave_file
            .try_clone()
            .map_err(|e| format!("clone slave stdin failed: {e}"))?;
        let slave_out = slave_file
            .try_clone()
            .map_err(|e| format!("clone slave stdout failed: {e}"))?;
        let slave_err = slave_file;

        let mut cmd = Command::new(&chosen_shell);
        cmd.current_dir(&resolved_cwd)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .stdin(Stdio::from(slave_in))
            .stdout(Stdio::from(slave_out))
            .stderr(Stdio::from(slave_err));

        // SAFETY: `setsid` and `ioctl(0, TIOCSCTTY, 0)` are async-signal-safe POSIX syscalls
        // establishing the child process as session leader with fd 0 (the PTY slave) as its
        // controlling terminal before `exec`.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                let _ = libc::ioctl(0, libc::TIOCSCTTY, 0);
                Ok(())
            });
        }

        let child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn PTY shell '{chosen_shell}': {e}"))?;
        let child_pid = child.id();

        let screen = Arc::new(Mutex::new(VtScreen::new(r, c, 2000)));
        let eof_reached = Arc::new(AtomicBool::new(false));
        let output_gen = Arc::new(AtomicU64::new(1));

        let reader_screen = Arc::clone(&screen);
        let reader_eof = Arc::clone(&eof_reached);
        let reader_gen = Arc::clone(&output_gen);
        let reader_dirty = Arc::clone(&self.dirty);
        let waker_opt = self
            .waker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        thread::Builder::new()
            .name(format!("lava-pty-{child_pid}"))
            .spawn(move || {
                let mut reader = reader_file;
                let mut writer = reply_writer;
                let mut buf = [0u8; 8192];
                let mut reply_buf: Vec<u8> = Vec::with_capacity(128);
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            reader_eof.store(true, Ordering::Release);
                            reader_gen.fetch_add(1, Ordering::Release);
                            if !reader_dirty.swap(true, Ordering::AcqRel)
                                && let Some(ref wake) = waker_opt
                            {
                                wake();
                            }
                            break;
                        }
                        Ok(n) => {
                            reply_buf.clear();
                            {
                                let mut guard = reader_screen
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                guard.feed_bytes_with_replies(&buf[..n], &mut reply_buf);
                            }
                            if !reply_buf.is_empty() {
                                let _ = writer.write_all(&reply_buf);
                                let _ = writer.flush();
                            }
                            reader_gen.fetch_add(1, Ordering::Release);
                            if !reader_dirty.swap(true, Ordering::AcqRel)
                                && let Some(ref wake) = waker_opt
                            {
                                wake();
                            }
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            reader_eof.store(true, Ordering::Release);
                            reader_gen.fetch_add(1, Ordering::Release);
                            if !reader_dirty.swap(true, Ordering::AcqRel)
                                && let Some(ref wake) = waker_opt
                            {
                                wake();
                            }
                            break;
                        }
                    }
                }
            })
            .map_err(|e| format!("failed to spawn PTY reader thread: {e}"))?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let inst = PtyInstance {
            writer: master_writer,
            master_fd: master_raw_fd,
            child,
            child_pid,
            cached_cwd: resolved_cwd.clone(),
            initial_cwd: resolved_cwd,
            last_cwd_gen: 0,
            output_gen,
            screen,
            eof_reached,
            cached_exit_code: None,
        };

        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, inst);

        Ok(id)
    }

    pub fn write_bytes(&self, id: u64, data: &[u8]) -> Result<(), String> {
        let mut guard = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(inst) = guard.get_mut(&id) else {
            return Err(format!("unknown PTY session id {id}"));
        };
        inst.writer
            .write_all(data)
            .and_then(|()| inst.writer.flush())
            .map_err(|e| format!("PTY write failed: {e}"))
    }

    pub fn send_key(
        &self,
        id: u64,
        key: &str,
        text: &str,
        ctrl: bool,
        alt: bool,
        shift: bool,
    ) -> Result<(), String> {
        let app_cursor = {
            let guard = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(inst) = guard.get(&id) else {
                return Err(format!("unknown PTY session id {id}"));
            };
            let scr = inst
                .screen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            scr.app_cursor
        };
        let encoded = encode_key_input(key, text, ctrl, alt, shift, app_cursor);
        if encoded.is_empty() {
            return Ok(());
        }
        self.write_bytes(id, &encoded)
    }

    pub fn resize(&self, id: u64, rows: usize, cols: usize) -> Result<(), String> {
        let r = rows.clamp(2, 300);
        let c = cols.clamp(4, 500);
        let guard = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(inst) = guard.get(&id) else {
            return Err(format!("unknown PTY session id {id}"));
        };
        {
            let mut scr = inst
                .screen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            scr.resize(r, c);
        }
        set_pty_winsize(inst.master_fd, r, c);
        Ok(())
    }

    pub fn snapshot(&self, id: u64, include_scrollback: bool) -> Result<TermSnapshot, String> {
        // Clear dirty flag whenever Nessa pulls a snapshot so subsequent PTY output re-triggers wakeup.
        self.dirty.store(false, Ordering::Release);

        let mut guard = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(inst) = guard.get_mut(&id) else {
            return Err(format!("unknown PTY session id {id}"));
        };
        let (alive, exit_code) = inst.poll_alive();
        let screen_arc = Arc::clone(&inst.screen);
        let scr = screen_arc
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let osc_cwd = scr.osc_cwd.as_deref().unwrap_or("");
        let live_cwd = inst.current_cwd(osc_cwd);
        Ok(scr.snapshot(include_scrollback, &live_cwd, alive, exit_code))
    }

    pub fn kill(&self, id: u64) -> bool {
        let mut guard = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.remove(&id).is_some()
    }

    pub fn kill_all(&self) {
        let mut guard = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clear();
    }
}

#[cfg(unix)]
fn resolve_shell_program(explicit: &str) -> String {
    let trimmed = explicit.trim();
    if !trimmed.is_empty() && Path::new(trimmed).exists() {
        return trimmed.to_string();
    }
    if let Ok(env_shell) = std::env::var("SHELL")
        && !env_shell.trim().is_empty()
        && Path::new(env_shell.trim()).exists()
    {
        return env_shell.trim().to_string();
    }
    for candidate in ["/bin/bash", "/usr/bin/bash", "/bin/sh"] {
        if Path::new(candidate).exists() {
            return candidate.to_string();
        }
    }
    "/bin/sh".to_string()
}

#[cfg(unix)]
fn set_pty_winsize(fd: libc::c_int, rows: usize, cols: usize) {
    let ws = libc::winsize {
        ws_row: rows.min(u16::MAX as usize) as u16,
        ws_col: cols.min(u16::MAX as usize) as u16,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `fd` is an open PTY master file descriptor owned by `PtyInstance`, and `ws` is a
    // valid stack-allocated `libc::winsize` struct passed to `TIOCSWINSZ`.
    unsafe {
        let _ = libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
    }
}

#[cfg(unix)]
fn open_posix_pty(rows: usize, cols: usize) -> Result<(File, File, File, libc::c_int), String> {
    // SAFETY: `posix_openpt`, `grantpt`, `unlockpt`, `ptsname_r`, and `open` are standard
    // POSIX/Linux PTY setup calls; all returned descriptors are checked `< 0` before wrapping
    // into `std::fs::File` via `FromRawFd::from_raw_fd` with exclusive ownership per `File`.
    unsafe {
        let master_fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if master_fd < 0 {
            return Err(format!(
                "posix_openpt failed: {}",
                io::Error::last_os_error()
            ));
        }

        if libc::grantpt(master_fd) != 0 {
            let err = io::Error::last_os_error();
            libc::close(master_fd);
            return Err(format!("grantpt failed: {err}"));
        }
        if libc::unlockpt(master_fd) != 0 {
            let err = io::Error::last_os_error();
            libc::close(master_fd);
            return Err(format!("unlockpt failed: {err}"));
        }

        let mut name_buf = [0 as libc::c_char; 256];
        if libc::ptsname_r(master_fd, name_buf.as_mut_ptr(), name_buf.len()) != 0 {
            let err = io::Error::last_os_error();
            libc::close(master_fd);
            return Err(format!("ptsname_r failed: {err}"));
        }
        let slave_name = CStr::from_ptr(name_buf.as_ptr());

        set_pty_winsize(master_fd, rows, cols);

        let slave_fd = libc::open(slave_name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        if slave_fd < 0 {
            let err = io::Error::last_os_error();
            libc::close(master_fd);
            return Err(format!("open slave pty failed: {err}"));
        }

        let master_writer = File::from_raw_fd(master_fd);
        let master_reader = match master_writer.try_clone() {
            Ok(f) => f,
            Err(err) => {
                libc::close(slave_fd);
                return Err(format!("clone master pty failed: {err}"));
            }
        };
        let slave_file = File::from_raw_fd(slave_fd);
        Ok((master_writer, master_reader, slave_file, master_fd))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn vt_screen_parses_ansi_truecolor_cursor_overwrite_and_alt_screen() {
        let mut vt = VtScreen::new(10, 40, 100);
        vt.feed_bytes(b"\x1b[1;38;2;166;227;161mREADY\x1b[0m normal\r\n");
        vt.feed_bytes(b"progress 10%\rprogress 100%");

        let snap = vt.snapshot(true, "/workspace", true, 0);
        assert_eq!(snap.lines[0], "READY normal");
        assert_eq!(snap.lines[1], "progress 100%");
        assert_eq!(snap.spans.len(), 1);
        assert_eq!(snap.spans[0].row, 0);
        assert_eq!(snap.spans[0].start_col, 0);
        assert_eq!(snap.spans[0].end_col, 5);
        assert_eq!(snap.spans[0].fg_rgba, pack_rgba(166, 227, 161, 255));
        assert!(snap.spans[0].bold);

        // Enter alternate screen, write TUI content, then leave alternate screen
        vt.feed_bytes(b"\x1b[?1049h\x1b[1;1HTUI FULLSCREEN");
        let alt_snap = vt.snapshot(true, "/workspace", true, 0);
        assert!(alt_snap.alt_screen);
        assert_eq!(alt_snap.lines[0], "TUI FULLSCREEN");

        vt.feed_bytes(b"\x1b[?1049l");
        let restored = vt.snapshot(true, "/workspace", true, 0);
        assert!(!restored.alt_screen);
        assert_eq!(restored.lines[0], "READY normal");
        assert_eq!(restored.lines[1], "progress 100%");
    }

    #[cfg(unix)]
    #[test]
    fn live_posix_pty_spawns_shell_streams_ansi_tracks_cwd_and_exits() {
        let mgr = PtyManager::new();
        let woke = Arc::new(AtomicBool::new(false));
        let woke_flag = Arc::clone(&woke);
        mgr.set_waker(Arc::new(move || {
            woke_flag.store(true, Ordering::Release);
        }));

        let id = mgr
            .spawn("/bin/sh", "/tmp", 24, 80)
            .expect("spawn /bin/sh on real Linux PTY");

        mgr.write_bytes(
            id,
            b"cd / && printf '\\033[32mPTY_LIVE_OK:%s\\033[0m\\n' \"$PWD\"\n",
        )
        .expect("write command to PTY");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut saw_marker = false;
        while Instant::now() < deadline {
            let snap = mgr.snapshot(id, true).expect("snapshot PTY");
            if snap.lines.iter().any(|l| l.contains("PTY_LIVE_OK:/")) {
                assert_eq!(snap.cwd, "/");
                assert!(!snap.spans.is_empty(), "expected green ANSI span");
                saw_marker = true;
                break;
            }
            thread::sleep(Duration::from_millis(15));
        }
        assert!(saw_marker, "expected PTY_LIVE_OK:/ in live PTY output");
        assert!(woke.load(Ordering::Acquire), "expected PTY waker callback");

        mgr.send_key(id, "d", "", true, false, false)
            .expect("send Ctrl-D to exit /bin/sh");
        mgr.kill(id);
    }

    #[test]
    fn vt_screen_answers_da1_da2_kitty_osc_dcs_and_strips_screen_title_without_leak() {
        let mut vt = VtScreen::new(10, 40, 100);
        // Simulate fish startup probing sequence:
        //   - GNU Screen title (`ESC k fish ESC \`)
        //   - Kitty keyboard query (`CSI ? u`) + push/pop (`CSI > 1 u`, `CSI = 5 u`, `CSI < u`)
        //   - Primary/Secondary DA (`CSI c`, `CSI > c`), DSR/CPR (`CSI 5 n`, `CSI 6 n`)
        //   - XTVERSION (`DCS > | ESC \`) + XTGETTCAP (`DCS + q 544e ESC \`)
        //   - OSC 10/11 foreground/background queries (`OSC 10 ; ? BEL`, `OSC 11 ; ? ST`)
        //   - APC payload (`ESC _ Gi=1;OK ESC \`)
        //   - Wide CJK prompt + text (`你好 prompt> `)
        let probe = concat!(
            "\x1bkfish\x1b\\",
            "\x1b[?u",
            "\x1b[>1u\x1b[=5u\x1b[<u",
            "\x1b[c\x1b[>c\x1b[5n\x1b[6n",
            "\x1bP>|\x1b\\",
            "\x1bP+q544e\x1b\\",
            "\x1b]10;?\x07\x1b]11;?\x1b\\",
            "\x1b_Gi=1;OK\x1b\\",
            "你好 prompt> ",
        );
        let mut replies = Vec::new();
        vt.feed_bytes_with_replies(probe.as_bytes(), &mut replies);
        let reply_str = String::from_utf8(replies).expect("UTF-8 terminal replies");

        assert!(
            reply_str.contains("\x1b[?0u"),
            "must answer Kitty keyboard query"
        );
        assert!(reply_str.contains("\x1b[?62;22c"), "must answer DA1 query");
        assert!(reply_str.contains("\x1b[>1;10;0c"), "must answer DA2 query");
        assert!(reply_str.contains("\x1b[0n"), "must answer DSR 5n");
        assert!(reply_str.contains("\x1b[1;1R"), "must answer CPR 6n");
        assert!(reply_str.contains("lava-term"), "must answer XTVERSION");
        assert!(
            reply_str.contains("\x1bP0+r\x1b\\"),
            "must answer XTGETTCAP"
        );
        assert!(
            reply_str.contains("\x1b]10;rgb:"),
            "must answer OSC 10 fg color"
        );
        assert!(
            reply_str.contains("\x1b]11;rgb:"),
            "must answer OSC 11 bg color"
        );

        let snap = vt.snapshot(true, "/tmp", true, 0);
        assert_eq!(snap.title, "fish");
        assert_eq!(
            snap.lines[0], "你好 prompt>",
            "no stray 'k' or escape payload should leak onto row 0"
        );
        // Wide CJK: '你' (2 cells) + '好' (2 cells) + " prompt> " (9 cells) = grid col 13, char col 11
        assert_eq!(vt.cursor_c, 13);
        assert_eq!(snap.cursor_row, 0);
        assert_eq!(snap.cursor_col, 11);

        // Now move cursor to 1-based grid col 9 (0-based grid col 8 -> char col 6)
        // and send Kitty `<u` pop: cursor MUST NOT jump to (0, 0)!
        vt.feed_bytes(b"\x1b[1;9H\x1b[<u");
        assert_eq!(vt.cursor_c, 8);
        let after_kitty_pop = vt.snapshot(true, "/tmp", true, 0);
        assert_eq!(after_kitty_pop.cursor_row, 0);
        assert_eq!(after_kitty_pop.cursor_col, 6);
    }

    #[cfg(unix)]
    #[test]
    fn fish_or_sh_pty_starts_instantly_without_da1_warning_and_exits_on_ctrl_d() {
        let shell = if std::path::Path::new("/usr/bin/fish").exists() {
            "/usr/bin/fish"
        } else {
            "/bin/sh"
        };
        let mgr = PtyManager::new();
        let started = Instant::now();
        let id = mgr
            .spawn(shell, "/tmp", 24, 88)
            .expect("spawn interactive shell on PTY");

        mgr.write_bytes(id, b"echo LAVA_INSTANT_READY\n")
            .expect("write echo command");

        let deadline = Instant::now() + Duration::from_millis(2500);
        let mut ready = false;
        while Instant::now() < deadline {
            let snap = mgr.snapshot(id, true).expect("snapshot shell PTY");
            let joined = snap.lines.join("\n");
            assert!(
                !joined.contains("could not read response to Primary Device Attribute"),
                "fish emitted DA1 timeout warning: {joined}"
            );
            if snap.lines.iter().any(|l| l.trim() == "LAVA_INSTANT_READY") {
                ready = true;
                break;
            }
            thread::sleep(Duration::from_millis(15));
        }
        assert!(
            ready,
            "{shell} did not become ready within 2.5s (elapsed {:?})",
            started.elapsed()
        );

        // Send Ctrl-D on an empty prompt line -> shell must exit cleanly via EOF!
        mgr.send_key(id, "d", "", true, false, false)
            .expect("send Ctrl-D EOF");

        let exit_deadline = Instant::now() + Duration::from_millis(2000);
        let mut exited = false;
        while Instant::now() < exit_deadline {
            let snap = mgr.snapshot(id, true).expect("snapshot after Ctrl-D");
            if !snap.alive {
                exited = true;
                break;
            }
            thread::sleep(Duration::from_millis(15));
        }
        assert!(exited, "{shell} should exit cleanly after Ctrl-D EOF");
        mgr.kill(id);
    }
}
