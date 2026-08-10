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
//! Not yet: selection/clipboard, mouse reporting, alt-as-ESC (cce-ui's
//! `KeyEvent` carries no alt modifier), measured cell metrics (0.60 em / 1.2 em
//! estimates — exact for Berkeley Mono in practice).

mod colors;
mod pty;

use std::io::{Read, Write};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
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
/// DECCKM (application cursor keys). No alt-as-ESC yet: `KeyEvent` carries no
/// alt modifier.
fn encode_key(ev: &KeyEvent, mode: TermMode) -> Option<Vec<u8>> {
    let app = mode.contains(TermMode::APP_CURSOR);
    match &ev.logical_key {
        Key::Named(k) => {
            let b: &[u8] = match k {
                NamedKey::Enter => b"\r",
                NamedKey::Backspace => b"\x7f",
                NamedKey::Tab => b"\t",
                NamedKey::Escape => b"\x1b",
                NamedKey::Space => b" ",
                NamedKey::ArrowUp => {
                    if app {
                        b"\x1bOA"
                    } else {
                        b"\x1b[A"
                    }
                }
                NamedKey::ArrowDown => {
                    if app {
                        b"\x1bOB"
                    } else {
                        b"\x1b[B"
                    }
                }
                NamedKey::ArrowRight => {
                    if app {
                        b"\x1bOC"
                    } else {
                        b"\x1b[C"
                    }
                }
                NamedKey::ArrowLeft => {
                    if app {
                        b"\x1bOD"
                    } else {
                        b"\x1b[D"
                    }
                }
                NamedKey::Home => {
                    if app {
                        b"\x1bOH"
                    } else {
                        b"\x1b[H"
                    }
                }
                NamedKey::End => {
                    if app {
                        b"\x1bOF"
                    } else {
                        b"\x1b[F"
                    }
                }
                NamedKey::PageUp => b"\x1b[5~",
                NamedKey::PageDown => b"\x1b[6~",
                NamedKey::Delete => b"\x1b[3~",
                NamedKey::F5 => b"\x1b[15~",
                _ => return None,
            };
            Some(b.to_vec())
        }
        Key::Character(s) => {
            if ev.ctrl {
                match s.chars().next()?.to_ascii_lowercase() {
                    c @ 'a'..='z' => Some(vec![c as u8 - b'a' + 1]),
                    '[' => Some(vec![0x1b]),
                    '\\' => Some(vec![0x1c]),
                    ']' => Some(vec![0x1d]),
                    _ => None,
                }
            } else if let Some(t) = &ev.text {
                Some(t.as_bytes().to_vec())
            } else {
                Some(s.as_bytes().to_vec())
            }
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
                // Selection/clipboard phase: ClipboardStore/ClipboardLoad.
                // Bell/Wakeup/cursor-blink: nothing to do yet.
                _ => {}
            },
        }
    }

    fn tick(&mut self, _dt: f32, _needs_rebuild: &mut bool) {}

    fn handle_resize(&mut self, width: f32, height: f32, _scale: f64) {
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
        }
    }

    fn handle_pointer_move(&mut self, _pos: LogicalPosition, _needs_rebuild: &mut bool) {}

    fn handle_mouse_input(
        &mut self,
        _button: MouseButton,
        _state: ElementState,
        _pos: LogicalPosition,
        _needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
        None
    }

    fn handle_mouse_wheel(
        &mut self,
        delta: &MouseScrollDelta,
        _pos: LogicalPosition,
        needs_rebuild: &mut bool,
    ) {
        self.scroll_accum += delta.notches_y() * SCROLL_LINES_PER_NOTCH;
        let lines = self.scroll_accum as i32;
        if lines == 0 {
            return;
        }
        self.scroll_accum -= lines as f32;

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
        if event.state != ElementState::Pressed {
            return None;
        }
        if let Some(bytes) = encode_key(event, *self.term.mode()) {
            self.write_pty(&bytes);
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

    #[test]
    fn app_cursor_mode_switches_arrow_encoding() {
        let ev = KeyEvent {
            state: ElementState::Pressed,
            logical_key: Key::Named(NamedKey::ArrowUp),
            text: None,
            repeat: false,
            ctrl: false,
            shift: false,
        };
        assert_eq!(encode_key(&ev, TermMode::empty()).unwrap(), b"\x1b[A");
        assert_eq!(encode_key(&ev, TermMode::APP_CURSOR).unwrap(), b"\x1bOA");
    }

    #[test]
    fn ctrl_chars_encode() {
        let ev = |c: &str, ctrl: bool| KeyEvent {
            state: ElementState::Pressed,
            logical_key: Key::Character(c.to_string()),
            text: Some(c.to_string()),
            repeat: false,
            ctrl,
            shift: false,
        };
        assert_eq!(encode_key(&ev("c", true), TermMode::empty()).unwrap(), vec![0x03]);
        assert_eq!(encode_key(&ev("a", false), TermMode::empty()).unwrap(), b"a");
    }
}
