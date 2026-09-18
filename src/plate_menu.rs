//! The root plate's corner control: the DE's circular menu trigger (the same
//! affordance cce-designer's panes and cce-files' preview pane carry, on
//! `cce_ui::widget::plate_dock`) riding the top-right of the terminal
//! window, and the menu it opens.
//!
//! The terminal has one plate — the window itself — so the toolkit's dock
//! vocabulary (collapse, detach) does not apply; the rows are the actions a
//! terminal without a menubar has nowhere else to put: the clipboard pair,
//! text zoom, scrollback/state resets, the tabs, and a new window. The tabs
//! borrow the designer's dock-tab language: every tab listed as a radio row
//! (the shown one marked, so the list reads as state), then New Tab and
//! Close Tab. Geometry is the toolkit's (`corner_center` on the window
//! rect), the menu is the shared `context_menu`, and the rows are dispatched
//! here — the same `plate_menu_actions` + `handle_plate_menu_click` contract
//! as the designer.

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::vte::ansi::Handler;
use cce_ui::widget::plate_dock::{self, CORNER_R};
use cce_ui::widget::{context_menu, ElementState, MouseButton, WidgetId};

use crate::{TabId, TerminalApp};

/// What the corner menu can do to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlateMenuAction {
    /// Selection → the regular clipboard (the Ctrl+Shift+C path).
    Copy,
    /// Regular clipboard → the pty (the Ctrl+Shift+V path).
    Paste,
    /// Text zoom, one point per step over the configured size.
    LargerText,
    SmallerText,
    /// Back to the configured size.
    ResetTextSize,
    /// Drop the scrollback history (the viewport stays).
    ClearScrollback,
    /// Full VT reset: modes, colors, tab stops, the alternate screen.
    ResetTerminal,
    /// Bring the named tab to the front. By id, not index: a tab whose
    /// shell exits while the menu is open shifts the indices under it.
    ShowTab(TabId),
    /// A fresh shell in the active tab's directory, shown.
    NewTab,
    /// Close the active tab (its shell is killed).
    CloseTab,
    /// Another cce-terminal, detached.
    NewWindow,
    /// A "-" row: engraved, inert — keeps the actions aligned with the option
    /// rows so a click on the line dispatches nothing.
    Separator,
}

/// The menu has no widget target — the terminal has no widget tree and
/// dispatches its own rows — so the shared menu's built-in action routing
/// (`mouse_input` with a `UiContext`) is never used; the id is a placeholder.
const NO_TARGET: WidgetId = WidgetId(0);

impl TerminalApp {
    /// Centre of the corner control, or `None` while the window is too small
    /// to carry one.
    pub(crate) fn plate_corner_center(&self) -> Option<(f32, f32)> {
        plate_dock::corner_center((0.0, 0.0, self.win_w, self.win_h), false)
    }

    pub(crate) fn plate_corner_hit(&self, px: f32, py: f32) -> bool {
        self.plate_corner_center().is_some_and(|c| plate_dock::corner_hit(c, px, py))
    }

    pub(crate) fn plate_menu_open(&self) -> bool {
        context_menu::is_visible() && !self.plate_menu_actions.is_empty()
    }

    /// Open the corner menu under its control. Rows are contextual: Copy
    /// only with a selection to copy, Reset Text Size only while zoomed.
    /// The menu hangs off the control's RIGHT edge, leftwards — anchored on
    /// the left as the designer's panes do, it would run off the window.
    pub(crate) fn open_plate_menu(&mut self) {
        let Some((cx, cy)) = self.plate_corner_center() else { return };
        let mut options: Vec<String> = Vec::new();
        let mut actions: Vec<PlateMenuAction> = Vec::new();
        let mut row = |label: &str, action: PlateMenuAction| {
            options.push(label.to_string());
            actions.push(action);
        };

        if self.tab().term.selection_to_string().is_some_and(|s| !s.is_empty()) {
            row("Copy", PlateMenuAction::Copy);
        }
        row("Paste", PlateMenuAction::Paste);
        row("-", PlateMenuAction::Separator);
        row("Larger Text", PlateMenuAction::LargerText);
        row("Smaller Text", PlateMenuAction::SmallerText);
        if self.zoom_steps != 0 {
            row("Reset Text Size", PlateMenuAction::ResetTextSize);
        }
        row("-", PlateMenuAction::Separator);
        row("Clear Scrollback", PlateMenuAction::ClearScrollback);
        row("Reset Terminal", PlateMenuAction::ResetTerminal);
        row("-", PlateMenuAction::Separator);
        // The tabs as a RADIO group: every one listed, the shown one marked.
        // Clicking the marked row is a no-op (show_tab declines the active
        // index), so the list reads as state, not just as actions.
        for (i, tab) in self.tabs.iter().enumerate() {
            let mark = if i == self.active { "●" } else { "○" };
            row(&format!("{mark} {}", tab.label(i)), PlateMenuAction::ShowTab(tab.id));
        }
        row("New Tab", PlateMenuAction::NewTab);
        if self.tabs.len() > 1 {
            row("Close Tab", PlateMenuAction::CloseTab);
        }
        row("-", PlateMenuAction::Separator);
        row("New Window", PlateMenuAction::NewWindow);

        // The menu sizes itself from its labels on `show`, so place it once
        // to learn the width, then again with its right edge on the control.
        let top = cy + CORNER_R;
        context_menu::show(0.0, top, options.clone(), 0, NO_TARGET);
        let left = (cx + CORNER_R - context_menu::w()).max(0.0);
        context_menu::show(left, top, options, 0, NO_TARGET);
        self.plate_menu_actions = actions;
    }

