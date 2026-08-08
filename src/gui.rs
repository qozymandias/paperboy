//! GUI front-end (`--gui`): the terminal UI, in a window.
//!
//! There is deliberately no second user interface here. `RataguiBackend` is a
//! real ratatui [`Backend`] that rasterises the terminal buffer into an image,
//! so the window shows the byte-identical output of [`crate::tui::draw::draw`]
//! driven by the same [`TuiApp`] state - same panels, same layout, same widgets,
//! same theme colours, same i18n. Adding a widget here would immediately drift
//! from the TUI, so don't: everything visible is drawn by `tui/draw.rs`.
//!
//! Only the *transport* differs. egui delivers winit events; `TuiApp` speaks
//! crossterm. This module is that translation plus the frame loop, and nothing
//! else. Because the translation lands on the same `on_key`/`on_mouse` entry
//! points the terminal uses, every keybinding, overlay, wizard and mouse
//! selection behaves exactly as it does in the terminal, with no duplicated
//! input handling to keep in sync.

use eframe::egui;
use egui_ratatui::RataguiBackend;
use ratatui::Terminal;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::style::Color;
use soft_ratatui::embedded_graphics_unicodefonts::{mono_9x18_atlas, mono_9x18_bold_atlas};
use soft_ratatui::{EmbeddedGraphics, SoftBackend};
use std::time::Duration;

use crate::tui::app::TuiApp;
use crate::tui::draw::draw;

/// How often to wake up when no input arrives. Matches the terminal loop's
/// `event::poll` timeout so background work (secret resolution, in-flight
/// requests, git operations, report runs) is applied - and a held selection
/// drag keeps auto-scrolling - at the same cadence in both front-ends.
const TICK: Duration = Duration::from_millis(120);

/// Entry point: open the window and run until the app asks to quit.
pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([480.0, 320.0])
            .with_title("PaperBoy"),
        ..Default::default()
    };
    eframe::run_native(
        "PaperBoy",
        options,
        Box::new(|_cc| Ok(Box::new(GuiApp::new()))),
    )
}

struct GuiApp {
    terminal: Terminal<RataguiBackend<EmbeddedGraphics>>,
    app: TuiApp,
    /// Rect the terminal image occupied last frame, used to convert egui's
    /// pixel coordinates into the cell coordinates `on_mouse` expects. `None`
    /// until the first frame has been laid out, so the very first mouse event
    /// (before anything is on screen) is simply dropped.
    image_rect: Option<egui::Rect>,
    /// Whether the left button is currently held, so a plain `PointerMoved`
    /// can be reported as a `Drag` - egui has no drag event of its own, and
    /// the TUI's text selection is driven entirely by `Drag(Left)`.
    dragging: bool,
    /// Last cell a drag reported, so a mouse moving within one cell doesn't
    /// re-send an identical `Drag` (and re-run the selection recompute) on
    /// every frame.
    last_drag_cell: Option<(u16, u16)>,
}

impl GuiApp {
    fn new() -> Self {
        // A fixed-cell bitmap font, so a cell is an exact integer pixel box and
        // the grid lines up the way box-drawing glyphs assume. The 9x18 X11
        // fixed face is used because it covers the box-drawing, block and shade
        // characters `tui/draw.rs` builds its borders, gauges and scrollbars
        // from. It has no italic companion, so the one italic hint in the
        // request wizard renders upright here - a cosmetic difference, and the
        // only one.
        let soft = SoftBackend::<EmbeddedGraphics>::new(
            80,
            24,
            mono_9x18_atlas(),
            Some(mono_9x18_bold_atlas()),
            None,
        );
        let mut app = TuiApp::restored();
        // egui reports modifiers on every key, so Ctrl+Enter always arrives
        // distinct from a bare Enter. In the terminal that depends on the
        // keyboard-enhancement protocol being available; in a window it just is.
        app.enhanced_keys = true;
        Self {
            terminal: Terminal::new(RataguiBackend::new("paperboy", soft))
                .expect("RataguiBackend is infallible"),
            app,
            image_rect: None,
            dragging: false,
            last_drag_cell: None,
        }
    }

    /// Convert an egui position to a terminal cell.
    fn cell(&self, pos: egui::Pos2) -> Option<(u16, u16)> {
        let backend = self.terminal.backend();
        cell_at(
            pos,
            self.image_rect?,
            backend.soft_backend.char_width,
            backend.soft_backend.char_height,
            self.terminal.size().ok()?,
        )
    }

