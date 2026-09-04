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
//! Config (KDL, live-reloaded on file change): a `terminal { … }` section in
//! the shared `~/.config/cce/config.kdl` or the per-app
//! `~/.config/cce/cce-terminal/config.kdl` (app file wins) with `font_size`,
//! `scrollback`, and a `colors { … }` block naming `foreground`, `background`,
//! `cursor`, `selection`, and the 16 ANSI slots (`black` … `bright_white`)
//! as hex strings. The font family comes from the shared config's
//! `fonts { terminal }` key, like the rest of the DE's font routing.
//!
//! Not yet: measured cell metrics (0.60 em / 1.2 em estimates — exact for
//! Berkeley Mono in practice).

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
use cce_ui::widget::{Bounds, ElementState, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey, ScrollMotion};
use wayland_client::QueueHandle;

const INIT_W: u32 = 840;
const INIT_H: u32 = 520;
/// Wheel notches → scrollback lines.
const SCROLL_LINES_PER_NOTCH: f32 = 3.0;
/// Presses in the same cell within this window escalate Simple → Semantic →
/// Lines selection.
const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// The KDL-configurable knobs (`terminal { … }` in the shared config.kdl or
/// the per-app `~/.config/cce/cce-terminal/config.kdl`, app file winning) and
/// the cell metrics derived from them. Toolkit conventions for the metrics:
/// ceil(font_size × 1.2) line height (`text_leaf_height`), 0.60 × font_size
/// mono advance (`estimate_label_width_helper`).
#[derive(Clone, Copy, PartialEq)]
struct Settings {
    font_size: f32,
    line_h: f32,
    cell_w: f32,
    scrollback: usize,
    palette: colors::Palette,
}

impl Settings {
    fn load(font_family: &str) -> Self {
        let font_size = cce_ui::config::get_f32("/terminal/font_size", 14.0).clamp(6.0, 72.0);
        let scrollback =
            cce_ui::config::get_i64("/terminal/scrollback", 5000).clamp(0, 200_000) as usize;
        // Measure the real glyph advance; the 0.60 em toolkit estimate is the
        // fallback (and the sanity band — a broken measurement won't wreck
        // the grid).
        let estimate = font_size * 0.60;
        let cell_w = measure_advance(font_family, font_size)
            .filter(|w| (0.5 * estimate..2.0 * estimate).contains(w))
            .unwrap_or(estimate);
        if std::env::var_os("CCE_TERM_DEBUG").is_some() {
            eprintln!("[metrics] family={font_family} size={font_size} cell_w={cell_w} (estimate {estimate})");
        }
        Settings {
            font_size,
            line_h: (font_size * 1.2).ceil(),
            cell_w,
            scrollback,
            palette: colors::Palette::from_config(),
        }
    }
}

/// Shape a long run of one ASCII glyph in the terminal font and divide out
/// the per-cell advance. Uses an app-side bundled-fonts `FontSystem` (the
/// documented pattern for measurement), cached across config reloads.
fn measure_advance(font_family: &str, font_size: f32) -> Option<f32> {
    use std::sync::{Mutex, OnceLock};
    static FONT_SYSTEM: OnceLock<Mutex<cce_ui::cosmic_text::FontSystem>> = OnceLock::new();
    const RUN: usize = 64;
    let fs = FONT_SYSTEM.get_or_init(|| Mutex::new(cce_ui::create_font_system()));
    let mut fs = fs.lock().ok()?;
    let mut buffer = cce_ui::cosmic_text::Buffer::new(
        &mut fs,
        cce_ui::cosmic_text::Metrics::new(font_size, (font_size * 1.2).ceil()),
    );
    buffer.set_size(&mut fs, None, None);
    buffer.set_text(
        &mut fs,
        &"M".repeat(RUN),
        cce_ui::cosmic_text::Attrs::new().family(cce_ui::cosmic_text::Family::Name(font_family)),
        cce_ui::cosmic_text::Shaping::Advanced,
    );
    let advance = buffer.layout_runs().next()?.line_w / RUN as f32;
    (advance.is_finite() && advance > 0.0).then_some(advance)
}

/// The app's rebindable shortcuts, resolved once at startup from input.kdl
/// (domain `cce-terminal`, falling back to `cce-ui` then these defaults).
struct Keys {
    copy: String,
    paste: String,
    scroll_up: String,
    scroll_down: String,
}

