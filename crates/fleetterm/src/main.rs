//! FleetTerm UI — Phase 2 / 3 / 5 / 6 fleet cockpit.
//!
//! Connects to `fleetd` over a unix socket (via `client::FleetClient`), renders all agent
//! sessions in a sidebar, lets the user approve/deny decisions and cycle autonomy, and
//! shows the focused session's terminal output in the main pane.
//!
//! P5 additions:
//! - View modes: Split (default) | Tiled | Focus — toggled via title-bar chips.
//! - Cmd-K / Ctrl-K command palette.
//!
//! P6 additions:
//! - Fleet Composer bar: a focusable input row above the status line. Type a command,
//!   prefix with `@all`, `@working`, or `@<name>` to fan-out; Enter sends it as PTY input.
//! - Palette now typeable: key strokes accumulate into `palette_query` and filter sessions.
//! - Command-block indicator: OSC 133 events are tallied per session and shown in the
//!   terminal pane header (command count, last exit code, running-flag dot).
//!
//! API verified against gpui 0.2.2 (new App/Window/Context surface).

mod client;
mod terminal;

use std::collections::{BTreeMap, HashMap};

use async_channel::Sender;
use gpui::{
    KeyBinding, actions, div, prelude::*, px, relative, rgb, size, App, Application, Bounds,
    Context, FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent, SharedString,
    TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions,
};
use protocol::{Autonomy, Event, Request, Session, SessionId, SpawnSpec, State, Target, Tool};

// ── P5 actions ───────────────────────────────────────────────────────────────
actions!(fleetterm, [TogglePalette]);

// ── v2 colour palette (matches the Phase-0 spike) ────────────────────────────
#[allow(dead_code)]
mod color {
    pub const BG: u32 = 0x16161e;
    pub const TERM: u32 = 0x1a1b26;
    pub const SIDE: u32 = 0x14151e;
    pub const HEAD: u32 = 0x11121a;
    pub const BORDER: u32 = 0x262838;
    pub const TEXT: u32 = 0xc0caf5;
    pub const DIM: u32 = 0x9aa5ce;
    pub const MUT: u32 = 0x565f89;
    pub const BLUE: u32 = 0x7aa2f7;
    pub const GREEN: u32 = 0x9ece6a;
    /// Violet — used in terminal VT escape rendering (future).
    pub const VIO: u32 = 0xbb9af7;
    pub const AMBER: u32 = 0xf5a623;
    pub const RED: u32 = 0xf7768e;
    /// Palette overlay background (slightly lighter than HEAD).
    pub const PALETTE_BG: u32 = 0x1e2030;
}

// ── terminal output cap ───────────────────────────────────────────────────────
const TERM_MAX_BYTES: usize = 40 * 1024; // 40 KB
const TERM_MAX_LINES: usize = 400;

/// Maximum session tiles shown in Tiled mode before truncation notice.
const TILED_MAX: usize = 6;

// ── P6: command-block tracking ────────────────────────────────────────────────

/// Accumulated OSC 133 statistics for one session.
/// `running` is true between a `CommandStart` / `OutputStart` and the matching `CommandEnd`.
#[derive(Default, Clone)]
struct BlockStats {
    /// How many commands have been run in this session (counts each `CommandStart`).
    commands: u32,
    /// Exit code from the most recent `CommandEnd`, or `None` if none seen yet.
    last_exit: Option<i32>,
    /// True while a command is executing (between OutputStart and CommandEnd).
    running: bool,
}

// ── P5 view mode ─────────────────────────────────────────────────────────────

/// Which layout the main body uses.
#[derive(Clone, Copy, PartialEq)]
enum ViewMode {
    /// Split: focused terminal pane on the left + session sidebar on the right (default).
    Split,
    /// Tiled: grid of session tiles, each showing a mini terminal.
    Tiled,
    /// Focus: terminal pane only — sidebar hidden.
    Focus,
}

// ── application state ────────────────────────────────────────────────────────

struct FleetTermApp {
    /// All known sessions, keyed by id (BTreeMap keeps stable insertion order by id).
    sessions: BTreeMap<SessionId, Session>,
    /// Terminal output buffer per session (raw UTF-8, lossy).
    term_text: HashMap<SessionId, String>,
    /// Styled cell-grid snapshots from `Event::Grid`, one per session.
    grids: HashMap<SessionId, terminal::GridState>,
    /// Which session's output is shown in the left pane.
    focused: Option<SessionId>,
    /// Fleet-wide default autonomy for new sessions.
    default_autonomy: Autonomy,
    /// Running total cost reported by the daemon.
    total_cost: f64,
    /// Channel for sending requests to the daemon.
    requests: Sender<Request>,
    /// GPUI focus handle — the terminal pane tracks this to receive keystrokes.
    focus_handle: FocusHandle,

    // ── P5 fields ─────────────────────────────────────────────────────────────
    /// Current view layout mode.
    view_mode: ViewMode,
    /// Whether the Cmd-K command palette overlay is open.
    palette_open: bool,
    /// Search query typed in the palette — now wired (P6): filters session jump rows.
    palette_query: String,

    // ── P6 fields ─────────────────────────────────────────────────────────────
    /// OSC 133 block statistics per session.
    blocks: HashMap<SessionId, BlockStats>,
    /// The current text in the fleet composer input bar.
    composer: String,
    /// GPUI focus handle for the composer bar (separate from the terminal handle).
    composer_focus: FocusHandle,

    // ── Bug-fix: PTY resize tracking ──────────────────────────────────────────
    /// Last (session, cols, rows) sent via Request::Resize, to avoid redundant sends.
    last_sent_size: Option<(SessionId, u16, u16)>,
}

impl FleetTermApp {
    fn new(cx: &mut Context<Self>) -> Self {
        let fleet = client::FleetClient::connect(None);
        let requests = fleet.requests.clone();
        let events = fleet.events;

        // Pump daemon events on the GPUI foreground executor.
        // Signature: cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| { ... })
        cx.spawn(async move |this: WeakEntity<FleetTermApp>, cx: &mut gpui::AsyncApp| {
            while let Ok(ev) = events.recv().await {
                let _ = this.update(cx, |state, cx| {
                    state.apply(ev);
                    cx.notify();
                });
            }
        })
        .detach();

        FleetTermApp {
            sessions: BTreeMap::new(),
            term_text: HashMap::new(),
            grids: HashMap::new(),
            focused: None,
            default_autonomy: Autonomy::Guarded,
            total_cost: 0.0,
            requests,
            focus_handle: cx.focus_handle(),
            view_mode: ViewMode::Split,
            palette_open: false,
            palette_query: String::new(),
            blocks: HashMap::new(),
            composer: String::new(),
            composer_focus: cx.focus_handle(),
            last_sent_size: None,
        }
    }