    /// Translate this frame's egui events into crossterm events and feed them
    /// to the same `on_key` / `on_mouse` the terminal front-end uses.
    fn handle_input(&mut self, ctx: &egui::Context) {
        let (events, pointer) = ctx.input(|i| {
            (
                i.events.clone(),
                i.pointer.latest_pos().or(i.pointer.interact_pos()),
            )
        });

        for event in events {
            match event {
                // A printable character. egui has already applied the keyboard
                // layout, dead keys and Shift, so this is the only reliable
                // source of text - the `Key` variant below reports physical-ish
                // keys and would mangle non-US layouts. Combos that produce no
                // text (Ctrl+C, Alt+F5) are left to the `Key` arm.
                egui::Event::Text(text) => {
                    for ch in text.chars() {
                        self.key(KeyCode::Char(ch), KeyModifiers::NONE);
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let mods = to_key_modifiers(modifiers);
                    // Shift+Tab is its own key code in crossterm, and the TUI
                    // matches on it to walk focus backwards.
                    if key == egui::Key::Tab && modifiers.shift {
                        self.key(KeyCode::BackTab, mods);
                    } else if let Some(code) = to_key_code(key) {
                        self.key(code, mods);
                    } else if modifiers.ctrl || modifiers.alt || modifiers.mac_cmd {
                        // A text key held with a modifier produces no `Text`
                        // event, so recover the character here. Lowercase to
                        // match what a terminal reports for Ctrl/Alt combos.
                        if let Some(ch) = printable_key_char(key) {
                            self.key(KeyCode::Char(ch), mods);
                        }
                    }
                }
                // The terminal receives a paste as a burst of individual key
                // presses (bracketed paste is not enabled), and every editor in
                // the TUI is built around that. Replaying it the same way keeps
                // one code path instead of adding a paste path to each editor.
                egui::Event::Paste(text) => {
                    for ch in text.chars() {
                        match ch {
                            '\n' => self.key(KeyCode::Enter, KeyModifiers::NONE),
                            // Lone carriage returns from CRLF text would insert
                            // a second, empty line.
                            '\r' => {}
                            _ => self.key(KeyCode::Char(ch), KeyModifiers::NONE),
                        }
                    }
                }
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    let Some(btn) = to_mouse_button(button) else {
                        continue;
                    };
                    let Some(cell) = self.cell(pos) else { continue };
                    if button == egui::PointerButton::Primary {
                        self.dragging = pressed;
                        self.last_drag_cell = pressed.then_some(cell);
                    }
                    let kind = if pressed {
                        MouseEventKind::Down(btn)
                    } else {
                        MouseEventKind::Up(btn)
                    };
                    self.mouse(kind, cell, to_key_modifiers(modifiers));
                }
                egui::Event::PointerMoved(pos) => {
                    if !self.dragging {
                        continue;
                    }
                    let Some(cell) = self.cell(pos) else { continue };
                    if self.last_drag_cell == Some(cell) {
                        continue;
                    }
                    self.last_drag_cell = Some(cell);
                    self.mouse(
                        MouseEventKind::Drag(MouseButton::Left),
                        cell,
                        KeyModifiers::NONE,
                    );
                }
                // The pointer left the window (or the button was released
                // outside it): end the drag so a later move doesn't extend a
                // selection the user has already let go of.
                egui::Event::PointerGone => {
                    self.dragging = false;
                    self.last_drag_cell = None;
                }
                egui::Event::MouseWheel {
                    delta, modifiers, ..
                } => {
                    if delta.y == 0.0 {
                        continue;
                    }
                    // egui reports a positive y for scrolling up (content moving
                    // down), which is crossterm's ScrollUp.
                    let kind = if delta.y > 0.0 {
                        MouseEventKind::ScrollUp
                    } else {
                        MouseEventKind::ScrollDown
                    };
                    // A wheel event carries no position of its own, so it is
                    // attributed to wherever the pointer currently is - the TUI
                    // scrolls the panel under the cursor.
                    let Some(cell) = pointer.and_then(|p| self.cell(p)) else {
                        continue;
                    };
                    self.mouse(kind, cell, to_key_modifiers(modifiers));
                }
                _ => {}
            }
        }
    }

