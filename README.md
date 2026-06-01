# FleetTerm

**An LLM-first terminal.** A from-scratch, GPU-native terminal emulator you'd run as your
daily driver — that also happens to be a cockpit for governing a fleet of CLI coding agents
(Claude Code first; codex/aider/gemini next).

Every other tool in this space makes you the multiplexer: you scroll pane to pane, hunt for
the agent that's blocked, and type into each window. FleetTerm treats **agents as first-class
processes the terminal is built around** — with a persistent fleet sidebar, attention routing,
and per-agent **autonomy** so you *supervise by exception* instead of babysitting.

> Status: **early build.** Phase 0–1 (terminal core + daemon) in progress. See
> [the plan](#roadmap) and `~/.claude/plans/lively-puzzling-pretzel.md`.

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

## Roadmap

- **P0** GPUI window + one terminal session, hit the perf budget. *(in progress)*
- **P1** Tabs · daemon owns PTYs · detach/reattach · command blocks (OSC 133).
- **P2** Claude rich lane: hooks → state → fleet sidebar (read + approve/deny).
- **P3** Autonomy engine + bulk actions + `@name`/`@all` composer. *(the leap)*
- **P4** Multi-tool adapters · spawn presets · worktrees.
- **P5** Tiled/Focus views · command palette · governance · polish.
- **P6** *(deferred)* inter-agent orchestration · remote/federation.

UI look & feel: see the mockups in `~/fleetterm-v2.html` (canonical) and the gallery
`~/fleetterm-index.html`.