    // ── event handler ─────────────────────────────────────────────────────────

    fn apply(&mut self, ev: Event) {
        match ev {
            Event::Snapshot {
                sessions,
                default_autonomy,
                total_cost_usd,
            } => {
                self.default_autonomy = default_autonomy;
                self.total_cost = total_cost_usd;
                self.sessions.clear();
                for s in sessions {
                    self.sessions.insert(s.id.clone(), s);
                }
                // Focus the first session (highest priority = lowest key since BTreeMap)
                if self.focused.is_none() {
                    self.focused = self.sessions.keys().next().cloned();
                }
            }
            Event::SessionUpdate(s) => {
                self.sessions.insert(s.id.clone(), s);
                // Recompute total cost from all sessions.
                self.total_cost = self.sessions.values().map(|s| s.cost_usd).sum();
            }
            Event::SessionRemoved(id) => {
                self.sessions.remove(&id);
                if self.focused.as_ref() == Some(&id) {
                    self.focused = self.sessions.keys().next().cloned();
                }
                self.term_text.remove(&id);
                self.grids.remove(&id);
                self.blocks.remove(&id); // P6: clean up block stats.
            }
            Event::Output { session, data } => {
                let text = String::from_utf8_lossy(&data).into_owned();
                let buf = self.term_text.entry(session.clone()).or_default();
                buf.push_str(&text);
                // Cap by bytes.
                if buf.len() > TERM_MAX_BYTES {
                    let trim_at = buf.len() - TERM_MAX_BYTES;
                    // Advance to a valid UTF-8 boundary.
                    let trim_at = buf
                        .char_indices()
                        .map(|(i, _)| i)
                        .find(|&i| i >= trim_at)
                        .unwrap_or(buf.len());
                    *buf = buf[trim_at..].to_owned();
                }
                // Cap by line count.
                let line_count = buf.lines().count();
                if line_count > TERM_MAX_LINES {
                    let excess = line_count - TERM_MAX_LINES;
                    let mut skip = 0usize;
                    for _ in 0..excess {
                        if let Some(nl) = buf[skip..].find('\n') {
                            skip += nl + 1;
                        } else {
                            break;
                        }
                    }
                    *buf = buf[skip..].to_owned();
                }
                // Auto-focus first session that produces output.
                if self.focused.is_none() {
                    self.focused = Some(session);
                }
            }
            // Grid: replace the stored cell-grid for this session.  Also auto-focus
            // the session so clicking a sidebar card (which sends RequestGrid) will
            // both fetch the grid *and* switch the terminal pane to that session.
            Event::Grid {
                session,
                cols,
                rows,
                cursor_col,
                cursor_row,
                cells,
            } => {
                self.grids.insert(
                    session.clone(),
                    terminal::GridState::new(cols, rows, cursor_col, cursor_row, cells),
                );
                // Focus the session that just sent us a grid.
                self.focused = Some(session);
            }
            // DecisionPending: state already updated via SessionUpdate; sidebar shows buttons.
            Event::DecisionPending { .. } => {}
            // AutoDecision and Error: silently ignored (could append to a log in the future).
            Event::AutoDecision { .. } => {}
            Event::Error { .. } => {}
            // P6: OSC 133 block markers — update per-session BlockStats.
            Event::Block { session, marker } => {
                use protocol::BlockMarker;
                let stats = self.blocks.entry(session).or_default();
                match marker {
                    BlockMarker::CommandStart => {
                        stats.commands += 1;
                        stats.running = true;
                    }
                    BlockMarker::OutputStart => {
                        // OutputStart: command accepted, output begins; still running.
                        stats.running = true;
                    }
                    BlockMarker::CommandEnd { exit } => {
                        stats.running = false;
                        stats.last_exit = exit;
                    }
                    // PromptStart: just a prompt rendering marker — no stats change needed.
                    BlockMarker::PromptStart => {}
                }
            }
        }
    }

    // ── helpers for counts ────────────────────────────────────────────────────

    fn counts(&self) -> (usize, usize, usize) {
        let mut needs = 0usize;
        let mut working = 0usize;
        let mut done = 0usize;
        for s in self.sessions.values() {
            match &s.state {
                State::NeedsInput(_) | State::Stuck => needs += 1,
                State::Working | State::Idle => working += 1,
                State::Done | State::Dead => done += 1,
            }
        }
        (needs, working, done)
    }

    // ── keyboard handler ──────────────────────────────────────────────────────