impl Keys {
    fn load() -> Self {
        let get = cce_ui::input::app_chord;
        Keys {
            copy: get("copy", "ctrl+shift+c"),
            paste: get("paste", "ctrl+shift+v"),
            scroll_up: get("scroll_up", "shift+pageup"),
            scroll_down: get("scroll_down", "shift+pagedown"),
        }
    }
}

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
    /// Fractional wheel remainder for the whole-line wheel protocols (mouse
    /// reports, alternate-scroll arrows), where a trackpad's pixel deltas
    /// have to add up to a line before anything is sent.
    scroll_accum: f32,
    /// Scrollback view motion in LINE units: `y.pos()` is the float display
    /// offset — a notch glides it, a trackpad tracks it 1:1 and its flick
    /// coasts (cce-ui's ScrollMotion) — and each frame the Term is stepped to
    /// its rounding. Drawing stays line-quantized; only the offset is smooth.
    scroll_motion: ScrollMotion,
    focused: bool,
    /// Left button held: pointer moves extend the selection.
    selecting: bool,
    /// Multi-click escalation state: last press instant + cell.
    last_click: Option<(Instant, Point)>,
    click_count: u8,
    settings: Settings,
    keys: Keys,
    /// Config-file mtime at the last (re)load — tick polls it so palette and
    /// font-size edits apply live, per the DE's edit-the-file convention.
    config_stamp: Option<std::time::SystemTime>,
    /// Bell flash intensity, 1.0 → 0 over ~a quarter second (decayed in tick).
    bell: f32,
    /// Last pointer position (any state) — tick's autoscroll reads it while a
    /// selection drag holds the pointer in the frame's edge padding.
    last_pointer: LogicalPosition,
    autoscroll_accum: f32,
    /// Current window size (for grid re-derivation on config reload and
    /// selection edge-autoscroll bounds).
    win_w: f32,
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
        grid_dims(w, h, self.pad, &self.settings)
    }

    fn write_pty(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
    }

    fn cell_rect(&self, row: usize, col: usize, width_cells: usize) -> Rect {
        Rect {
            x: self.pad + col as f32 * self.settings.cell_w,
            y: self.pad + row as f32 * self.settings.line_h,
            width: width_cells as f32 * self.settings.cell_w,
            height: self.settings.line_h,
        }
    }

    /// Pixel position → 0-based viewport cell (clamped).
    fn viewport_cell(&self, pos: LogicalPosition) -> (usize, usize) {
        let col = (((pos.x as f32 - self.pad) / self.settings.cell_w).max(0.0) as usize)
            .min(self.cols as usize - 1);
        let row = (((pos.y as f32 - self.pad) / self.settings.line_h).max(0.0) as usize)
            .min(self.rows as usize - 1);
        (col, row)
    }

    /// Pixel position → grid point (viewport-clamped; grid-space line, so
    /// scrolled history resolves to negative lines) plus which half of the
    /// cell was hit.
    fn grid_point(&self, pos: LogicalPosition) -> (Point, Side) {
        let (col, row) = self.viewport_cell(pos);
        let col_f = ((pos.x as f32 - self.pad) / self.settings.cell_w).max(0.0);
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

    /// Quantize wheel motion for the whole-line protocols, carrying the
    /// fractional remainder across events.
    fn take_whole_lines(&mut self, lines: f32) -> Option<i32> {
        self.scroll_accum += lines;
        let whole = self.scroll_accum as i32;
        if whole == 0 {
            return None;
        }
        self.scroll_accum -= whole as f32;
        Some(whole)
    }

    /// Adopt view moves made behind the motion's back — PageUp/PageDown, the
    /// snap to the bottom on a keypress or paste, new output pushing a held
    /// view up the history — so the motion resumes from where the view is.
    fn sync_scroll_motion(&mut self) {
        let offset = self.term.grid().display_offset() as f32;
        if self.scroll_motion.y.pos().round() != offset {
            self.scroll_motion.y.jump_to(offset);
        }
    }

    /// The scrollback offset's range: 0 (live bottom) ..= history length.
    fn scrollback_bounds(&self) -> Bounds {
        Bounds::max(self.term.grid().history_size() as f32)
    }

    /// Step the Term to the motion's rounded offset; true if the view moved.
    fn apply_scroll_motion(&mut self) -> bool {
        let target = self.scroll_motion.y.pos().round() as i32;
        let current = self.term.grid().display_offset() as i32;
        if target == current {
            return false;
        }
        self.term.scroll_display(Scroll::Delta(target - current));
        true
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

fn grid_dims(w: f32, h: f32, pad: f32, settings: &Settings) -> (u16, u16) {
    let cols = (((w - 2.0 * pad) / settings.cell_w).floor() as i64).clamp(2, u16::MAX as i64);
    let rows = (((h - 2.0 * pad) / settings.line_h).floor() as i64).clamp(1, u16::MAX as i64);
    (cols as u16, rows as u16)
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
        let pad = cce_ui::layout::root_plate_padding();
        // `.3` is the `fonts { terminal }` family (this was the 7-tuple's
        // fontconfig `terminal` alias before the DE's fonts moved into the
        // shared KDL config), falling back to Noto Sans Mono.
        let font = cce_ui::layout::read_preferred_fonts().3;
        let settings = Settings::load(&font);
        let config_stamp = cce_ui::config::config_files_modified();
        let (cols, rows) = grid_dims(INIT_W as f32, INIT_H as f32, pad, &settings);

        // A command instead of $SHELL: `cce-terminal -e <cmd> [args…]`
        // (xterm-style), or bare trailing args (foot-style) — the launcher
        // hosts `Terminal=true` entries positionally (`term sh -c …`), so
        // both conventions must work.
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let command: Option<Vec<String>> = match argv.first().map(String::as_str) {
            Some("-e") => Some(argv[1..].to_vec()).filter(|c| !c.is_empty()),
            Some(_) => Some(argv.clone()),
            None => None,
        };

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

        let config =
            TermConfig { scrolling_history: settings.scrollback, ..TermConfig::default() };
        let term = Term::new(config, &TermDims { cols, rows }, EventProxy(sender));

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
            scroll_motion: ScrollMotion::new(),
            focused: true,
            selecting: false,
            last_click: None,
            click_count: 0,
            settings,
            keys: Keys::load(),
            config_stamp,
            bell: 0.0,
            last_pointer: LogicalPosition::new(0.0, 0.0),
            autoscroll_accum: 0.0,
            win_w: INIT_W as f32,
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
                        .unwrap_or_else(|| colors::default_color(index, &self.settings.palette));
                    let response = format(rgb);
                    self.write_pty(response.as_bytes());
                }
                TermEvent::TextAreaSizeRequest(format) => {
                    let response = format(WindowSize {
                        num_lines: self.rows,
                        num_cols: self.cols,
                        cell_width: self.settings.cell_w as u16,
                        cell_height: self.settings.line_h as u16,
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
                TermEvent::Bell => {
                    self.bell = 1.0;
                    *needs_rebuild = true;
                }
                // Wakeup/cursor-blink: nothing to do.
                _ => {}
            },
        }
    }

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        // Bell flash decay.
        if self.bell > 0.0 {
            self.bell = (self.bell - dt * 4.0).max(0.0);
            *needs_rebuild = true;
        }

        // Wheel glide / trackpad coast through the scrollback: advance the
        // line-unit motion and step the Term to its rounded offset, asking
        // for frames while it is still moving.
        if self.scroll_motion.is_animating() {
            self.sync_scroll_motion();
            if self.scroll_motion.is_animating() {
                let bounds = self.scrollback_bounds();
                self.scroll_motion.tick(dt, Bounds::max(0.0), bounds);
                if self.apply_scroll_motion() || self.scroll_motion.is_animating() {
                    *needs_rebuild = true;
                }
            }
        }

        // Selection autoscroll: while a drag holds the pointer in the top or
        // bottom edge padding, scroll at a rate scaling with the overshoot
        // (motion events stop at the edge, so this is time-driven).
        if self.selecting {
            let y = self.last_pointer.y as f32;
            let overshoot = if y < self.pad {
                self.pad - y
            } else if y > self.win_h - self.pad {
                (self.win_h - self.pad) - y
            } else {
                0.0
            };
            if overshoot != 0.0 {
                let rate = 4.0 + overshoot.abs().min(24.0) * 2.0; // lines/sec
                self.autoscroll_accum += dt * rate * overshoot.signum();
                let lines = self.autoscroll_accum as i32;
                if lines != 0 {
                    self.autoscroll_accum -= lines as f32;
                    self.term.scroll_display(Scroll::Delta(lines));
                    let (point, side) = self.grid_point(self.last_pointer);
                    if let Some(selection) = &mut self.term.selection {
                        selection.update(point, side);
                    }
                    *needs_rebuild = true;
                }
            } else {
                self.autoscroll_accum = 0.0;
            }
        }

        // Config edits apply live: poll the config files' mtime (the
        // toolkit's own getters stat them on every call anyway) and re-derive
        // settings, grid, and pty size on change. Scrollback capacity is the
        // exception — it's baked into the Term at startup.
        let stamp = cce_ui::config::config_files_modified();
        if stamp == self.config_stamp {
            return;
        }
        self.config_stamp = stamp;
        let font = self.font.clone();
        let reloaded = Settings::load(&font);
        let old = std::mem::replace(&mut self.settings, reloaded);
        if reloaded != old {
            *needs_rebuild = true;
            let (cols, rows) = self.grid_for(self.win_w, self.win_h);
            if (cols, rows) != (self.cols, self.rows) {
                self.cols = cols;
                self.rows = rows;
                self.pty.resize(cols, rows);
                self.term.resize(TermDims { cols, rows });
            }
        }
    }

    fn handle_resize(&mut self, width: f32, height: f32, _scale: f64) {
        self.win_w = width;
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
            plate[3] = cce_ui::color::root_plate_opacity();
        }
        // PlateSpec (cce-ui RFC 7b): the root plate wears the silhouette arc.
        let frame = Rect { x: 0.0, y: 0.0, width: w, height: h };
        pc.plate_spec(&cce_ui::scene::paint::PlateSpec {
            rect: frame,
            color: plate,
            blur: false,
            window_corners: (true, true, true, true),
            depth: cce_ui::layout::bevel_width(),
        });

        let rows = self.rows as usize;
        let pad = self.pad;
        let font = self.font.clone();
        let bounds = [pad, 0.0, w - pad, h];

        let content = self.term.renderable_content();
        let display_offset = content.display_offset as i32;
        let overrides = content.colors;
        let cfg_palette = self.settings.palette;

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
            let fg = colors::resolve(fg_color, overrides, dim, &cfg_palette);

            // Selection highlight span.
            if content.selection.is_some_and(|sel| sel.contains(cell.point)) {
                match sel_runs.last_mut() {
                    Some((r, _s, e)) if *r == row && *e == col => *e = col + width_cells,
                    _ => sel_runs.push((row, col, col + width_cells)),
                }
            }

            // Background run (skip the default background: the plate shows through).
            let bg = (bg_color != AnsiColor::Named(NamedColor::Background))
                .then(|| colors::resolve(bg_color, overrides, false, &cfg_palette));
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
        let cursor_rgb = colors::indexed(NamedColor::Cursor as usize, overrides, &cfg_palette);
        let mode = content.mode;

        for (row, start, end, rgb) in bg_runs {
            pc.quad(self.cell_rect(row, start, end - start), quad_color(rgb, 1.0));
        }
        for (row, start, end) in sel_runs {
            pc.quad(
                self.cell_rect(row, start, end - start),
                quad_color(cfg_palette.selection, 0.28),
            );
        }
        for run in text_runs {
            pc.text_attrs(
                run.text,
                pad + run.col as f32 * self.settings.cell_w,
                pad + run.row as f32 * self.settings.line_h,
                self.settings.font_size,
                [run.fg.r, run.fg.g, run.fg.b],
                Some(font.clone()),
                Some(bounds),
                TextAttrs { italic: run.italic, weight: run.bold.then_some(700) },
            );
        }
        for (row, start, end, rgb, is_strike) in deco_runs {
            let mut rect = self.cell_rect(row, start, end - start);
            rect.y += if is_strike {
                self.settings.line_h * 0.55
            } else {
                self.settings.line_h - 2.0
            };
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
                            Rect { y: rect.y + self.settings.line_h - 2.0, height: 2.0, ..rect },
                            quad_color(cursor_rgb, 0.9),
                        );
                    }
                    // Block (and HollowBlock while focused): translucent
                    // overlay so the glyph beneath stays readable.
                    _ => pc.quad(rect, quad_color(cursor_rgb, 0.4)),
                }
            }
        }

        // Visual bell: a brief foreground-tinted wash over the frame.
        if self.bell > 0.0 {
            pc.quad(frame, quad_color(cfg_palette.foreground, 0.12 * self.bell));
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
        self.last_pointer = pos;
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
        // Edge overshoot scrolls on tick's autoscroll clock, not per event.
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
        // Lines, signed like the display offset: up is positive.
        let lines = delta.notches_y() * SCROLL_LINES_PER_NOTCH;

        // Wheel reports take precedence over alternate-scroll arrows. Both
        // are whole-line protocols and stay quantized.
        if self.mouse_reporting() {
            let Some(lines) = self.take_whole_lines(lines) else { return };
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
            let Some(lines) = self.take_whole_lines(lines) else { return };
            let key: &[u8] = if lines > 0 { b"\x1b[A" } else { b"\x1b[B" };
            let bytes = key.repeat(lines.unsigned_abs() as usize);
            self.write_pty(&bytes);
            return;
        }

        // Scrollback: feed the line-unit motion. `apply_px` rather than
        // `apply` — that one's sign flip and 1:1 pixel path are for pixel
        // offsets, whereas here up grows the offset and the delta is already
        // in lines. A finger lift arrives as a zero delta and must reach the
        // motion (it is what starts the coast), so no early return on zero.
        self.sync_scroll_motion();
        let bounds = self.scrollback_bounds();
        let discrete = matches!(delta, MouseScrollDelta::LineDelta(..));
        let moved = self.scroll_motion.apply_px(0.0, lines, discrete, Bounds::max(0.0), bounds);
        if moved && self.apply_scroll_motion() {
            *needs_rebuild = true;
        }
        if self.scroll_motion.is_animating() {
            // A notch only moved the target; tick glides the view there.
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

        // App shortcuts (input.kdl-rebindable), ahead of terminal encoding —
        // a bare Ctrl+C must still reach the shell as SIGINT.
        use cce_ui::widget::match_key_shortcut;
        if match_key_shortcut(event, &self.keys.copy) {
            if let Some(text) = self.term.selection_to_string() {
                if !text.is_empty() {
                    cce_ui::widget::clipboard::copy_to_clipboard(&text);
                }
            }
            return None;
        }
        if match_key_shortcut(event, &self.keys.paste) {
            if let Some(text) = cce_ui::widget::clipboard::read_from_clipboard() {
                if !text.is_empty() {
                    self.paste(&text);
                }
            }
            return None;
        }
        if match_key_shortcut(event, &self.keys.scroll_up) {
            self.term.scroll_display(Scroll::PageUp);
            *needs_rebuild = true;
            return None;
        }
        if match_key_shortcut(event, &self.keys.scroll_down) {
            self.term.scroll_display(Scroll::PageDown);
            *needs_rebuild = true;
            return None;
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
    // cce-ui logs its own fatal paths (Wayland dispatch / protocol errors that
    // end the event loop) through `log`, which is a no-op sink unless the app
    // installs a logger — without this, an app that dies with its compositor
    // connection leaves no explanation behind.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    log::info!("cce-terminal starting (pid {})", std::process::id());
    cce_ui::engine::run::<TerminalApp>();
    log::info!("cce-terminal event loop returned; exiting");
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
    fn measured_advance_is_sane() {
        // Depends on the bundled fonts being present ($CCE_FONTS_DIR /
        // ~/Dropbox/Fonts); when they are, the measured advance must sit in
        // the mono band around the 0.60 em estimate.
        if let Some(adv) = measure_advance("Berkeley Mono", 14.0) {
            assert!((5.0..=14.0).contains(&adv), "advance {adv} out of band");
        }
    }

    #[test]
    fn shortcut_matching_honors_alt() {
        let ev = key(Key::Character("c".into()), true, true, false);
        assert!(cce_ui::widget::match_key_shortcut(&ev, "ctrl+shift+c"));
        assert!(!cce_ui::widget::match_key_shortcut(&ev, "ctrl+alt+c"));
        let alt_ev = key(Key::Character("c".into()), false, false, true);
        assert!(cce_ui::widget::match_key_shortcut(&alt_ev, "alt+c"));
        assert!(!cce_ui::widget::match_key_shortcut(&alt_ev, "c"));
    }

    #[test]
    fn ctrl_chars_encode() {
        let c = key(Key::Character("c".into()), true, false, false);
        assert_eq!(encode_key(&c, TermMode::empty()).unwrap(), vec![0x03]);
        let a = key(Key::Character("a".into()), false, false, false);
        assert_eq!(encode_key(&a, TermMode::empty()).unwrap(), b"a");
    }
}
