//! cce-terminal — terminal emulator for the cce desktop.
//!
//! VT emulation is `alacritty_terminal`'s `Term` fed by its `vte` parser; this
//! crate owns the pty ([`pty`]), the palette mapping ([`colors`]), and the
//! rendering onto cce-ui's display-list path: cell backgrounds and decorations
//! as quads, foreground text batched into per-style runs, everything on the
//! DE's standard window plate. Key input encodes to terminal bytes
//! (APP_CURSOR-aware); the wheel scrolls the scrollback (or synthesizes
//! arrows on the alternate screen); OSC titles flow to the xdg toplevel via
//! the engine's `settings().title` poll.
//!
//! Selection follows the X/Wayland convention: click-drag (double = word,
//! triple = line) highlights and copies to PRIMARY on release; middle-click
//! pastes PRIMARY; Ctrl+Shift+C / Ctrl+Shift+V are the regular clipboard,
//! with bracketed paste when the app enables it. OSC 52 stores are honored.
//!
//! Mouse reporting: TUIs that enable a mouse mode (1000/1002/1003, with SGR
//! 1006 / UTF-8 1005 / legacy encodings) get button, drag-motion, and wheel
//! reports at cell coordinates, plus focus in/out (1004); holding Shift
//! bypasses reporting so selection stays reachable, per convention.
//!
//! Not yet: measured cell metrics (0.60 em / 1.2 em estimates — exact for
//! Berkeley Mono in practice), KDL config for font size and palette.

mod clip;
mod colors;
mod pty;

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{ClipboardType, Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb,
};
use cce_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{DisplayList, PaintCtx, TextAttrs};
use cce_ui::widget::{ElementState, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey};
use wayland_client::QueueHandle;

const FONT_SIZE: f32 = 14.0;
/// Toolkit conventions: ceil(font_size × 1.2) line height (`text_leaf_height`),
/// 0.60 × font_size mono advance (`estimate_label_width_helper`).
const LINE_H: f32 = 17.0;
const CELL_W: f32 = FONT_SIZE * 0.60;
const INIT_W: u32 = 840;
const INIT_H: u32 = 520;
const SCROLLBACK: usize = 5000;
/// Wheel notches → scrollback lines.
const SCROLL_LINES_PER_NOTCH: f32 = 3.0;
/// Presses in the same cell within this window escalate Simple → Semantic →
/// Lines selection.
const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(400);
/// Selection highlight, over cell backgrounds and under glyphs.
const SELECTION_RGB: Rgb = Rgb { r: 0xc8, g: 0xd0, b: 0xe0 };

#[derive(Clone)]
enum Msg {
    Pty(Vec<u8>),
    PtyClosed,
    Term(TermEvent),
}

/// The terminal's grid dimensions, for `Term::new`/`resize`.
#[derive(Clone, Copy)]
struct TermDims {
    cols: u16,
    rows: u16,
}

impl Dimensions for TermDims {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// Forwards `Term`'s synthesized events (pty write-backs, title changes …)
/// into the engine's message channel; they are handled in `update`.
struct EventProxy(calloop::channel::Sender<Msg>);

impl EventListener for EventProxy {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.send(Msg::Term(event));
    }
}

struct TerminalApp {
    term: Term<EventProxy>,
    parser: Processor,
    pty: pty::Pty,
    writer: std::fs::File,
    font: String,
    pad: f32,
    cols: u16,
    rows: u16,
    /// OSC title; polled by the engine through `settings().title`.
    title: Option<String>,
    /// Fractional wheel-scroll remainder (trackpad pixel deltas).
    scroll_accum: f32,
    focused: bool,
    /// Left button held: pointer moves extend the selection.
    selecting: bool,
    /// Multi-click escalation state: last press instant + cell.
    last_click: Option<(Instant, Point)>,
    click_count: u8,
    /// Current window size (for selection edge-autoscroll bounds).
    win_h: f32,
    /// Modifier state tracked from key events (mouse handlers receive no
    /// modifiers). Shift bypasses mouse reporting; ctrl/alt ride report codes.
    shift_down: bool,
    ctrl_down: bool,
    alt_down: bool,
    /// Held buttons for drag-motion reports: bit 0 left, 1 middle, 2 right.
    mouse_buttons: u8,
    /// Cell of the last motion report — motion is per-cell, not per-pixel.
    last_mouse_cell: Option<(usize, usize)>,
}

impl TerminalApp {
    fn grid_for(&self, w: f32, h: f32) -> (u16, u16) {
        let cols = (((w - 2.0 * self.pad) / CELL_W).floor() as i64).clamp(2, u16::MAX as i64);
        let rows = (((h - 2.0 * self.pad) / LINE_H).floor() as i64).clamp(1, u16::MAX as i64);
        (cols as u16, rows as u16)
    }

    fn write_pty(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
    }

    fn cell_rect(&self, row: usize, col: usize, width_cells: usize) -> Rect {
        Rect {
            x: self.pad + col as f32 * CELL_W,
            y: self.pad + row as f32 * LINE_H,
            width: width_cells as f32 * CELL_W,
            height: LINE_H,
        }
    }