    /// Forward a keystroke to the focused session's PTY.
    fn on_term_key_down(
        &mut self,
        ev: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ref session_id) = self.focused else {
            return;
        };
        if let Some(data) = terminal::encode_key(&ev.keystroke) {
            let tx = self.requests.clone();
            let target = Target::Session(session_id.clone());
            let _ = tx.try_send(Request::Input { target, data });
            cx.stop_propagation();
        }
    }

    // ── P6: Composer helpers ──────────────────────────────────────────────────

    /// Submit whatever is in `self.composer` as PTY input to the resolved target.
    /// Parses a leading `@all `, `@working `, or `@<name> ` token.
    /// Falls back to the focused session when no `@` prefix is present.
    fn submit_composer(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.clone();
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            return;
        }

        let target = if let Some(rest) = text.strip_prefix("@all ") {
            let data: Vec<u8> = format!("{}\r", rest).into_bytes();
            let tx = self.requests.clone();
            let _ = tx.try_send(Request::Input { target: Target::All, data });
            self.composer.clear();
            cx.notify();
            return;
        } else if let Some(rest) = text.strip_prefix("@working ") {
            let data: Vec<u8> = format!("{}\r", rest).into_bytes();
            let tx = self.requests.clone();
            let _ = tx.try_send(Request::Input { target: Target::AllWorking, data });
            self.composer.clear();
            cx.notify();
            return;
        } else if let Some(at_rest) = text.strip_prefix('@') {
            // @<name> <rest>: find a session whose name matches the token.
            if let Some(space_pos) = at_rest.find(' ') {
                let name_token = &at_rest[..space_pos];
                let rest = &at_rest[space_pos + 1..];
                let maybe_id = self
                    .sessions
                    .values()
                    .find(|s| s.name == name_token)
                    .map(|s| s.id.clone());
                if let Some(id) = maybe_id {
                    let data: Vec<u8> = format!("{}\r", rest).into_bytes();
                    let tx = self.requests.clone();
                    let _ = tx.try_send(Request::Input { target: Target::Session(id), data });
                    self.composer.clear();
                    cx.notify();
                }
                // Unknown @name — do nothing (keep composer so user can correct).
                return;
            } else {
                // Bare "@something" with no space yet — do nothing, user hasn't typed the command yet.
                return;
            }
        } else {
            // No @ prefix: send to focused session.
            if let Some(ref id) = self.focused.clone() {
                Target::Session(id.clone())
            } else {
                return;
            }
        };

        let data: Vec<u8> = format!("{}\r", text).into_bytes();
        let tx = self.requests.clone();
        let _ = tx.try_send(Request::Input { target, data });
        self.composer.clear();
        cx.notify();
    }

    /// Handle a key event directed at the composer bar.
    fn on_composer_key_down(
        &mut self,
        ev: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ctrl = ev.keystroke.modifiers.control;
        let platform = ev.keystroke.modifiers.platform; // Cmd on macOS
        let key = ev.keystroke.key.as_str();

        if key == "escape" {
            // Defocus composer (just stop_propagation; root will not eat this since
            // palette is not open and root key handler only cares about escape-while-palette).
            cx.stop_propagation();
            return;
        }

        if key == "enter" {
            self.submit_composer(cx);
            cx.stop_propagation();
            return;
        }

        if key == "backspace" && !ctrl && !platform {
            self.composer.pop();
            cx.notify();
            cx.stop_propagation();
            return;
        }

        if key == "space" && !ctrl && !platform {
            self.composer.push(' ');
            cx.notify();
            cx.stop_propagation();
            return;
        }

        // Printable single-char key (letters, digits, punctuation).
        // Shift is already folded into the key string by GPUI (e.g. "A" for shift-a).
        if key.chars().count() == 1 && !ctrl && !platform {
            self.composer.push_str(key);
            cx.notify();
            cx.stop_propagation();
        }
    }

    /// Handle a key event directed at the command palette (when palette_open).
    fn on_palette_key_down(
        &mut self,
        ev: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ctrl = ev.keystroke.modifiers.control;
        let platform = ev.keystroke.modifiers.platform;
        let key = ev.keystroke.key.as_str();

        if key == "escape" {
            self.palette_open = false;
            cx.notify();
            cx.stop_propagation();
            return;
        }

        if key == "enter" {
            // Jump to the first session row that matches palette_query.
            let query = self.palette_query.to_lowercase();
            if let Some(id) = self
                .sessions
                .values()
                .find(|s| query.is_empty() || s.name.to_lowercase().contains(&query))
                .map(|s| s.id.clone())
            {
                self.focused = Some(id.clone());
                let tx = self.requests.clone();
                let _ = tx.try_send(Request::RequestGrid(id));
            }
            self.palette_open = false;
            self.palette_query.clear();
            cx.notify();
            cx.stop_propagation();
            return;
        }

        if key == "backspace" && !ctrl && !platform {
            self.palette_query.pop();
            cx.notify();
            cx.stop_propagation();
            return;
        }

        if key == "space" && !ctrl && !platform {
            self.palette_query.push(' ');
            cx.notify();
            cx.stop_propagation();
            return;
        }

        if key.chars().count() == 1 && !ctrl && !platform {
            self.palette_query.push_str(key);
            cx.notify();
            cx.stop_propagation();
        }
    }
}

// ── Focusable impl (lets GPUI route keyboard events to our terminal pane) ─────

impl Focusable for FleetTermApp {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ── small rendering helpers ───────────────────────────────────────────────────

fn status_dot(c: u32) -> impl IntoElement {
    div().w(px(8.0)).h(px(8.0)).rounded_full().bg(rgb(c))
}

/// Map a session's State to a dot colour.
fn dot_color(state: &State) -> u32 {
    match state {
        State::NeedsInput(_) => color::AMBER,
        State::Stuck => color::RED,
        State::Working => color::BLUE,
        State::Idle => color::DIM,
        State::Done => color::GREEN,
        State::Dead => color::MUT,
    }
}

/// Map Autonomy to (label, colour).
fn autonomy_badge(a: Autonomy) -> (&'static str, u32) {
    match a {
        Autonomy::Manual => ("Manual", color::AMBER),
        Autonomy::Guarded => ("Guarded", color::BLUE),
        Autonomy::Auto => ("Auto", color::GREEN),
    }
}

/// Status-bar glyph for a session state.
fn state_glyph(state: &State) -> &'static str {
    match state {
        State::NeedsInput(_) => "⚑",
        State::Stuck => "⚠",
        State::Working => "●",
        State::Idle => "○",
        State::Done => "✓",
        State::Dead => "✗",
    }
}

/// Status-bar glyph colour.
fn state_glyph_color(state: &State) -> u32 {
    dot_color(state)
}

/// A small pill button used in the title bar for view mode toggles.
/// `active` controls whether it gets the highlighted background.
fn view_mode_chip(label: &'static str, active: bool) -> gpui::Div {
    div()
        .text_xs()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(if active {
            rgb(color::BORDER)
        } else {
            rgb(color::HEAD)
        })
        .border_1()
        .border_color(rgb(color::BORDER))
        .text_color(if active {
            rgb(color::TEXT)
        } else {
            rgb(color::MUT)
        })
        .child(SharedString::from(label))
}

// ── Render ────────────────────────────────────────────────────────────────────

