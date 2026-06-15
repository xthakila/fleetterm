# FleetTerm

**An LLM-first terminal.** A from-scratch, GPU-native terminal emulator you'd run as your
daily driver — that also happens to be a cockpit for governing a fleet of CLI coding agents
(Claude Code first; codex/aider/gemini next).

Every other tool in this space makes you the multiplexer: you scroll pane to pane, hunt for
the agent that's blocked, and type into each window. FleetTerm treats **agents as first-class
processes the terminal is built around** — with a persistent fleet sidebar, attention routing,
and per-agent **autonomy** so you *supervise by exception* instead of babysitting.

> Status: **working v1.** P0–P6 shipped — GPU terminal (styled cell grid + keyboard +
> scrollback), fleet cockpit (sidebar, decisions inbox, autonomy, views, ⌘K palette,
> composer), multi-tool, git-worktree spawn, and inter-agent pipelines. 66 tests green;
> runs live. See [roadmap](#roadmap). Repo: https://github.com/xthakila/fleetterm

## Why it's different

- **Supervise-by-exception autonomy** — each agent is 🔒 Manual / 🛡 Guarded / 🚀 Auto. Safe
  actions auto-approve; risky ones escalate; **irreversible ones (`rm -rf`, force-push, …)
  always ask, even on Auto** (see `crates/protocol/src/safety.rs`).
- **The fleet is the terminal** — a governed roster with a decisions inbox and one input that
  addresses `@name` or `@all`, not panes you navigate between.
- **Local, private, open, Linux-first, agent-agnostic** — no cloud, no account, no vendor
  agent. (The gap Warp's cloud model and cmux's macOS-only build leave open.)
- **Light + native** — GPU-rendered, embeds a proven VT engine; designed to a hard perf budget
  so it wins as a *plain* terminal first.

## Architecture

```
fleetterm (GPUI app)  ──unix socket (msgpack)──►  fleetd (daemon)  ──►  per-tool adapters
  tabs · terminal · fleet sidebar · status line     owns PTYs + state         Claude (hooks)
  embeds alacritty_terminal for VT                  autonomy engine            codex/aider/gemini (PTY heuristics)
                                                    ▲
                                   fleetterm-hook ──┘  (forwards Claude hook events; blocks for PreToolUse decisions)
```

The daemon owns the PTYs so agents survive the UI closing (detach/reattach), and the
socket protocol is transport-agnostic so remote/federation is a later transport swap.

## Workspace

| Crate | Role |
|---|---|
| `crates/protocol` | Wire types (UI↔daemon↔hook), msgpack framing, and the **autonomy safety classifier**. |
| `crates/fleetd` | The daemon: PTY manager, session/state machine, autonomy engine, socket server. |
| `crates/fleetterm` | The GPU terminal emulator (GPUI) + fleet UI. |
| `crates/fleetterm-hook` | Tiny forwarder Claude invokes per hook event; blocks for PreToolUse decisions. |

## Build

```bash
cargo test                    # protocol + safety unit tests
cargo run -p fleetd           # the daemon (later)
cargo run -p fleetterm        # the terminal (later)
```

## Tech

Rust · [GPUI](https://www.gpui.rs/) (UI) · [`alacritty_terminal`](https://docs.rs/alacritty_terminal) (VT engine) ·
`portable-pty` · tokio · msgpack. Linux-first (Wayland/X11, Vulkan).

## Status (2026-06-02)

Daemon proven headless (**32 tests**); GPUI window **launches on Wayland, connects, and
renders the live fleet**. Build: `cargo test` (daemon) green; `cargo run -p fleetd` + `cargo
run -p fleetterm` for the cockpit. Needs `libxkbcommon-x11-dev` to link the UI.

```bash
target/debug/fleetd &                 # daemon (owns PTYs + state + autonomy)
target/debug/examples/seed            # spawn a few demo sessions
target/debug/fleetterm                # the cockpit window
```

## Roadmap

- **P0** GPUI window + native render — ✅ builds + opens + renders on this box.
- **P1** daemon owns PTYs · grid-snapshot streaming — ✅ (detach/reattach + OSC 133 blocks pending).
- **P2** Claude rich lane: hooks → state → fleet sidebar (read + approve/deny) — ✅.
- **P3** Autonomy engine + pause/resume + per-session controls — ✅ (`@all` composer pending). *(the leap)*
- **P4** Multi-tool heuristic adapters (codex/aider/gemini) — ✅ (spawn presets/worktrees pending).
- **P5** Split/Tiled/Focus views · `⌘K` palette — ✅.
- **terminal** styled cell-grid renderer + keyboard→PTY input + live focused grid + **scrollback** (mouse wheel) — ✅.
- **composer** `@name`/`@all`/`@working` fan-out input · typeable palette · OSC-133 command-block indicator — ✅.
- **worktrees** git-worktree-per-agent spawn (clean-only removal) — ✅.
- **P6** inter-agent **pipelines** (spawn B after A finishes) + process-exit→Done — ✅ · remote/federation — *(future)*.
- **next** render OSC-133 as full command blocks · terminal-history scroll depth · remote/federation · live real-`claude`-in-GUI demo (first-run onboarding handling).

UI look & feel: see the mockups in `~/fleetterm-v2.html` (canonical) and the gallery
`~/fleetterm-index.html`.