    pub(crate) fn close_plate_menu(&mut self) {
        context_menu::hide();
        self.plate_menu_actions.clear();
    }

    /// Route a button event while the corner menu is open: a left press on a
    /// row dispatches it, any other press dismisses, and releases are eaten
    /// so the press that opened the menu never completes as a click on the
    /// grid underneath. `true` when the event was the menu's.
    pub(crate) fn handle_plate_menu_input(
        &mut self,
        button: MouseButton,
        state: ElementState,
        px: f32,
        py: f32,
    ) -> bool {
        if !self.plate_menu_open() {
            return false;
        }
        if state != ElementState::Pressed {
            return true;
        }
        let picked = if button == MouseButton::Left && context_menu::hit_test(px, py) {
            let row = ((py - context_menu::y()) / context_menu::ROW_H).floor() as usize;
            self.plate_menu_actions.get(row).copied()
        } else {
            None
        };
        self.close_plate_menu();
        if let Some(action) = picked {
            self.dispatch_plate_menu(action);
        }
        true
    }

    fn dispatch_plate_menu(&mut self, action: PlateMenuAction) {
        match action {
            PlateMenuAction::Copy => {
                if let Some(text) = self.tab().term.selection_to_string() {
                    if !text.is_empty() {
                        cce_ui::widget::clipboard::copy_to_clipboard(&text);
                    }
                }
            }
            PlateMenuAction::Paste => {
                if let Some(text) = cce_ui::widget::clipboard::read_from_clipboard() {
                    if !text.is_empty() {
                        self.paste(&text);
                    }
                }
            }
            PlateMenuAction::LargerText => self.set_zoom(self.zoom_steps + 1),
            PlateMenuAction::SmallerText => self.set_zoom(self.zoom_steps - 1),
            PlateMenuAction::ResetTextSize => self.set_zoom(0),
            PlateMenuAction::ClearScrollback => {
                // Drop the view to the live screen first: a display offset
                // into history that no longer exists is not a state the grid
                // guards against.
                let term = &mut self.tab_mut().term;
                term.scroll_display(Scroll::Bottom);
                term.grid_mut().clear_history();
                self.sync_scroll_motion();
            }
            PlateMenuAction::ResetTerminal => {
                let term = &mut self.tab_mut().term;
                term.selection = None;
                term.reset_state();
                self.sync_scroll_motion();
            }
            PlateMenuAction::ShowTab(id) => self.show_tab_by_id(id),
            PlateMenuAction::NewTab => self.new_tab(),
            PlateMenuAction::CloseTab => self.close_active_tab(),
            PlateMenuAction::NewWindow => match std::env::current_exe() {
                Ok(exe) => {
                    if let Err(e) = cce_ui::process::spawn_detached(std::process::Command::new(exe)) {
                        log::warn!("cce-terminal: failed to spawn a new window: {e}");
                    }
                }
                Err(e) => log::warn!("cce-terminal: cannot locate own executable: {e}"),
            },
            PlateMenuAction::Separator => {}
        }
    }

    /// Text zoom: re-derive the settings at the new step and reflow the grid,
    /// the same path a config edit takes.
    fn set_zoom(&mut self, steps: i32) {
        self.zoom_steps = steps.clamp(-8, 24);
        let font = self.font.clone();
        let reloaded = crate::Settings::load(&font, self.zoom_steps);
        self.apply_settings(reloaded);
    }

    /// Draw the corner control (emphasized while hovered or open) and, over
    /// everything, the open menu. Called last in `display_list`.
    pub(crate) fn paint_plate_menu(&self, pc: &mut cce_ui::scene::paint::PaintCtx) {
        if let Some(c) = self.plate_corner_center() {
            let emphasized = self.plate_menu_open()
                || plate_dock::corner_hit(c, self.last_pointer.x as f32, self.last_pointer.y as f32);
            plate_dock::draw_corner_dot(pc, c, emphasized);
        }
        if !self.plate_menu_open() {
            return;
        }
        // The shared menu paints as the DE's lit plate; its labels carry
        // bounds equal to the menu rect, which the engine's text-occlusion
        // clamp exempts, so they render inside the menu while the grid's
        // text beneath stays clamped.
        // Labels come with the plate: a TextLabel carries no family, so the
        // hand-rolled label loop that used to live here passed None and drew
        // the menu in the default sans rather than the DE's menu font.
        context_menu::paint_with_labels(pc);
    }
}