    /// Pixel position → 0-based viewport cell (clamped).
    fn viewport_cell(&self, pos: LogicalPosition) -> (usize, usize) {
        let col = (((pos.x as f32 - self.pad) / CELL_W).max(0.0) as usize)
            .min(self.cols as usize - 1);
        let row = (((pos.y as f32 - self.pad) / LINE_H).max(0.0) as usize)
            .min(self.rows as usize - 1);
        (col, row)
    }

    /// Pixel position → grid point (viewport-clamped; grid-space line, so
    /// scrolled history resolves to negative lines) plus which half of the
    /// cell was hit.
    fn grid_point(&self, pos: LogicalPosition) -> (Point, Side) {
        let (col, row) = self.viewport_cell(pos);
        let col_f = ((pos.x as f32 - self.pad) / CELL_W).max(0.0);
        let line = Line(row as i32 - self.term.grid().display_offset() as i32);
        let side = if col_f.fract() > 0.5 { Side::Right } else { Side::Left };
        (Point::new(line, Column(col)), side)
    }

    /// Whether pointer events currently belong to the application rather than
    /// the selection machinery (Shift bypasses, per convention).
    fn mouse_reporting(&self) -> bool {
        self.term.mode().intersects(TermMode::MOUSE_MODE) && !self.shift_down
    }

    /// Modifier bits added to every report's button code. Shift never
    /// arrives here (it bypasses reporting).
    fn report_mods(&self) -> u8 {
        (self.alt_down as u8) * 8 + (self.ctrl_down as u8) * 16
    }

    fn send_mouse_report(&mut self, code: u8, pressed: bool, col: usize, row: usize) {
        if let Some(bytes) = mouse_report_bytes(*self.term.mode(), code, pressed, col, row) {
            self.write_pty(&bytes);
        }
    }

    /// Send pasted text to the pty and snap the view to the bottom.
    fn paste(&mut self, text: &str) {
        let bracketed = self.term.mode().contains(TermMode::BRACKETED_PASTE);
        let bytes = paste_bytes(text, bracketed);
        self.write_pty(&bytes);
        if self.term.grid().display_offset() != 0 {
            self.term.scroll_display(Scroll::Bottom);
        }
    }
}

/// Encode one mouse report. `code` is the xterm button code with modifier
/// bits already applied (0/1/2 buttons, 64/65 wheel, +32 for motion);
/// `col`/`row` are 0-based viewport cells. Picks the encoding the app
/// negotiated: SGR (1006) > UTF-8 extended coords (1005) > legacy X10 bytes
/// (coordinates saturate at their encodable maximum).
fn mouse_report_bytes(
    mode: TermMode,
    code: u8,
    pressed: bool,
    col: usize,
    row: usize,
) -> Option<Vec<u8>> {
    if mode.contains(TermMode::SGR_MOUSE) {
        let suffix = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{};{};{}{}", code, col + 1, row + 1, suffix).into_bytes());
    }
    // Non-SGR encodings can't distinguish which button released.
    let byte = 32 + if pressed { code } else { 3 | (code & !0b11) };
    let mut bytes = vec![0x1b, b'[', b'M', byte];
    if mode.contains(TermMode::UTF8_MOUSE) {
        for coord in [col, row] {
            let n = (coord + 1 + 32).min(2015) as u32;
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(
                char::from_u32(n).unwrap_or(' ').encode_utf8(&mut buf).as_bytes(),
            );
        }
    } else {
        bytes.push((col + 1 + 32).min(255) as u8);
        bytes.push((row + 1 + 32).min(255) as u8);
    }
    Some(bytes)
}