    /// Rendered pixel size of the whole grid, used to detect that the widget
    /// resized the terminal during layout.
    fn pixmap_size(&self) -> (usize, usize) {
        let b = &self.terminal.backend().soft_backend;
        (b.get_pixmap_width(), b.get_pixmap_height())
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.app.on_key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
    }

    fn mouse(&mut self, kind: MouseEventKind, (column, row): (u16, u16), modifiers: KeyModifiers) {
        self.app.on_mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers,
        });
    }
}

impl eframe::App for GuiApp {
    /// Everything that isn't painting: input, background-work polling, and
    /// rendering the ratatui frame into the backend's pixel buffer. eframe
    /// calls this before [`Self::ui`], and forbids painting from it.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_input(ctx);

        // Same set, and same order, as the terminal loop: every one of these
        // applies the result of work running off the UI thread.
        self.app.poll_env_updates();
        self.app.poll_capture_updates();
        self.app.poll_git_updates();
        self.app.poll_workspace_redownload_updates();
        self.app.poll_git_save_updates();
        self.app.poll_batch_run_updates();
        self.app.poll_report_run_updates();
        // Keep a selection dragged past its panel's edge scrolling even while
        // the mouse itself is still (no new Drag event to drive it).
        if self.app.has_pending_autoscroll() {
            self.app.autoscroll_tick();
        }

        if self.app.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        let app = &mut self.app;
        self.terminal
            .draw(|f| draw(f, app))
            .expect("RataguiBackend is infallible");
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let before = self.pixmap_size();
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme_bg(&self.app)))
            .show_inside(ui, |ui| {
                self.image_rect = Some(ui.add(self.terminal.backend_mut()).rect);
            });
        // The widget resizes the grid to the window during layout, i.e. *after*
        // the frame was drawn at the old size. Repaint straight away so a
        // resize settles on the next frame instead of waiting out the tick.
        if self.pixmap_size() != before {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(TICK);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Covers both quitting from inside the app and closing the window, so
        // tabs, collections and environments survive either way.
        self.app.save_state();
    }
}

/// Window background behind the terminal image. The grid is a whole number of
/// cells, so up to one cell of slack can remain at the right and bottom edges;
/// filling it with the theme's own background keeps that invisible instead of
/// showing a strip of egui grey.
fn theme_bg(app: &TuiApp) -> egui::Color32 {
    match app.theme().bg {
        Color::Rgb(r, g, b) => egui::Color32::from_rgb(r, g, b),
        _ => egui::Color32::BLACK,
    }
}

/// Pixel position (relative to the whole window) to a cell in the grid drawn
/// at `rect`, clamped into the grid. Clamping - rather than discarding an
/// out-of-bounds point - mirrors the terminal loop, which clamps every mouse
/// point to the terminal size once so the rest of the app can assume a point is
/// on screen: a selection dragged past the window edge must still select to the
/// edge instead of freezing.
fn cell_at(
    pos: egui::Pos2,
    rect: egui::Rect,
    char_width: usize,
    char_height: usize,
    size: ratatui::layout::Size,
) -> Option<(u16, u16)> {
    if char_width == 0 || char_height == 0 || size.width == 0 || size.height == 0 {
        return None;
    }
    let col = ((pos.x - rect.min.x) / char_width as f32).floor();
    let row = ((pos.y - rect.min.y) / char_height as f32).floor();
    // `as u16` is a saturating cast, so a NaN coordinate (which `clamp` passes
    // through) lands on 0 rather than being undefined.
    let col = col.clamp(0.0, (size.width - 1) as f32) as u16;
    let row = row.clamp(0.0, (size.height - 1) as f32) as u16;
    Some((col, row))
}

fn to_key_modifiers(m: egui::Modifiers) -> KeyModifiers {
    let mut out = KeyModifiers::NONE;
    out.set(KeyModifiers::SHIFT, m.shift);
    // macOS Command maps to Control so the Ctrl-based shortcuts the TUI
    // documents work with the modifier macOS users actually reach for.
    out.set(KeyModifiers::CONTROL, m.ctrl || m.mac_cmd);
    out.set(KeyModifiers::ALT, m.alt);
    out
}

fn to_mouse_button(b: egui::PointerButton) -> Option<MouseButton> {
    match b {
        egui::PointerButton::Primary => Some(MouseButton::Left),
        egui::PointerButton::Secondary => Some(MouseButton::Right),
        egui::PointerButton::Middle => Some(MouseButton::Middle),
        _ => None,
    }
}