impl Render for FleetTermApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // ── Bug fix (B2): Resize the focused session's PTY to fit the pane. ──
        // Compute monospace cell metrics from the window's current text style.
        // This mirrors the exact calls in terminal.rs::request_layout so the grid
        // dimensions stay in lock-step with what the GridElement will actually paint.
        {
            let style = window.text_style();
            let font_id = window.text_system().resolve_font(&style.font());
            let font_size = style.font_size.to_pixels(window.rem_size());
            let cell_w = window
                .text_system()
                .em_advance(font_id, font_size)
                .unwrap_or(px(8.0));
            let line_h = window.line_height();

            let viewport = window.viewport_size();

            // Estimate the terminal pane area:
            //   Split mode: subtract sidebar (320px) and left border (~1px) from width.
            //   Focus mode: full width (no sidebar).
            //   Tiled mode: no single terminal pane — skip resize.
            // Heights: subtract title bar (40px) + status bar (28px) + composer (32px)
            //          + 8px padding margin.
            let sidebar_w: f32 = if self.view_mode == ViewMode::Split { 321.0 } else { 0.0 };
            let chrome_h: f32 = 40.0 + 28.0 + 32.0 + 8.0; // titlebar + status + composer + pad

            let pane_w = f32::from(viewport.width) - sidebar_w;
            let pane_h = f32::from(viewport.height) - chrome_h;

            let cell_w_f32 = f32::from(cell_w);
            let line_h_f32 = f32::from(line_h);

            let cols = if cell_w_f32 > 0.0 {
                ((pane_w / cell_w_f32) as u16).max(20)
            } else {
                80
            };
            let rows = if line_h_f32 > 0.0 {
                ((pane_h / line_h_f32) as u16).max(6)
            } else {
                24
            };

            // Only send Resize when the target (session, cols, rows) has changed.
            // Sending in render is acceptable because it is guarded by this change-check
            // and does NOT call cx.notify() (which would trigger an infinite render loop).
            if self.view_mode != ViewMode::Tiled {
                if let Some(ref fid) = self.focused.clone() {
                    let needs_send = match &self.last_sent_size {
                        Some((last_id, last_cols, last_rows)) => {
                            last_id != fid || *last_cols != cols || *last_rows != rows
                        }
                        None => true,
                    };
                    if needs_send {
                        let _ = self.requests.try_send(Request::Resize {
                            session: fid.clone(),
                            cols,
                            rows,
                        });
                        self.last_sent_size = Some((fid.clone(), cols, rows));
                    }
                }
            }
        }

        let (needs, working, done) = self.counts();
        let n_sessions = self.sessions.len();
        let total_cost = self.total_cost;
        let default_autonomy = self.default_autonomy;

        // ── Collect session data we need (avoid holding &self borrows in closures) ──
        // We build snapshot vecs before constructing elements so closures only capture
        // plain data (SessionId, Autonomy, State) rather than &self.

        // Sessions sorted by priority (highest first), then by id for stability.
        let mut sorted_sessions: Vec<Session> = self.sessions.values().cloned().collect();
        sorted_sessions.sort_by(|a, b| {
            b.state
                .priority()
                .cmp(&a.state.priority())
                .then_with(|| a.id.cmp(&b.id))
        });

        // Terminal text for the focused pane.
        let term_content: Vec<SharedString> = self
            .focused
            .as_ref()
            .and_then(|id| self.term_text.get(id))
            .map(|t| {
                t.lines()
                    .map(|l| SharedString::from(l.to_owned()))
                    .collect()
            })
            .unwrap_or_else(|| {
                vec![SharedString::from(
                    "No session selected — connect a daemon or spawn a session.".to_owned(),
                )]
            });

        let focused_id = self.focused.clone();
        let requests_tx = self.requests.clone();
        let focus_handle = self.focus_handle.clone();

        // If there's a grid for the focused session, clone it for rendering.
        let focused_grid: Option<terminal::GridState> = self
            .focused
            .as_ref()
            .and_then(|id| self.grids.get(id))
            .cloned();

        // Snapshot of view mode and palette state for the closure captures below.
        let view_mode = self.view_mode;
        let palette_open = self.palette_open;
        let palette_query = self.palette_query.clone();

        // ── P6: Snapshot composer + block stats ───────────────────────────────
        let composer_text = self.composer.clone();
        let composer_focus = self.composer_focus.clone();
        // Clone block stats for the focused session (if any).
        let focused_block_stats: Option<BlockStats> = self
            .focused
            .as_ref()
            .and_then(|id| self.blocks.get(id))
            .cloned();

        // ── Title bar ─────────────────────────────────────────────────────────
        // View mode toggle chips
        let chip_split_active = view_mode == ViewMode::Split;
        let chip_tiled_active = view_mode == ViewMode::Tiled;
        let chip_focus_active = view_mode == ViewMode::Focus;

        let chip_split = view_mode_chip("Split", chip_split_active).on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                this.view_mode = ViewMode::Split;
                cx.notify();
            }),
        );
        let chip_tiled = view_mode_chip("Tiled", chip_tiled_active).on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                this.view_mode = ViewMode::Tiled;
                cx.notify();
            }),
        );
        let chip_focus = view_mode_chip("Focus", chip_focus_active).on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                this.view_mode = ViewMode::Focus;
                cx.notify();
            }),
        );

        let title_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(40.0))
            .px_3()
            .bg(rgb(color::HEAD))
            .border_b_1()
            .border_color(rgb(color::BORDER))
            // macOS-style traffic dots
            .child(status_dot(color::RED))
            .child(status_dot(color::AMBER))
            .child(status_dot(color::GREEN))
            .child(
                div()
                    .ml_2()
                    .text_color(rgb(0xe6e6f0))
                    .child(SharedString::from("FleetTerm")),
            )
            // ── P5: view mode chips ────────────────────────────────────────────
            .child(div().flex_row().gap_1().flex().child(chip_split).child(chip_tiled).child(chip_focus))
            .child(div().flex_1())
            // Live counts
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(color::AMBER))
                    .child(SharedString::from(format!("⚑ {} need you", needs))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(color::BLUE))
                    .child(SharedString::from(format!("● {} working", working))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(color::GREEN))
                    .child(SharedString::from(format!("✓ {} done", done))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(color::DIM))
                    .child(SharedString::from(format!("${:.2}", total_cost))),
            );

        // ── P6: Command-block indicator bar ───────────────────────────────────
        // Shown at the top of the terminal pane when OSC 133 data is available
        // for the focused session.  Subtle: same bg as HEAD, thin border at bottom.
        let block_indicator: Option<gpui::AnyElement> = focused_block_stats.map(|stats| {
            // Running dot: amber while executing.
            let running_part: gpui::AnyElement = if stats.running {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .w(px(6.0))
                            .h(px(6.0))
                            .rounded_full()
                            .bg(rgb(color::AMBER)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(color::AMBER))
                            .child(SharedString::from("running")),
                    )
                    .into_any_element()
            } else {
                div().into_any_element()
            };

            // Exit code badge.
            let exit_part: gpui::AnyElement = match stats.last_exit {
                None => div().into_any_element(),
                Some(code) => {
                    let (label, clr) = if code == 0 {
                        (format!("exit {}", code), color::GREEN)
                    } else {
                        (format!("exit {}", code), color::RED)
                    };
                    div()
                        .text_xs()
                        .text_color(rgb(clr))
                        .child(SharedString::from(label))
                        .into_any_element()
                }
            };

            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_3()
                .h(px(22.0))
                .bg(rgb(color::HEAD))
                .border_b_1()
                .border_color(rgb(color::BORDER))
                // Command count
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(color::MUT))
                        .child(SharedString::from(format!("⎇ {} cmds", stats.commands))),
                )
                .child(exit_part)
                .child(running_part)
                .into_any_element()
        });

        // ── Terminal pane ──────────────────────────────────────────────────────
        // When a `Event::Grid` has been received for the focused session, render it
        // as a real cell-grid (Layer 1 bg quads + Layer 2 glyphs + Layer 3 cursor)
        // inside a focusable div that forwards keystrokes to the PTY.
        // Otherwise fall back to the raw text-line view.
        let terminal_pane = if let Some(grid) = focused_grid {
            // Build the grid element (paints only; key handling is on the wrapping div).
            let grid_elem = terminal::GridElement {
                grid,
                focus_handle: focus_handle.clone(),
            };
            // Wrap in a focusable, key-handling div.
            // Clicking the terminal grabs keyboard focus so key events start flowing.
            let fh_click = focus_handle.clone();
            div()
                .flex()
                .flex_col()
                .flex_1()
                .bg(rgb(color::TERM))
                .overflow_hidden()
                .track_focus(&focus_handle)
                .key_context("Terminal")
                .on_key_down(cx.listener(FleetTermApp::on_term_key_down))
                .on_mouse_down(
                    MouseButton::Left,
                    move |_ev: &MouseDownEvent, window, _cx| {
                        fh_click.focus(window);
                    },
                )
                // P6: block indicator at the top (conditionally).
                .when_some(block_indicator, |d, bar| d.child(bar))
                .child(grid_elem)
        } else {
            // Fallback: plain text lines.
            div()
                .flex()
                .flex_col()
                .flex_1()
                .p_3()
                .bg(rgb(color::TERM))
                .text_color(rgb(color::TEXT))
                .overflow_hidden()
                .child(
                    div()
                        .id("term-scroll")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .gap_1()
                        .overflow_y_scroll()
                        .children(term_content.into_iter().map(|line| {
                            div().text_xs().text_color(rgb(color::TEXT)).child(line)
                        })),
                )
        };

        // ── Fleet sidebar ──────────────────────────────────────────────────────
        // Header with session count, cost, and + buttons.
        let tx_shell = requests_tx.clone();
        let tx_claude = requests_tx.clone();
        let sidebar_header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(color::BORDER))
            .child(
                div()
                    .text_color(rgb(0xe6e6f0))
                    .child(SharedString::from(format!(
                        "Fleet · {} sessions",
                        n_sessions
                    ))),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(color::DIM))
                    .child(SharedString::from(format!("${:.2}", total_cost))),
            )
            // "+ shell" button
            .child(
                div()
                    .text_xs()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(color::BORDER))
                    .text_color(rgb(color::TEXT))
                    .child(SharedString::from("+ shell"))
                    .on_mouse_down(
                        MouseButton::Left,
                        move |_ev: &MouseDownEvent, _window, _cx| {
                            let _ = tx_shell.try_send(Request::Spawn(SpawnSpec {
                                name: None,
                                tool: Tool::Shell,
                                model: None,
                                cwd: None,
                                worktree_from: None,
                                autonomy: default_autonomy,
                                opening: None,
                                env: vec![],
                            }));
                        },
                    ),
            )
            // "+ claude" button
            .child(
                div()
                    .text_xs()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(color::BLUE))
                    .text_color(rgb(color::HEAD))
                    .child(SharedString::from("+ claude"))
                    .on_mouse_down(
                        MouseButton::Left,
                        move |_ev: &MouseDownEvent, _window, _cx| {
                            let _ = tx_claude.try_send(Request::Spawn(SpawnSpec {
                                name: None,
                                tool: Tool::Claude,
                                model: None,
                                cwd: None,
                                worktree_from: None,
                                autonomy: default_autonomy,
                                opening: None,
                                env: vec![],
                            }));
                        },
                    ),
            );

        // ── Session rows ───────────────────────────────────────────────────────
        // Build the three groups: NEEDS YOU, WORKING, DONE.
        let needs_you_sessions: Vec<Session> = sorted_sessions
            .iter()
            .filter(|s| s.state.needs_human())
            .cloned()
            .collect();
        let working_sessions: Vec<Session> = sorted_sessions
            .iter()
            .filter(|s| matches!(s.state, State::Working | State::Idle))
            .cloned()
            .collect();
        let done_sessions: Vec<Session> = sorted_sessions
            .iter()
            .filter(|s| matches!(s.state, State::Done | State::Dead))
            .cloned()
            .collect();

        // Helper that builds one session card row.
        // Bug fix (B1): all click handlers now use cx.listener so they can mutate
        // FleetTermApp state directly (set self.focused, self.sessions autonomy) and
        // call cx.notify() for an immediate repaint — giving instant visual feedback.
        //
        // Autonomy pill and action buttons stop event propagation so the outer
        // card-click handler does not also fire.
        //
        // IMPORTANT: build_row captures cx (for cx.listener calls). It is scoped
        // inside a block so that the cx borrow is released before the later palette
        // cx.listener calls further down in render.
        let (needs_rows, working_rows, done_rows) = {
        let build_row = |s: &Session, show_buttons: bool| {
            let id = s.id.clone();
            let id_for_auto = s.id.clone();
            let id_for_approve = s.id.clone();
            let id_for_deny = s.id.clone();
            let current_autonomy = s.autonomy;
            let is_focused = focused_id.as_ref() == Some(&s.id);

            let (auto_label, auto_color) = autonomy_badge(s.autonomy);
            let dot_c = dot_color(&s.state);
            let branch_label = s.branch.as_deref().unwrap_or("-").to_owned();
            let tool_label = format!("{:?}", s.tool);
            let name_label = s.name.clone();
            let activity_label = s.activity.clone();

            let row_bg = if is_focused { color::BORDER } else { color::SIDE };

            // Autonomy pill — cycles Manual→Guarded→Auto; optimistically updates
            // local state for instant feedback, then sends the daemon request.
            // Stops propagation so the card-click focus handler does not also fire.
            let autonomy_pill = div()
                .text_xs()
                .px_1()
                .rounded_md()
                .text_color(rgb(auto_color))
                .bg(rgb(color::HEAD))
                .child(SharedString::from(auto_label))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                        let next = match current_autonomy {
                            Autonomy::Manual => Autonomy::Guarded,
                            Autonomy::Guarded => Autonomy::Auto,
                            Autonomy::Auto => Autonomy::Manual,
                        };
                        // Optimistic local update so the pill re-renders immediately.
                        if let Some(session) = this.sessions.get_mut(&id_for_auto) {
                            session.autonomy = next;
                        }
                        let _ = this.requests.try_send(Request::SetAutonomy {
                            session: id_for_auto.clone(),
                            level: next,
                        });
                        cx.stop_propagation();
                        cx.notify();
                    }),
                );

            // Name + autonomy + branch row.
            let name_row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_color(rgb(0xe6e6f0))
                        .child(SharedString::from(name_label)),
                )
                .child(autonomy_pill)
                .child(div().flex_1())
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(color::GREEN))
                        .child(SharedString::from(branch_label)),
                );

            // Activity + tool row.
            let activity_row = div()
                .text_xs()
                .text_color(rgb(color::DIM))
                .child(SharedString::from(activity_label));
            let tool_row = div()
                .text_xs()
                .text_color(rgb(color::MUT))
                .child(SharedString::from(tool_label));

            // Approve / Deny buttons (shown only in NEEDS YOU group).
            // Both stop propagation to prevent the card-click from also firing.
            let buttons_row = if show_buttons {
                let approve = div()
                    .text_xs()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(color::GREEN))
                    .text_color(rgb(color::HEAD))
                    .child(SharedString::from("Approve"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            let _ = this.requests.try_send(Request::Decide {
                                session: id_for_approve.clone(),
                                approve: true,
                                instruction: None,
                            });
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    );
                let deny = div()
                    .text_xs()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(color::RED))
                    .text_color(rgb(0xe6e6f0))
                    .child(SharedString::from("Deny"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            let _ = this.requests.try_send(Request::Decide {
                                session: id_for_deny.clone(),
                                approve: false,
                                instruction: None,
                            });
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    );
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .mt(px(4.0))
                    .child(approve)
                    .child(deny)
            } else {
                div()
            };

            // The full card — clicking immediately sets self.focused (instant highlight)
            // AND requests a fresh grid from the daemon.
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap_2()
                .px_2()
                .py_2()
                .rounded_md()
                .bg(rgb(row_bg))
                .child(div().mt(px(5.0)).child(status_dot(dot_c)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .gap_1()
                        .child(name_row)
                        .child(activity_row)
                        .child(tool_row)
                        .child(buttons_row),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                        this.focused = Some(id.clone());
                        let _ = this.requests.try_send(Request::RequestGrid(id.clone()));
                        cx.notify();
                    }),
                )
        };

        // Collect session rows; build_row captures cx via closure.
        let nr = needs_you_sessions
            .iter()
            .map(|s| build_row(s, true))
            .collect::<Vec<_>>();
        let wr = working_sessions
            .iter()
            .map(|s| build_row(s, false))
            .collect::<Vec<_>>();
        let dr = done_sessions
            .iter()
            .map(|s| build_row(s, false))
            .collect::<Vec<_>>();
        (nr, wr, dr)
        }; // end of build_row scope — closure dropped here, releasing the cx borrow

        // Build group label helper (no cx capture needed).
        let group_label = |text: &'static str, color: u32| {
            div()
                .text_xs()
                .px_2()
                .py_1()
                .text_color(rgb(color))
                .child(SharedString::from(text))
        };

        let sidebar = div()
            .flex()
            .flex_col()
            .w(px(320.0))
            .bg(rgb(color::SIDE))
            .border_l_1()
            .border_color(rgb(color::BORDER))
            .child(sidebar_header)
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .p_2()
                    .gap_1()
                    .overflow_y_scroll()
                    // NEEDS YOU group
                    .when(!needs_you_sessions.is_empty(), |d| {
                        d.child(group_label("NEEDS YOU", color::AMBER))
                            .children(needs_rows)
                    })
                    // WORKING group
                    .when(!working_sessions.is_empty(), |d| {
                        d.child(group_label("WORKING", color::BLUE))
                            .children(working_rows)
                    })
                    // DONE group
                    .when(!done_sessions.is_empty(), |d| {
                        d.child(group_label("DONE", color::DIM))
                            .children(done_rows)
                    })
                    // Empty state
                    .when(n_sessions == 0, |d| {
                        d.child(
                            div()
                                .p_4()
                                .text_xs()
                                .text_color(rgb(color::MUT))
                                .child(SharedString::from(
                                    "No sessions. Press + shell or + claude to start.",
                                )),
                        )
                    }),
            );

        // ── P6: Fleet Composer bar ─────────────────────────────────────────────
        // A focusable input row that lets the operator type a command and send it to
        // one or many sessions.  Sits above the status line; clicking it steals focus
        // from the terminal pane.
        //
        // Caret: we append a block-cursor "█" glyph when the composer is focused.
        // Since we cannot query `is_focused` without window state during render, we
        // always show the caret when the composer is non-empty OR as a static prompt.
        // (A future improvement could use Window::is_focused(&composer_focus) once the
        //  GPUI 0.2.x API stabilises for querying focus state in render.)
        let composer_display = {
            let prompt = "❯ ";
            // Show the typed text; append "█" as a visible caret.
            let text = format!("{}{}█", prompt, composer_text);
            SharedString::from(text)
        };
        let composer_focus_click = composer_focus.clone();
        let composer_bar = div()
            .id("composer-bar")
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(32.0))
            .bg(rgb(color::BG))
            .border_t_1()
            .border_color(rgb(color::BORDER))
            .track_focus(&composer_focus)
            // Clicking the bar focuses it so key events route here.
            .on_mouse_down(
                MouseButton::Left,
                move |_ev: &MouseDownEvent, window, _cx| {
                    composer_focus_click.focus(window);
                },
            )
            // Key handler: accumulate into composer, enter = submit.
            .on_key_down(cx.listener(FleetTermApp::on_composer_key_down))
            // Composer text (prompt + typed text + caret)
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(rgb(color::TEXT))
                    .child(composer_display),
            )
            // Hint
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(color::MUT))
                    .child(SharedString::from(
                        "@name / @all / @working to target · ⏎ send",
                    )),
            );

        // ── Status line ────────────────────────────────────────────────────────
        // Per-session segments in the footer.
        let status_segments = sorted_sessions.iter().enumerate().map(|(i, s)| {
            let glyph = state_glyph(&s.state);
            let glyph_color = state_glyph_color(&s.state);
            let label = format!("{}:{} {}", i + 1, s.name, glyph);
            div()
                .text_xs()
                .text_color(rgb(glyph_color))
                .child(SharedString::from(label))
        });

        let status_line = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(28.0))
            .px_3()
            .bg(rgb(color::HEAD))
            .border_t_1()
            .border_color(rgb(color::BORDER))
            .text_xs()
            .text_color(rgb(color::MUT))
            .children(status_segments)
            .child(div().flex_1())
            .child(
                div()
                    .text_color(rgb(color::DIM))
                    .child(SharedString::from(format!("${:.2}", total_cost))),
            )
            .child(div().text_color(rgb(color::MUT)).child(SharedString::from("⌘K palette")));

        // ── P5: Body — layout varies by view mode ─────────────────────────────
        let body = match view_mode {
            ViewMode::Split => {
                // Original layout: terminal pane + sidebar.
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(terminal_pane)
                    .child(sidebar)
                    .into_any_element()
            }
            ViewMode::Focus => {
                // Terminal pane only — sidebar hidden.
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(terminal_pane)
                    .into_any_element()
            }
            ViewMode::Tiled => {
                // Grid of tiles, one per session with a grid snapshot (or text fallback).
                // We show at most TILED_MAX tiles; if more sessions exist we note it.
                let all_sessions: Vec<Session> = sorted_sessions.clone();
                let truncated = all_sessions.len() > TILED_MAX;
                let tile_sessions = all_sessions.into_iter().take(TILED_MAX);

                let tiles = tile_sessions.enumerate().map(|(tile_idx, s)| {
                    let sid = s.id.clone();
                    let sid_click = s.id.clone();
                    let dot_c = dot_color(&s.state);
                    let glyph = state_glyph(&s.state);
                    let tile_name = format!("{} {}", s.name, glyph);
                    let tx_tile = requests_tx.clone();

                    // Pick a stable element id for the tile.
                    let tile_id = SharedString::from(format!("tile-{}", tile_idx));

                    // If we have a grid for this session, render it as a GridElement.
                    // Otherwise show the last few lines of term_text.
                    let tile_content: gpui::AnyElement = {
                        if let Some(grid) = self.grids.get(&sid).cloned() {
                            let fh = focus_handle.clone();
                            terminal::GridElement {
                                grid,
                                focus_handle: fh,
                            }
                            .into_any_element()
                        } else {
                            // Show last 6 lines of text output, or a placeholder.
                            let lines: Vec<SharedString> = self
                                .term_text
                                .get(&sid)
                                .map(|t| {
                                    t.lines()
                                        .rev()
                                        .take(6)
                                        .collect::<Vec<_>>()
                                        .into_iter()
                                        .rev()
                                        .map(|l| SharedString::from(l.to_owned()))
                                        .collect()
                                })
                                .unwrap_or_else(|| vec![SharedString::from("(no output yet)")]);
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .gap_1()
                                .p_1()
                                .children(lines.into_iter().map(|l| {
                                    div()
                                        .text_xs()
                                        .text_color(rgb(color::DIM))
                                        .child(l)
                                }))
                                .into_any_element()
                        }
                    };

                    div()
                        .id(tile_id)
                        // Each tile takes ~50% of the row width; two columns via flex-wrap.
                        .w(relative(0.5))
                        .h(px(200.0))
                        .flex()
                        .flex_col()
                        .border_1()
                        .border_color(rgb(color::BORDER))
                        .bg(rgb(color::TERM))
                        .overflow_hidden()
                        // Tile header: status dot + name.
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .px_2()
                                .py_1()
                                .bg(rgb(color::HEAD))
                                .border_b_1()
                                .border_color(rgb(color::BORDER))
                                .child(status_dot(dot_c))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(color::TEXT))
                                        .child(SharedString::from(tile_name)),
                                ),
                        )
                        // Tile body: grid or text.
                        .child(div().flex().flex_col().flex_1().overflow_hidden().child(tile_content))
                        // Clicking focuses this session and fetches a fresh grid.
                        .on_mouse_down(
                            MouseButton::Left,
                            move |_ev: &MouseDownEvent, _window, _cx| {
                                let _ = tx_tile.try_send(Request::RequestGrid(sid_click.clone()));
                            },
                        )
                });

                let mut tiled_div = div()
                    .id("tiled-body")
                    .flex()
                    .flex_wrap()
                    .flex_1()
                    .min_h(px(0.0))
                    .bg(rgb(color::BG))
                    .overflow_y_scroll()
                    .children(tiles);

                // Truncation notice when session count exceeds TILED_MAX.
                if truncated {
                    tiled_div = tiled_div.child(
                        div()
                            .w_full()
                            .px_3()
                            .py_2()
                            .text_xs()
                            .text_color(rgb(color::MUT))
                            .child(SharedString::from(format!(
                                "… {} more sessions — switch to Split view to see all",
                                n_sessions.saturating_sub(TILED_MAX)
                            ))),
                    );
                }

                tiled_div.into_any_element()
            }
        };

        // ── P5/P6: Command palette overlay ────────────────────────────────────
        // The palette is layered as an absolute child of the root so it sits above
        // everything else.  Pattern from gpui's own FallbackPromptRenderer (prompts.rs):
        // parent is relative, palette is absolute + top_0 + left_0 + size_full.
        //
        // P6: palette_query is now typeable (routed via root on_key_down when
        // palette_open) and session rows are filtered by substring match.

        // Build the session jump rows — filtered by palette_query (case-insensitive).
        let palette_query_lower = palette_query.to_lowercase();
        let palette_sessions: Vec<Session> = sorted_sessions
            .iter()
            .filter(|s| {
                palette_query_lower.is_empty()
                    || s.name.to_lowercase().contains(&palette_query_lower)
            })
            .cloned()
            .collect();
        let palette_open_inner = palette_open;

        // Collect info for the "approve next NeedsInput" action.
        let first_needs: Option<SessionId> = palette_sessions
            .iter()
            .find(|s| s.state.needs_human())
            .map(|s| s.id.clone());

        let tx_palette_shell = requests_tx.clone();
        let tx_palette_claude = requests_tx.clone();
        let tx_palette_approve = requests_tx.clone();
        let first_needs_for_approve = first_needs.clone();

        // Build palette rows for each session.
        let palette_session_rows: Vec<_> = palette_sessions
            .iter()
            .map(|s| {
                let label = format!(
                    "jump to {}  {}",
                    s.name,
                    state_glyph(&s.state)
                );
                let sid_jump = s.id.clone();
                let tx_jump = requests_tx.clone();

                div()
                    .id(SharedString::from(format!("pal-sess-{}", s.id)))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(color::BORDER))
                    .text_xs()
                    .text_color(rgb(color::TEXT))
                    .child(status_dot(dot_color(&s.state)))
                    .child(SharedString::from(label))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                            this.focused = Some(sid_jump.clone());
                            let _ = tx_jump.try_send(Request::RequestGrid(sid_jump.clone()));
                            this.palette_open = false;
                            cx.notify();
                        }),
                    )
            })
            .collect();

        // "approve next ⚑" row — only shown when there is a NeedsInput session.
        let approve_row = first_needs.map(|_| {
            div()
                .id("pal-approve-next")
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(rgb(color::BORDER))
                .text_xs()
                .text_color(rgb(color::AMBER))
                .child(SharedString::from("approve next ⚑"))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                        if let Some(ref sid) = first_needs_for_approve {
                            let _ = tx_palette_approve.try_send(Request::Decide {
                                session: sid.clone(),
                                approve: true,
                                instruction: None,
                            });
                        }
                        this.palette_open = false;
                        cx.notify();
                    }),
                )
        });

        // ── Root ───────────────────────────────────────────────────────────────
        // The root div must be `id`'d and `track_focus`'d so that on_action fires
        // for keybindings registered on the App (cmd-k → TogglePalette).
        div()
            .id("fleet-root")
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(color::BG))
            .text_color(rgb(color::TEXT))
            .track_focus(&self.focus_handle)
            .key_context("FleetTerm")
            // P5: Toggle palette action handler.
            .on_action(cx.listener(|this, _: &TogglePalette, _window, cx| {
                this.palette_open = !this.palette_open;
                cx.notify();
            }))
            // P5/P6: Keyboard routing on the root element.
            // When the palette is open, every key event is consumed by the palette
            // (accumulate into palette_query, enter jumps, escape closes).
            // When the palette is closed, only Escape is handled here (terminal pane
            // handles all other keys via its own on_key_down + focus_handle).
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if this.palette_open {
                    this.on_palette_key_down(ev, window, cx);
                }
                // When palette is closed, let other handlers (terminal, composer) take over.
            }))
            .child(title_bar)
            .child(body)
            // P6: Fleet composer bar — sits above the status line.
            .child(composer_bar)
            .child(status_line)
            // ── Palette overlay (conditionally rendered) ───────────────────────
            // Pattern: absolute child covering the full root.  The palette panel itself
            // is centered via flex justify/items_center on the backdrop div.
            .when(palette_open_inner, |root| {
                root.child(
                    // Semi-transparent backdrop — clicking it closes the palette.
                    div()
                        .id("palette-backdrop")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .bg(rgb(0x000000))
                        .opacity(0.6)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _ev: &MouseDownEvent, _window, cx| {
                                this.palette_open = false;
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    // The palette panel — centered absolute overlay.
                    div()
                        .id("palette-panel")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_start()
                        // Push the panel down ~15% from the top so it reads as a modal.
                        .pt(px(80.0))
                        .child(
                            div()
                                .w(px(520.0))
                                .h(px(480.0))
                                .flex()
                                .flex_col()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(color::BORDER))
                                .bg(rgb(color::PALETTE_BG))
                                .overflow_hidden()
                                // Query prompt — P6: now typeable via root on_key_down.
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_2()
                                        .px_3()
                                        .py_2()
                                        .bg(rgb(color::HEAD))
                                        .border_b_1()
                                        .border_color(rgb(color::BORDER))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(color::MUT))
                                                .child(SharedString::from("⌘")),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .text_xs()
                                                .text_color(rgb(color::TEXT))
                                                .child(SharedString::from({
                                                    // Show typed query with caret, or placeholder.
                                                    if palette_query.is_empty() {
                                                        "Search commands and sessions…█".to_owned()
                                                    } else {
                                                        format!("{}█", palette_query)
                                                    }
                                                })),
                                        ),
                                )
                                // Scrollable list of actions + sessions.
                                .child(
                                    div()
                                        .id("palette-list")
                                        .flex()
                                        .flex_col()
                                        .overflow_y_scroll()
                                        // Static action: "+ new shell"
                                        .child(
                                            div()
                                                .id("pal-new-shell")
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap_2()
                                                .px_3()
                                                .py_2()
                                                .border_b_1()
                                                .border_color(rgb(color::BORDER))
                                                .text_xs()
                                                .text_color(rgb(color::BLUE))
                                                .child(SharedString::from("+ new shell"))
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                                                        let _ = tx_palette_shell.try_send(Request::Spawn(SpawnSpec {
                                                            name: None,
                                                            tool: Tool::Shell,
                                                            model: None,
                                                            cwd: None,
                                                            worktree_from: None,
                                                            autonomy: default_autonomy,
                                                            opening: None,
                                                            env: vec![],
                                                        }));
                                                        this.palette_open = false;
                                                        cx.notify();
                                                    }),
                                                ),
                                        )
                                        // Static action: "+ new claude"
                                        .child(
                                            div()
                                                .id("pal-new-claude")
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap_2()
                                                .px_3()
                                                .py_2()
                                                .border_b_1()
                                                .border_color(rgb(color::BORDER))
                                                .text_xs()
                                                .text_color(rgb(color::BLUE))
                                                .child(SharedString::from("+ new claude"))
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(move |this, _ev: &MouseDownEvent, _window, cx| {
                                                        let _ = tx_palette_claude.try_send(Request::Spawn(SpawnSpec {
                                                            name: None,
                                                            tool: Tool::Claude,
                                                            model: None,
                                                            cwd: None,
                                                            worktree_from: None,
                                                            autonomy: default_autonomy,
                                                            opening: None,
                                                            env: vec![],
                                                        }));
                                                        this.palette_open = false;
                                                        cx.notify();
                                                    }),
                                                ),
                                        )
                                        // Conditional: "approve next ⚑"
                                        .when_some(approve_row, |d, row| d.child(row))
                                        // Session jump rows.
                                        .when(!palette_session_rows.is_empty(), |d| {
                                            d.child(
                                                div()
                                                    .px_3()
                                                    .py_1()
                                                    .text_xs()
                                                    .text_color(rgb(color::MUT))
                                                    .child(SharedString::from("SESSIONS")),
                                            )
                                            .children(palette_session_rows)
                                        }),
                                ),
                        ),
                )
            })
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    Application::new().run(|cx: &mut App| {
        // P5: bind Cmd-K and Ctrl-K to TogglePalette globally.
        cx.bind_keys([
            KeyBinding::new("cmd-k", TogglePalette, None),
            KeyBinding::new("ctrl-k", TogglePalette, None),
        ]);

        let bounds = Bounds::centered(None, size(px(1100.0), px(700.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("FleetTerm".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(FleetTermApp::new),
        )
        .unwrap();
        cx.activate(true);
    });
}