/// Paste encoding: bracketed when the app asked for it (end-marker
/// occurrences stripped so a paste can't fake the terminator),
/// newline-normalized to CR otherwise.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend_from_slice(text.replace("\x1b[201~", "").as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
        bytes
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Terminal-space RGB → cce-ui quad color (the geometry pipeline is linear;
/// terminal colors are sRGB).
fn quad_color(rgb: Rgb, alpha: f32) -> [f32; 4] {
    [
        cce_ui::color::srgb_to_linear(rgb.r as f32 / 255.0),
        cce_ui::color::srgb_to_linear(rgb.g as f32 / 255.0),
        cce_ui::color::srgb_to_linear(rgb.b as f32 / 255.0),
        alpha,
    ]
}

/// Terminal byte encoding for a key press; `None` = nothing to send. Honors
/// DECCKM (application cursor keys) and the xterm modifier parameter
/// (`1 + shift + 2·alt + 4·ctrl`) on CSI keys; alt prefixes ESC everywhere
/// else (readline's alt-b / alt-backspace family).
fn encode_key(ev: &KeyEvent, mode: TermMode) -> Option<Vec<u8>> {
    let app = mode.contains(TermMode::APP_CURSOR);
    let m = 1 + ev.shift as u8 + 2 * ev.alt as u8 + 4 * ev.ctrl as u8;

    enum Enc {
        /// `ESC[<final>` / `ESCO<final>` (app mode) / `ESC[1;<m><final>`.
        Csi(char),
        /// `ESC[<n>~` / `ESC[<n>;<m>~`.
        Tilde(u8),
        /// Raw bytes, ESC-prefixed when alt is held.
        Plain(&'static [u8]),
    }

    match &ev.logical_key {
        Key::Named(k) => {
            let enc = match k {
                NamedKey::ArrowUp => Enc::Csi('A'),
                NamedKey::ArrowDown => Enc::Csi('B'),
                NamedKey::ArrowRight => Enc::Csi('C'),
                NamedKey::ArrowLeft => Enc::Csi('D'),
                NamedKey::Home => Enc::Csi('H'),
                NamedKey::End => Enc::Csi('F'),
                NamedKey::PageUp => Enc::Tilde(5),
                NamedKey::PageDown => Enc::Tilde(6),
                NamedKey::Delete => Enc::Tilde(3),
                NamedKey::F5 => Enc::Tilde(15),
                NamedKey::Enter => Enc::Plain(b"\r"),
                NamedKey::Backspace => Enc::Plain(b"\x7f"),
                NamedKey::Tab if ev.shift => return Some(b"\x1b[Z".to_vec()),
                NamedKey::Tab => Enc::Plain(b"\t"),
                NamedKey::Escape => Enc::Plain(b"\x1b"),
                NamedKey::Space if ev.ctrl => Enc::Plain(b"\x00"),
                NamedKey::Space => Enc::Plain(b" "),
                _ => return None,
            };
            Some(match enc {
                Enc::Csi(c) if m > 1 => format!("\x1b[1;{m}{c}").into_bytes(),
                Enc::Csi(c) if app => format!("\x1bO{c}").into_bytes(),
                Enc::Csi(c) => format!("\x1b[{c}").into_bytes(),
                Enc::Tilde(n) if m > 1 => format!("\x1b[{n};{m}~").into_bytes(),
                Enc::Tilde(n) => format!("\x1b[{n}~").into_bytes(),
                Enc::Plain(b) => {
                    let mut bytes = Vec::with_capacity(b.len() + 1);
                    if ev.alt {
                        bytes.push(0x1b);
                    }
                    bytes.extend_from_slice(b);
                    bytes
                }
            })
        }
        Key::Character(s) => {
            let mut bytes: Vec<u8> = if ev.ctrl {
                match s.chars().next()?.to_ascii_lowercase() {
                    c @ 'a'..='z' => vec![c as u8 - b'a' + 1],
                    '[' => vec![0x1b],
                    '\\' => vec![0x1c],
                    ']' => vec![0x1d],
                    _ => return None,
                }
            } else if let Some(t) = &ev.text {
                t.as_bytes().to_vec()
            } else {
                s.as_bytes().to_vec()
            };
            if ev.alt {
                bytes.insert(0, 0x1b);
            }
            Some(bytes)
        }
    }
}

/// A run of contiguous same-style cells on one row, batched into a single
/// text prim (valid because the grid is monospace-cell-addressed).
struct TextRun {
    row: usize,
    col: usize,
    next_col: usize,
    text: String,
    fg: Rgb,
    bold: bool,
    italic: bool,
}

impl Application for TerminalApp {
    type Message = Msg;

    fn new(
        _qh: &QueueHandle<EngineState<Self>>,
        sender: calloop::channel::Sender<Self::Message>,
    ) -> Self {
        let pad = cce_ui::layout::backplate_padding();
        let cols = (((INIT_W as f32 - 2.0 * pad) / CELL_W) as i64).clamp(2, u16::MAX as i64) as u16;
        let rows = (((INIT_H as f32 - 2.0 * pad) / LINE_H) as i64).clamp(1, u16::MAX as i64) as u16;

        // `cce-terminal -e <cmd> [args…]` runs a command instead of $SHELL.
        let args: Vec<String> = std::env::args().collect();
        let command: Option<Vec<String>> = args
            .iter()
            .position(|a| a == "-e")
            .map(|i| args[i + 1..].to_vec())
            .filter(|c| !c.is_empty());

        let pty = pty::spawn_shell(cols, rows, command.as_deref())
            .expect("cce-terminal: failed to spawn shell on pty");
        let writer = pty.dup_handle().expect("cce-terminal: pty dup failed");
        let mut reader = pty.dup_handle().expect("cce-terminal: pty dup failed");
        let pty_sender = sender.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if pty_sender.send(Msg::Pty(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // EIO when the last slave fd closes: the normal exit path.
                    Err(_) => break,
                }
            }
            let _ = pty_sender.send(Msg::PtyClosed);
        });

        let config = TermConfig { scrolling_history: SCROLLBACK, ..TermConfig::default() };
        let term = Term::new(config, &TermDims { cols, rows }, EventProxy(sender));

        // Index 6 of the preferred-fonts tuple is the `terminal` alias
        // (~/.config/fontconfig/fonts.conf), falling back to Noto Sans Mono.
        let font = cce_ui::layout::read_preferred_fonts().6;

        TerminalApp {
            term,
            parser: Processor::new(),
            pty,
            writer,
            font,
            pad,
            cols,
            rows,
            title: None,
            scroll_accum: 0.0,
            focused: true,
            selecting: false,
            last_click: None,
            click_count: 0,
            win_h: INIT_H as f32,
            shift_down: false,
            ctrl_down: false,
            alt_down: false,
            mouse_buttons: 0,
            last_mouse_cell: None,
        }
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: self.title.clone().unwrap_or_else(|| "cce-terminal".to_string()),
            app_id: "cce-terminal".to_string(),
            width: INIT_W,
            height: INIT_H,
            fullscreen: false,
            min_size: Some((240, 140)),
        }
    }

    fn update(&mut self, msg: Self::Message, needs_rebuild: &mut bool, exit: &mut bool) {
        match msg {
            Msg::Pty(bytes) => {
                self.parser.advance(&mut self.term, &bytes);
                *needs_rebuild = true;
            }
            Msg::PtyClosed => {
                let _ = self.pty.child.wait();
                *exit = true;
            }
            Msg::Term(event) => match event {
                TermEvent::PtyWrite(s) => self.write_pty(s.clone().as_bytes()),
                TermEvent::Title(t) => {
                    self.title = Some(t);
                    *needs_rebuild = true;
                }
                TermEvent::ResetTitle => {
                    self.title = None;
                    *needs_rebuild = true;
                }
                TermEvent::ColorRequest(index, format) => {
                    let rgb = self
                        .term
                        .colors()[index]
                        .unwrap_or_else(|| colors::default_color(index));
                    let response = format(rgb);
                    self.write_pty(response.as_bytes());
                }
                TermEvent::TextAreaSizeRequest(format) => {
                    let response = format(WindowSize {
                        num_lines: self.rows,
                        num_cols: self.cols,
                        cell_width: CELL_W as u16,
                        cell_height: LINE_H as u16,
                    });
                    self.write_pty(response.as_bytes());
                }
                // OSC 52: programs storing to (or, if enabled in the term
                // config, reading from) the system clipboards.
                TermEvent::ClipboardStore(ty, text) => match ty {
                    ClipboardType::Clipboard => cce_ui::widget::clipboard::copy_to_clipboard(&text),
                    ClipboardType::Selection => clip::copy_primary(&text),
                },
                TermEvent::ClipboardLoad(ty, format) => {
                    let text = match ty {
                        ClipboardType::Clipboard => {
                            cce_ui::widget::clipboard::read_from_clipboard()
                        }
                        ClipboardType::Selection => clip::paste_primary(),
                    }
                    .unwrap_or_default();
                    let response = format(&text);
                    self.write_pty(response.as_bytes());
                }
                // Bell/Wakeup/cursor-blink: nothing to do yet.
                _ => {}
            },
        }
    }

    fn tick(&mut self, _dt: f32, _needs_rebuild: &mut bool) {}

    fn handle_resize(&mut self, width: f32, height: f32, _scale: f64) {
        self.win_h = height;
        let (cols, rows) = self.grid_for(width, height);
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            self.pty.resize(cols, rows);
            self.term.resize(TermDims { cols, rows });
        }
    }

    fn display_list(&mut self, size: LogicalSize, _scale: f64) -> Option<DisplayList> {
        let (w, h) = (size.width, size.height);
        let mut pc = PaintCtx::new();

        // The window plate, per the DE convention.
        let mut plate = cce_ui::color::page_low_color();
        if plate[3] > 0.001 {
            plate[3] = cce_ui::color::active_backplate_opacity();
        }
        let radius = cce_ui::colors::backplate_corner_radius();
        let frame = Rect { x: 0.0, y: 0.0, width: w, height: h };
        pc.plate(frame, (radius, radius, radius, radius), plate, cce_ui::layout::bevel_width());

        let rows = self.rows as usize;
        let pad = self.pad;
        let font = self.font.clone();
        let bounds = [pad, 0.0, w - pad, h];

        let content = self.term.renderable_content();
        let display_offset = content.display_offset as i32;
        let palette = content.colors;

        // One ordered pass over the viewport cells, batching backgrounds and
        // same-style text into runs. Emission order: bg quads → text →
        // decorations → cursor.
        let mut bg_runs: Vec<(usize, usize, usize, Rgb)> = Vec::new();
        let mut text_runs: Vec<TextRun> = Vec::new();
        // (row, col_start, col_end, color, is_strikeout)
        let mut deco_runs: Vec<(usize, usize, usize, Rgb, bool)> = Vec::new();
        // (row, col_start, col_end) — selection highlight spans.
        let mut sel_runs: Vec<(usize, usize, usize)> = Vec::new();
        let mut cur_bg: Option<(usize, usize, usize, Rgb)> = None;
        let mut cur_text: Option<TextRun> = None;

        for cell in content.display_iter {
            let row_i = cell.point.line.0 + display_offset;
            if row_i < 0 {
                continue;
            }
            let row = row_i as usize;
            if row >= rows {
                break;
            }
            let col = cell.point.column.0;
            let flags = cell.flags;
            if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let width_cells = if flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
            let (fg_color, bg_color) = if flags.contains(Flags::INVERSE) {
                (cell.bg, cell.fg)
            } else {
                (cell.fg, cell.bg)
            };
            let dim = flags.intersects(Flags::DIM);
            let fg = colors::resolve(fg_color, palette, dim);

            // Selection highlight span.
            if content.selection.is_some_and(|sel| sel.contains(cell.point)) {
                match sel_runs.last_mut() {
                    Some((r, _s, e)) if *r == row && *e == col => *e = col + width_cells,
                    _ => sel_runs.push((row, col, col + width_cells)),
                }
            }

            // Background run (skip the default background: the plate shows through).
            let bg = (bg_color != AnsiColor::Named(NamedColor::Background))
                .then(|| colors::resolve(bg_color, palette, false));
            match (&mut cur_bg, bg) {
                (Some((r, _s, e, rgb)), Some(new)) if *r == row && *e == col && *rgb == new => {
                    *e = col + width_cells;
                }
                (run, bg) => {
                    if let Some(done) = run.take() {
                        bg_runs.push(done);
                    }
                    if let Some(new) = bg {
                        *run = Some((row, col, col + width_cells, new));
                    }
                }
            }

            // Decoration runs (underline family collapses to underline).
            let underline = flags.intersects(
                Flags::UNDERLINE
                    | Flags::DOUBLE_UNDERLINE
                    | Flags::UNDERCURL
                    | Flags::DOTTED_UNDERLINE
                    | Flags::DASHED_UNDERLINE,
            );
            let strikeout = flags.contains(Flags::STRIKEOUT);
            for (on, is_strike) in [(underline, false), (strikeout, true)] {
                if !on {
                    continue;
                }
                if let Some(last) = deco_runs.last_mut() {
                    if last.0 == row && last.2 == col && last.4 == is_strike && last.3 == fg {
                        last.2 = col + width_cells;
                        continue;
                    }
                }
                deco_runs.push((row, col, col + width_cells, fg, is_strike));
            }

            // Text run: spaces and hidden cells only break runs, never render.
            let bold = flags.intersects(Flags::BOLD);
            let italic = flags.intersects(Flags::ITALIC);
            let renders = cell.c != ' ' && !flags.contains(Flags::HIDDEN);
            match &mut cur_text {
                Some(run)
                    if renders
                        && run.row == row
                        && run.next_col == col
                        && run.fg == fg
                        && run.bold == bold
                        && run.italic == italic
                        && width_cells == 1 =>
                {
                    run.text.push(cell.c);
                    run.next_col += 1;
                }
                run => {
                    if let Some(done) = run.take() {
                        text_runs.push(done);
                    }
                    if renders {
                        *run = Some(TextRun {
                            row,
                            col,
                            next_col: col + width_cells,
                            text: cell.c.to_string(),
                            fg,
                            bold,
                            italic,
                        });
                        // A wide char ends its run: the glyph's natural advance
                        // (~2 cells) is not guaranteed to match, so don't let
                        // drift accumulate into following cells.
                        if width_cells == 2 {
                            text_runs.push(run.take().unwrap());
                        }
                    }
                }
            }
        }
        if let Some(run) = cur_bg.take() {
            bg_runs.push(run);
        }
        if let Some(run) = cur_text.take() {
            text_runs.push(run);
        }

        let cursor = content.cursor;
        let cursor_shape = cursor.shape;
        let cursor_row = cursor.point.line.0 + display_offset;
        let cursor_col = cursor.point.column.0;
        let cursor_rgb = colors::indexed(NamedColor::Cursor as usize, palette);
        let mode = content.mode;

        for (row, start, end, rgb) in bg_runs {
            pc.quad(self.cell_rect(row, start, end - start), quad_color(rgb, 1.0));
        }
        for (row, start, end) in sel_runs {
            pc.quad(self.cell_rect(row, start, end - start), quad_color(SELECTION_RGB, 0.28));
        }
        for run in text_runs {
            pc.text_attrs(
                run.text,
                pad + run.col as f32 * CELL_W,
                pad + run.row as f32 * LINE_H,
                FONT_SIZE,
                [run.fg.r, run.fg.g, run.fg.b],
                Some(font.clone()),
                Some(bounds),
                TextAttrs { italic: run.italic, weight: run.bold.then_some(700) },
            );
        }
        for (row, start, end, rgb, is_strike) in deco_runs {
            let mut rect = self.cell_rect(row, start, end - start);
            rect.y += if is_strike { LINE_H * 0.55 } else { LINE_H - 2.0 };
            rect.height = 1.0;
            pc.quad(rect, quad_color(rgb, 1.0));
        }

        // Cursor last, over the glyphs. Only when visible in the viewport
        // (scrolled history moves it off) and not hidden by DECTCEM.
        if cursor_shape != CursorShape::Hidden
            && mode.contains(TermMode::SHOW_CURSOR)
            && (0..rows as i32).contains(&cursor_row)
        {
            let rect = self.cell_rect(cursor_row as usize, cursor_col, 1);
            if !self.focused {
                // Hollow outline while unfocused.
                pc.border(
                    rect,
                    (0.0, 0.0, 0.0, 0.0),
                    [0.0, 0.0, 0.0, 0.0],
                    quad_color(cursor_rgb, 0.8),
                    1.0,
                );
            } else {
                match cursor_shape {
                    CursorShape::Beam => {
                        pc.quad(
                            Rect { width: 2.0, ..rect },
                            quad_color(cursor_rgb, 0.9),
                        );
                    }
                    CursorShape::Underline => {
                        pc.quad(
                            Rect { y: rect.y + LINE_H - 2.0, height: 2.0, ..rect },
                            quad_color(cursor_rgb, 0.9),
                        );
                    }
                    // Block (and HollowBlock while focused): translucent
                    // overlay so the glyph beneath stays readable.
                    _ => pc.quad(rect, quad_color(cursor_rgb, 0.4)),
                }
            }
        }

        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
    }

    fn handle_focus_change(&mut self, focused: bool, needs_rebuild: &mut bool) {
        if self.focused != focused {
            self.focused = focused;
            *needs_rebuild = true;
            // Modifier releases are lost while unfocused — start clean.
            self.shift_down = false;
            self.ctrl_down = false;
            self.alt_down = false;
            self.mouse_buttons = 0;
            if self.term.mode().contains(TermMode::FOCUS_IN_OUT) {
                self.write_pty(if focused { b"\x1b[I" } else { b"\x1b[O" });
            }
        }
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        if std::env::var_os("CCE_TERM_DEBUG").is_some() {
            eprintln!("[input] move ({:.1},{:.1}) selecting={}", pos.x, pos.y, self.selecting);
        }
        if self.mouse_reporting() && !self.selecting {
            let mode = *self.term.mode();
            let motion_wanted = mode.contains(TermMode::MOUSE_MOTION)
                || (mode.contains(TermMode::MOUSE_DRAG) && self.mouse_buttons != 0);
            if motion_wanted {
                let cell = self.viewport_cell(pos);
                if self.last_mouse_cell != Some(cell) {
                    self.last_mouse_cell = Some(cell);
                    // Lowest held button, or 3 (no button) for plain motion.
                    let button = (0..3).find(|b| self.mouse_buttons & (1 << b) != 0).unwrap_or(3);
                    let code = 32 + button + self.report_mods();
                    self.send_mouse_report(code, true, cell.0, cell.1);
                }
            }
            return;
        }
        if !self.selecting {
            return;
        }
        // Dragging past the frame edge nudges the scrollback along (one line
        // per motion event — no autoscroll timer yet).
        if (pos.y as f32) < self.pad {
            self.term.scroll_display(Scroll::Delta(1));
        } else if pos.y as f32 > self.win_h - self.pad {
            self.term.scroll_display(Scroll::Delta(-1));
        }
        let (point, side) = self.grid_point(pos);
        if let Some(selection) = &mut self.term.selection {
            selection.update(point, side);
            *needs_rebuild = true;
        }
    }

    fn handle_mouse_input(
        &mut self,
        button: MouseButton,
        state: ElementState,
        pos: LogicalPosition,
        needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
        if std::env::var_os("CCE_TERM_DEBUG").is_some() {
            eprintln!(
                "[input] {:?} {:?} ({:.1},{:.1}) sel={} mode={:?} shift={}",
                button,
                state,
                pos.x,
                pos.y,
                self.term.selection.is_some(),
                self.term.mode(),
                self.shift_down
            );
        }
        let button_bit = match button {
            MouseButton::Left => Some(0u8),
            MouseButton::Middle => Some(1),
            MouseButton::Right => Some(2),
            _ => None,
        };
        if let Some(bit) = button_bit {
            match state {
                ElementState::Pressed => self.mouse_buttons |= 1 << bit,
                ElementState::Released => self.mouse_buttons &= !(1 << bit),
            }
        }

        // Application-owned pointer: report and stop — no selection, no
        // middle-paste (Shift bypasses via mouse_reporting).
        if self.mouse_reporting() && !self.selecting {
            if let Some(code) = button_bit {
                let (col, row) = self.viewport_cell(pos);
                let code = code + self.report_mods();
                self.send_mouse_report(code, state == ElementState::Pressed, col, row);
                if state == ElementState::Pressed {
                    self.last_mouse_cell = Some((col, row));
                }
            }
            return None;
        }

        match (button, state) {
            (MouseButton::Left, ElementState::Pressed) => {
                let (point, side) = self.grid_point(pos);
                let now = Instant::now();
                let repeat = self
                    .last_click
                    .is_some_and(|(t, p)| now - t < MULTI_CLICK_WINDOW && p == point);
                self.click_count = if repeat { self.click_count % 3 + 1 } else { 1 };
                self.last_click = Some((now, point));
                let ty = match self.click_count {
                    2 => SelectionType::Semantic,
                    3 => SelectionType::Lines,
                    _ => SelectionType::Simple,
                };
                let had_selection = self.term.selection.is_some();
                self.term.selection = Some(Selection::new(ty, point, side));
                self.selecting = true;
                // Semantic/Lines are non-empty immediately; a fresh Simple
                // press only needs a repaint if it cleared an old highlight.
                *needs_rebuild = had_selection || ty != SelectionType::Simple;
            }
            (MouseButton::Left, ElementState::Released) => {
                self.selecting = false;
                // Empty selections (a plain click) drop; real ones go to
                // PRIMARY, per the select-then-middle-click convention.
                match self.term.selection_to_string() {
                    Some(text) if !text.is_empty() => clip::copy_primary(&text),
                    _ => {
                        if self.term.selection.take().is_some() {
                            *needs_rebuild = true;
                        }
                    }
                }
            }
            (MouseButton::Middle, ElementState::Pressed) => {
                if let Some(text) = clip::paste_primary() {
                    if !text.is_empty() {
                        self.paste(&text);
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn handle_mouse_wheel(
        &mut self,
        delta: &MouseScrollDelta,
        pos: LogicalPosition,
        needs_rebuild: &mut bool,
    ) {
        self.scroll_accum += delta.notches_y() * SCROLL_LINES_PER_NOTCH;
        let lines = self.scroll_accum as i32;
        if lines == 0 {
            return;
        }
        self.scroll_accum -= lines as f32;

        // Wheel reports take precedence over alternate-scroll arrows.
        if self.mouse_reporting() {
            let (col, row) = self.viewport_cell(pos);
            let code = if lines > 0 { 64 } else { 65 } + self.report_mods();
            for _ in 0..lines.unsigned_abs() {
                self.send_mouse_report(code, true, col, row);
            }
            return;
        }

        let mode = *self.term.mode();
        if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            // Full-screen apps without mouse reporting get arrow keys.
            let key: &[u8] = if lines > 0 { b"\x1b[A" } else { b"\x1b[B" };
            let bytes = key.repeat(lines.unsigned_abs() as usize);
            self.write_pty(&bytes);
        } else {
            self.term.scroll_display(Scroll::Delta(lines));
            *needs_rebuild = true;
        }
    }

    fn handle_key_input(&mut self, event: &KeyEvent, needs_rebuild: &mut bool) -> Option<Self::Message> {
        // Modifier state for mouse reporting (both edges, ahead of the
        // pressed-only gate).
        match &event.logical_key {
            Key::Named(NamedKey::Shift) => self.shift_down = event.state == ElementState::Pressed,
            Key::Named(NamedKey::Control) => {
                self.ctrl_down = event.state == ElementState::Pressed
            }
            Key::Named(NamedKey::Alt) => self.alt_down = event.state == ElementState::Pressed,
            _ => {}
        }
        if event.state != ElementState::Pressed {
            return None;
        }

        // Clipboard chords, ahead of terminal encoding (a bare Ctrl+C must
        // still reach the shell as SIGINT).
        if event.ctrl && event.shift {
            if let Key::Character(s) = &event.logical_key {
                match s.to_lowercase().as_str() {
                    "c" => {
                        if let Some(text) = self.term.selection_to_string() {
                            if !text.is_empty() {
                                cce_ui::widget::clipboard::copy_to_clipboard(&text);
                            }
                        }
                        return None;
                    }
                    "v" => {
                        if let Some(text) = cce_ui::widget::clipboard::read_from_clipboard() {
                            if !text.is_empty() {
                                self.paste(&text);
                            }
                        }
                        return None;
                    }
                    _ => {}
                }
            }
        }

        if let Some(bytes) = encode_key(event, *self.term.mode()) {
            self.write_pty(&bytes);
            if self.term.selection.take().is_some() {
                *needs_rebuild = true;
            }
            if self.term.grid().display_offset() != 0 {
                self.term.scroll_display(Scroll::Bottom);
                *needs_rebuild = true;
            }
        }
        None
    }

    fn clear_color(&self) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}

fn main() {
    cce_ui::engine::run::<TerminalApp>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::{Column, Line};

    fn term_with(bytes: &[u8]) -> Term<VoidListener> {
        let mut term = Term::new(
            TermConfig::default(),
            &TermDims { cols: 20, rows: 5 },
            VoidListener,
        );
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        term
    }

    #[test]
    fn sgr_colors_land_in_grid() {
        let term = term_with(b"\x1b[31mred\x1b[0m ok");
        let grid = term.grid();
        assert_eq!(grid[Line(0)][Column(0)].c, 'r');
        assert_eq!(grid[Line(0)][Column(0)].fg, AnsiColor::Named(NamedColor::Red));
        assert_eq!(grid[Line(0)][Column(4)].c, 'o');
        assert_eq!(grid[Line(0)][Column(4)].fg, AnsiColor::Named(NamedColor::Foreground));
    }

    #[test]
    fn cursor_addressing_works() {
        // CUP to row 3 col 5, write X — the stub couldn't do this.
        let term = term_with(b"\x1b[3;5HX");
        assert_eq!(term.grid()[Line(2)][Column(4)].c, 'X');
        assert_eq!(term.grid().cursor.point.line, Line(2));
    }

    fn key(logical_key: Key, ctrl: bool, shift: bool, alt: bool) -> KeyEvent {
        let text = match &logical_key {
            Key::Character(s) if !ctrl && !alt => Some(s.clone()),
            _ => None,
        };
        KeyEvent { state: ElementState::Pressed, logical_key, text, repeat: false, ctrl, shift, alt }
    }

    #[test]
    fn app_cursor_mode_switches_arrow_encoding() {
        let ev = key(Key::Named(NamedKey::ArrowUp), false, false, false);
        assert_eq!(encode_key(&ev, TermMode::empty()).unwrap(), b"\x1b[A");
        assert_eq!(encode_key(&ev, TermMode::APP_CURSOR).unwrap(), b"\x1bOA");
    }

    #[test]
    fn modifier_parameters_on_csi_keys() {
        let up = |c, s, a| key(Key::Named(NamedKey::ArrowUp), c, s, a);
        // alt = +2, ctrl = +4, shift = +1 on the xterm modifier parameter.
        assert_eq!(encode_key(&up(false, false, true), TermMode::empty()).unwrap(), b"\x1b[1;3A");
        assert_eq!(encode_key(&up(true, false, false), TermMode::empty()).unwrap(), b"\x1b[1;5A");
        assert_eq!(encode_key(&up(true, true, true), TermMode::empty()).unwrap(), b"\x1b[1;8A");
        // Modified keys keep CSI form even in app-cursor mode.
        assert_eq!(
            encode_key(&up(true, false, false), TermMode::APP_CURSOR).unwrap(),
            b"\x1b[1;5A"
        );
        // Tilde keys carry the parameter after their number.
        let del = key(Key::Named(NamedKey::Delete), false, false, true);
        assert_eq!(encode_key(&del, TermMode::empty()).unwrap(), b"\x1b[3;3~");
    }

    #[test]
    fn alt_prefixes_esc() {
        let b = key(Key::Character("b".into()), false, false, true);
        assert_eq!(encode_key(&b, TermMode::empty()).unwrap(), b"\x1bb");
        // ctrl+alt compose: ESC then the ctrl byte.
        let w = key(Key::Character("w".into()), true, false, true);
        assert_eq!(encode_key(&w, TermMode::empty()).unwrap(), vec![0x1b, 0x17]);
        // alt+backspace: readline backward-kill-word.
        let bs = key(Key::Named(NamedKey::Backspace), false, false, true);
        assert_eq!(encode_key(&bs, TermMode::empty()).unwrap(), vec![0x1b, 0x7f]);
        // shift+tab is backtab regardless of other state.
        let tab = key(Key::Named(NamedKey::Tab), false, true, false);
        assert_eq!(encode_key(&tab, TermMode::empty()).unwrap(), b"\x1b[Z");
    }

    #[test]
    fn selection_extracts_text() {
        let mut term = term_with(b"hello world\r\nsecond line");
        // Word-select "world": semantic selection from a point inside it.
        term.selection = Some(Selection::new(
            SelectionType::Semantic,
            Point::new(Line(0), Column(8)),
            Side::Left,
        ));
        assert_eq!(term.selection_to_string().as_deref(), Some("world"));
        // Line-select the second row.
        term.selection = Some(Selection::new(
            SelectionType::Lines,
            Point::new(Line(1), Column(3)),
            Side::Left,
        ));
        // Line selections carry their trailing newline.
        assert_eq!(term.selection_to_string().as_deref(), Some("second line\n"));
        // Simple drag across the first word.
        let mut sel =
            Selection::new(SelectionType::Simple, Point::new(Line(0), Column(0)), Side::Left);
        sel.update(Point::new(Line(0), Column(4)), Side::Right);
        term.selection = Some(sel);
        assert_eq!(term.selection_to_string().as_deref(), Some("hello"));
    }

    #[test]
    fn mouse_report_encodings() {
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        // SGR: press 'M', release 'm', same button code, 1-based coords.
        assert_eq!(mouse_report_bytes(sgr, 0, true, 4, 2).unwrap(), b"\x1b[<0;5;3M");
        assert_eq!(mouse_report_bytes(sgr, 2, false, 0, 0).unwrap(), b"\x1b[<2;1;1m");
        // Legacy: +32 bytes, release collapses the button to 3.
        let legacy = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            mouse_report_bytes(legacy, 0, true, 4, 2).unwrap(),
            vec![0x1b, b'[', b'M', 32, 37, 35]
        );
        assert_eq!(
            mouse_report_bytes(legacy, 0, false, 4, 2).unwrap(),
            vec![0x1b, b'[', b'M', 35, 37, 35]
        );
        // Legacy coordinate saturation at byte 255.
        assert_eq!(mouse_report_bytes(legacy, 0, true, 300, 2).unwrap()[4], 255);
        // UTF-8 extended coords: col 200 → 233 → two-byte UTF-8.
        let utf8 = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
        let bytes = mouse_report_bytes(utf8, 0, true, 199, 0).unwrap();
        assert_eq!(&bytes[4..], "\u{e8}\u{21}".to_string().as_bytes());
    }

    #[test]
    fn mouse_modes_land_from_escapes() {
        let term = term_with(b"\x1b[?1002h\x1b[?1006h");
        assert!(term.mode().contains(TermMode::MOUSE_DRAG));
        assert!(term.mode().contains(TermMode::SGR_MOUSE));
        assert!(term.mode().intersects(TermMode::MOUSE_MODE));
    }

    #[test]
    fn paste_encoding() {
        assert_eq!(paste_bytes("a\nb\r\nc", false), b"a\rb\rc");
        assert_eq!(
            paste_bytes("hi\x1b[201~!", true),
            b"\x1b[200~hi!\x1b[201~"
        );
    }

    #[test]
    fn ctrl_chars_encode() {
        let c = key(Key::Character("c".into()), true, false, false);
        assert_eq!(encode_key(&c, TermMode::empty()).unwrap(), vec![0x03]);
        let a = key(Key::Character("a".into()), false, false, false);
        assert_eq!(encode_key(&a, TermMode::empty()).unwrap(), b"a");
    }
}
