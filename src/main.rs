//! cce-terminal — terminal emulator for the cce desktop.
//!
//! PTY-spike state: spawns `$SHELL` on a pty (TERM=dumb), pumps output through
//! a reader thread into the engine's message channel, renders a scrollback of
//! plain text lines, and encodes key input back to the pty. The VT layer is a
//! deliberate stub ([`term::Screen`]) — the planned next step replaces it with
//! `alacritty_terminal` and a real cell grid.

mod pty;
mod term;

use std::io::{Read, Write};

use cce_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{DisplayList, PaintCtx};
use cce_ui::widget::{ElementState, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey};
use wayland_client::QueueHandle;

const FONT_SIZE: f32 = 14.0;
/// Toolkit conventions: ceil(font_size × 1.2) line height (`text_leaf_height`),
/// 0.60 × font_size mono advance (`estimate_label_width_helper`).
const LINE_H: f32 = 17.0;
const CELL_W: f32 = FONT_SIZE * 0.60;
const FG: [u8; 3] = [0xd8, 0xd8, 0xde];
const CURSOR: [f32; 4] = [0.80, 0.80, 0.85, 0.35];
const INIT_W: u32 = 840;
const INIT_H: u32 = 520;

#[derive(Clone)]
enum Msg {
    Pty(Vec<u8>),
    PtyClosed,
}

struct TerminalApp {
    screen: term::Screen,
    pty: pty::Pty,
    writer: std::fs::File,
    font: String,
    pad: f32,
    cols: u16,
    rows: u16,
    /// Lines scrolled up from the bottom of the scrollback.
    scroll_offset: usize,
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
}

/// Terminal byte encoding for a key press. `None` = nothing to send (bare
/// modifiers, unmapped chords). No alt-as-ESC yet: `KeyEvent` carries no alt
/// modifier — a toolkit extension when the real VT layer lands.
fn encode_key(ev: &KeyEvent) -> Option<Vec<u8>> {
    match &ev.logical_key {
        Key::Named(k) => {
            let b: &[u8] = match k {
                NamedKey::Enter => b"\r",
                NamedKey::Backspace => b"\x7f",
                NamedKey::Tab => b"\t",
                NamedKey::Escape => b"\x1b",
                NamedKey::Space => b" ",
                NamedKey::ArrowUp => b"\x1b[A",
                NamedKey::ArrowDown => b"\x1b[B",
                NamedKey::ArrowRight => b"\x1b[C",
                NamedKey::ArrowLeft => b"\x1b[D",
                NamedKey::Home => b"\x1b[H",
                NamedKey::End => b"\x1b[F",
                NamedKey::PageUp => b"\x1b[5~",
                NamedKey::PageDown => b"\x1b[6~",
                NamedKey::Delete => b"\x1b[3~",
                _ => return None,
            };
            Some(b.to_vec())
        }
        Key::Character(s) => {
            if ev.ctrl {
                match s.chars().next()?.to_ascii_lowercase() {
                    c @ 'a'..='z' => Some(vec![c as u8 - b'a' + 1]),
                    '[' => Some(vec![0x1b]),
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

impl Application for TerminalApp {
    type Message = Msg;

    fn new(
        _qh: &QueueHandle<EngineState<Self>>,
        sender: calloop::channel::Sender<Self::Message>,
    ) -> Self {
        let pad = cce_ui::layout::backplate_padding();
        let cols = (((INIT_W as f32 - 2.0 * pad) / CELL_W) as i64).clamp(2, u16::MAX as i64) as u16;
        let rows = (((INIT_H as f32 - 2.0 * pad) / LINE_H) as i64).clamp(1, u16::MAX as i64) as u16;

        let pty = pty::spawn_shell(cols, rows).expect("cce-terminal: failed to spawn shell on pty");
        let writer = pty.dup_handle().expect("cce-terminal: pty dup failed");
        let mut reader = pty.dup_handle().expect("cce-terminal: pty dup failed");
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if sender.send(Msg::Pty(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // EIO when the last slave fd closes: the normal exit path.
                    Err(_) => break,
                }
            }
            let _ = sender.send(Msg::PtyClosed);
        });

        // Index 6 of the preferred-fonts tuple is the `terminal` alias
        // (~/.config/fontconfig/fonts.conf), falling back to Noto Sans Mono.
        let font = cce_ui::layout::read_preferred_fonts().6;

        TerminalApp {
            screen: term::Screen::new(),
            pty,
            writer,
            font,
            pad,
            cols,
            rows,
            scroll_offset: 0,
        }
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: "cce-terminal".to_string(),
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
                self.screen.feed(&bytes);
                self.scroll_offset = 0;
                *needs_rebuild = true;
            }
            Msg::PtyClosed => {
                let _ = self.pty.child.wait();
                *exit = true;
            }
        }
    }

    fn tick(&mut self, _dt: f32, _needs_rebuild: &mut bool) {}

    fn handle_resize(&mut self, width: f32, height: f32, _scale: f64) {
        let (cols, rows) = self.grid_for(width, height);
        if (cols, rows) != (self.cols, self.rows) {
            self.cols = cols;
            self.rows = rows;
            self.pty.resize(cols, rows);
        }
    }

    fn display_list(&mut self, size: LogicalSize, _scale: f64) -> Option<DisplayList> {
        let (w, h) = (size.width, size.height);
        let mut pc = PaintCtx::new();

        // The window plate, per the DE convention (page-low color at backplate
        // opacity, config corner radius, rolled perimeter).
        let mut plate = cce_ui::color::page_low_color();
        if plate[3] > 0.001 {
            plate[3] = cce_ui::color::active_backplate_opacity();
        }
        let radius = cce_ui::colors::backplate_corner_radius();
        let frame = Rect { x: 0.0, y: 0.0, width: w, height: h };
        pc.plate(frame, (radius, radius, radius, radius), plate, cce_ui::layout::bevel_width());

        let (cols, rows) = self.grid_for(w, h);
        let bounds = [self.pad, 0.0, w - self.pad, h];
        let visible = self.screen.visible(rows as usize, self.scroll_offset);
        for (i, line) in visible.iter().enumerate() {
            if line.is_empty() {
                continue;
            }
            let text: String = line.iter().take(cols as usize).collect();
            pc.text_with(
                text,
                self.pad,
                self.pad + i as f32 * LINE_H,
                FONT_SIZE,
                FG,
                Some(self.font.clone()),
                Some(bounds),
            );
        }

        // Block cursor, only while viewing the live bottom of the scrollback.
        if self.scroll_offset == 0 && !visible.is_empty() {
            let row = visible.len() - 1;
            let cx = self.pad + self.screen.cursor_col.min(cols as usize) as f32 * CELL_W;
            let cy = self.pad + row as f32 * LINE_H;
            pc.quad(Rect { x: cx, y: cy, width: CELL_W, height: LINE_H }, CURSOR);
        }

        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
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
        let lines = (delta.notches_y() * 3.0).round() as i64;
        let max = self.screen.line_count().saturating_sub(1);
        let next = (self.scroll_offset as i64 + lines).clamp(0, max as i64) as usize;
        if next != self.scroll_offset {
            self.scroll_offset = next;
            *needs_rebuild = true;
        }
    }

    fn handle_key_input(&mut self, event: &KeyEvent, needs_rebuild: &mut bool) -> Option<Self::Message> {
        if event.state != ElementState::Pressed {
            return None;
        }
        if let Some(bytes) = encode_key(event) {
            self.write_pty(&bytes);
            if self.scroll_offset != 0 {
                self.scroll_offset = 0;
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