/// Non-text keys, i.e. exactly the [`KeyCode`]s the TUI matches on besides
/// `Char`. Anything not listed is either a printable character (handled via
/// `Event::Text`, or `printable_key_char` when a modifier suppresses it) or a
/// key the app has no binding for.
fn to_key_code(key: egui::Key) -> Option<KeyCode> {
    use egui::Key as K;
    Some(match key {
        K::Enter => KeyCode::Enter,
        K::Tab => KeyCode::Tab,
        K::Escape => KeyCode::Esc,
        K::Backspace => KeyCode::Backspace,
        K::Delete => KeyCode::Delete,
        K::Insert => KeyCode::Insert,
        K::Home => KeyCode::Home,
        K::End => KeyCode::End,
        K::PageUp => KeyCode::PageUp,
        K::PageDown => KeyCode::PageDown,
        K::ArrowUp => KeyCode::Up,
        K::ArrowDown => KeyCode::Down,
        K::ArrowLeft => KeyCode::Left,
        K::ArrowRight => KeyCode::Right,
        K::F1 => KeyCode::F(1),
        K::F2 => KeyCode::F(2),
        K::F3 => KeyCode::F(3),
        K::F4 => KeyCode::F(4),
        K::F5 => KeyCode::F(5),
        K::F6 => KeyCode::F(6),
        K::F7 => KeyCode::F(7),
        K::F8 => KeyCode::F(8),
        K::F9 => KeyCode::F(9),
        K::F10 => KeyCode::F(10),
        K::F11 => KeyCode::F(11),
        K::F12 => KeyCode::F(12),
        _ => return None,
    })
}

