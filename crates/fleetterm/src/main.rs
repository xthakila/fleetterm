//! FleetTerm UI — Phase 2 / 3 fleet cockpit.
//!
//! Connects to `fleetd` over a unix socket (via `client::FleetClient`), renders all agent
//! sessions in a sidebar, lets the user approve/deny decisions and cycle autonomy, and
//! shows the focused session's terminal output in the main pane.
//!
//! API verified against gpui 0.2.2 (new App/Window/Context surface).

mod client;

use std::collections::{BTreeMap, HashMap};

use async_channel::Sender;
use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, MouseButton,
    MouseDownEvent, SharedString, TitlebarOptions, WeakEntity, Window, WindowBounds,
    WindowOptions,
};
use protocol::{Autonomy, Event, Request, Session, SessionId, SpawnSpec, State, Tool};

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
}

// ── terminal output cap ───────────────────────────────────────────────────────
const TERM_MAX_BYTES: usize = 40 * 1024; // 40 KB
const TERM_MAX_LINES: usize = 400;

// ── application state ────────────────────────────────────────────────────────

struct FleetTermApp {
    /// All known sessions, keyed by id (BTreeMap keeps stable insertion order by id).
    sessions: BTreeMap<SessionId, Session>,
    /// Terminal output buffer per session (raw UTF-8, lossy).
    term_text: HashMap<SessionId, String>,
    /// Which session's output is shown in the left pane.
    focused: Option<SessionId>,
    /// Fleet-wide default autonomy for new sessions.
    default_autonomy: Autonomy,
    /// Running total cost reported by the daemon.
    total_cost: f64,
    /// Channel for sending requests to the daemon.
    requests: Sender<Request>,
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
            focused: None,
            default_autonomy: Autonomy::Guarded,
            total_cost: 0.0,
            requests,
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
            // DecisionPending: state already updated via SessionUpdate; sidebar shows buttons.
            Event::DecisionPending { .. } => {}
            // AutoDecision and Error: silently ignored (could append to a log in the future).
            Event::AutoDecision { .. } => {}
            Event::Error { .. } => {}
            // Catch-all: forward-compatible with new daemon variants.
            _ => {}
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

// ── Render ────────────────────────────────────────────────────────────────────

impl Render for FleetTermApp {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
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

        // ── Title bar ─────────────────────────────────────────────────────────
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

        // ── Terminal pane ──────────────────────────────────────────────────────
        // Each line of terminal output is a text child; the column scrolls vertically.
        let terminal_pane = div()
            .flex()
            .flex_col()
            .flex_1()
            .p_3()
            .bg(rgb(color::TERM))
            .text_color(rgb(color::TEXT))
            .overflow_hidden()
            // Scrollable inner container — needs an id for StatefulInteractiveElement.
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
            );

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
        // We can't use a free function here because we need to capture closures that
        // send requests — so we inline the build.  The per-session data (id, autonomy,
        // state) is cloned into each listener closure.
        let build_row = |s: &Session, show_buttons: bool, requests: Sender<Request>, focused_id: Option<SessionId>| {
            let sid_auto = s.id.clone();
            let sid_approve = s.id.clone();
            let sid_deny = s.id.clone();
            let sid_focus = s.id.clone();
            let current_autonomy = s.autonomy;
            let is_focused = focused_id.as_ref() == Some(&s.id);

            let (auto_label, auto_color) = autonomy_badge(s.autonomy);
            let dot_c = dot_color(&s.state);
            let branch_label = s
                .branch
                .as_deref()
                .unwrap_or("-")
                .to_owned();
            let tool_label = format!("{:?}", s.tool);
            let name_label = s.name.clone();
            let activity_label = s.activity.clone();

            let tx_auto = requests.clone();
            let tx_approve = requests.clone();
            let tx_deny = requests.clone();
            let tx_focus = requests.clone();

            let row_bg = if is_focused { color::BORDER } else { color::SIDE };

            // Autonomy pill button — on click cycles autonomy.
            let autonomy_pill = div()
                .text_xs()
                .px_1()
                .rounded_md()
                .text_color(rgb(auto_color))
                .bg(rgb(color::HEAD))
                .child(SharedString::from(auto_label))
                .on_mouse_down(
                    MouseButton::Left,
                    move |_ev: &MouseDownEvent, _window, _cx| {
                        let next = match current_autonomy {
                            Autonomy::Manual => Autonomy::Guarded,
                            Autonomy::Guarded => Autonomy::Auto,
                            Autonomy::Auto => Autonomy::Manual,
                        };
                        let _ = tx_auto.try_send(Request::SetAutonomy {
                            session: sid_auto.clone(),
                            level: next,
                        });
                    },
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
                        move |_ev: &MouseDownEvent, _window, _cx| {
                            let _ = tx_approve.try_send(Request::Decide {
                                session: sid_approve.clone(),
                                approve: true,
                                instruction: None,
                            });
                        },
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
                        move |_ev: &MouseDownEvent, _window, _cx| {
                            let _ = tx_deny.try_send(Request::Decide {
                                session: sid_deny.clone(),
                                approve: false,
                                instruction: None,
                            });
                        },
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

            // The full card — clicking focuses the session in the terminal pane.
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
                    move |_ev: &MouseDownEvent, _window, _cx| {
                        // NOTE: we cannot call self.focus_session here — listeners passed to
                        // on_mouse_down outside cx.listener don't have Entity access.
                        // Sending RequestGrid causes Output events → apply() auto-sets focused.
                        let _ = tx_focus.try_send(Request::RequestGrid(sid_focus.clone()));
                    },
                )
        };

        // Build group sections.
        let group_label = |text: &'static str, color: u32| {
            div()
                .text_xs()
                .px_2()
                .py_1()
                .text_color(rgb(color))
                .child(SharedString::from(text))
        };

        let tx = requests_tx.clone();
        let f_id = focused_id.clone();
        let needs_rows = needs_you_sessions
            .iter()
            .map(|s| build_row(s, true, tx.clone(), f_id.clone()))
            .collect::<Vec<_>>();

        let tx2 = requests_tx.clone();
        let f_id2 = focused_id.clone();
        let working_rows = working_sessions
            .iter()
            .map(|s| build_row(s, false, tx2.clone(), f_id2.clone()))
            .collect::<Vec<_>>();

        let tx3 = requests_tx.clone();
        let f_id3 = focused_id.clone();
        let done_rows = done_sessions
            .iter()
            .map(|s| build_row(s, false, tx3.clone(), f_id3.clone()))
            .collect::<Vec<_>>();

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

        // ── Root ───────────────────────────────────────────────────────────────
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(color::BG))
            .text_color(rgb(color::TEXT))
            .child(title_bar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(terminal_pane)
                    .child(sidebar),
            )
            .child(status_line)
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    Application::new().run(|cx: &mut App| {
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