/// The character a text key stands for, used only to rebuild modifier combos
/// (Ctrl+S, Alt+F5) that produce no `Event::Text`. Lowercase, matching what a
/// terminal reports for such combos.
fn printable_key_char(key: egui::Key) -> Option<char> {
    use egui::Key as K;
    Some(match key {
        K::Space => ' ',
        K::Comma => ',',
        K::Period => '.',
        K::Semicolon => ';',
        K::Colon => ':',
        K::Slash => '/',
        K::Backslash => '\\',
        K::Pipe => '|',
        K::Minus => '-',
        K::Plus => '+',
        K::Equals => '=',
        K::Questionmark => '?',
        K::Exclamationmark => '!',
        K::Quote => '\'',
        K::Backtick => '`',
        K::OpenBracket => '[',
        K::CloseBracket => ']',
        K::OpenCurlyBracket => '{',
        K::CloseCurlyBracket => '}',
        _ => {
            // `Key::A`..`Key::Z` and `Key::Num0`..`Key::Num9` render as their
            // own name, so the single-character ones are exactly the letters
            // and digits.
            let name = key.name();
            let mut chars = name.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            ch.to_ascii_lowercase()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(90.0, 90.0))
    }

    fn size(width: u16, height: u16) -> ratatui::layout::Size {
        ratatui::layout::Size { width, height }
    }

    #[test]
    fn cell_at_maps_pixels_to_cells_relative_to_the_image_origin() {
        // The image does not start at the window origin, so the rect's offset
        // must be subtracted before dividing by the cell size.
        assert_eq!(
            cell_at(egui::pos2(10.0, 20.0), rect(), 9, 18, size(10, 5)),
            Some((0, 0))
        );
        assert_eq!(
            cell_at(
                egui::pos2(10.0 + 26.0, 20.0 + 37.0),
                rect(),
                9,
                18,
                size(10, 5)
            ),
            Some((2, 2))
        );
    }

    #[test]
    fn cell_at_clamps_points_outside_the_grid() {
        // A drag past the window edge must still resolve to the edge cell
        // rather than being dropped, or a selection would stop tracking.
        assert_eq!(
            cell_at(egui::pos2(-500.0, -500.0), rect(), 9, 18, size(10, 5)),
            Some((0, 0))
        );
        assert_eq!(
            cell_at(egui::pos2(5000.0, 5000.0), rect(), 9, 18, size(10, 5)),
            Some((9, 4))
        );
    }

    #[test]
    fn cell_at_rejects_a_degenerate_grid() {
        assert_eq!(
            cell_at(egui::pos2(0.0, 0.0), rect(), 0, 18, size(10, 5)),
            None
        );
        assert_eq!(
            cell_at(egui::pos2(0.0, 0.0), rect(), 9, 18, size(0, 0)),
            None
        );
    }

    #[test]
    fn modifiers_map_to_crossterm_including_mac_command_as_control() {
        assert_eq!(to_key_modifiers(egui::Modifiers::NONE), KeyModifiers::NONE);
        assert_eq!(
            to_key_modifiers(egui::Modifiers::CTRL),
            KeyModifiers::CONTROL
        );
        assert_eq!(to_key_modifiers(egui::Modifiers::ALT), KeyModifiers::ALT);
        assert_eq!(
            to_key_modifiers(egui::Modifiers::SHIFT),
            KeyModifiers::SHIFT
        );
        assert_eq!(
            to_key_modifiers(egui::Modifiers::MAC_CMD),
            KeyModifiers::CONTROL
        );
    }

    #[test]
    fn every_non_char_key_the_tui_binds_is_translated() {
        // Guards the mapping against the TUI gaining a binding the GUI cannot
        // deliver: these are the exact `KeyCode`s `tui/input.rs` matches on
        // besides `Char`.
        let expected = [
            (egui::Key::Enter, KeyCode::Enter),
            (egui::Key::Tab, KeyCode::Tab),
            (egui::Key::Escape, KeyCode::Esc),
            (egui::Key::Backspace, KeyCode::Backspace),
            (egui::Key::Delete, KeyCode::Delete),
            (egui::Key::Insert, KeyCode::Insert),
            (egui::Key::Home, KeyCode::Home),
            (egui::Key::End, KeyCode::End),
            (egui::Key::PageUp, KeyCode::PageUp),
            (egui::Key::PageDown, KeyCode::PageDown),
            (egui::Key::ArrowUp, KeyCode::Up),
            (egui::Key::ArrowDown, KeyCode::Down),
            (egui::Key::ArrowLeft, KeyCode::Left),
            (egui::Key::ArrowRight, KeyCode::Right),
            (egui::Key::F1, KeyCode::F(1)),
            (egui::Key::F5, KeyCode::F(5)),
            (egui::Key::F12, KeyCode::F(12)),
        ];
        for (key, code) in expected {
            assert_eq!(to_key_code(key), Some(code), "{key:?}");
        }
        // Letters are not here: they arrive as text, so translating them as
        // key codes would double every keystroke.
        assert_eq!(to_key_code(egui::Key::A), None);
    }

    #[test]
    fn the_real_tui_renders_through_the_gui_backend() {
        // The whole premise of this front-end: `tui::draw::draw` runs unchanged
        // against the GUI's backend and rasterises to actual glyphs. If the
        // font atlas ever lost coverage of the box-drawing characters the TUI
        // builds its borders from, the buffer would come out blank.
        let soft = SoftBackend::<EmbeddedGraphics>::new(
            100,
            30,
            mono_9x18_atlas(),
            Some(mono_9x18_bold_atlas()),
            None,
        );
        let mut terminal = Terminal::new(RataguiBackend::new("test", soft)).unwrap();
        let mut app = TuiApp::default();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let soft = &terminal.backend().soft_backend;
        assert_eq!(soft.get_pixmap_width(), 100 * soft.char_width);
        assert_eq!(soft.get_pixmap_height(), 30 * soft.char_height);
        // More than one distinct pixel value means glyphs (and theme colours)
        // were actually rasterised rather than a flat fill being emitted.
        let pixels = soft.get_pixmap_data();
        let first = &pixels[..3];
        assert!(
            pixels.chunks_exact(3).any(|px| px != first),
            "the rendered terminal is a flat fill, so nothing was drawn"
        );
    }

    #[test]
    fn modifier_combos_recover_the_character_that_produced_no_text() {
        // Ctrl+S and Alt+letter emit no `Event::Text`, so the character has to
        // come back from the key itself - lowercased, as a terminal reports it.
        assert_eq!(printable_key_char(egui::Key::S), Some('s'));
        assert_eq!(printable_key_char(egui::Key::Num0), Some('0'));
        assert_eq!(printable_key_char(egui::Key::Space), Some(' '));
        assert_eq!(printable_key_char(egui::Key::OpenBracket), Some('['));
        assert_eq!(printable_key_char(egui::Key::CloseBracket), Some(']'));
        assert_eq!(printable_key_char(egui::Key::Enter), None);
    }
}
