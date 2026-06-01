//! The `Daemon` ties the [`Registry`] (metadata + events) to the live [`PtySession`]
//! map, and turns protocol requests into process actions (spawn / input / resize / stop).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use portable_pty::CommandBuilder;
use protocol::{DecisionKind, Session, SessionId, SpawnSpec, State, Target, Tool};

use crate::claude;
use crate::pty::PtySession;
use crate::registry::Registry;
use crate::shellinit;
use crate::tools;

pub struct Daemon {
    pub reg: Arc<Registry>,
    ptys: Mutex<HashMap<SessionId, Arc<PtySession>>>,
    pub sock_path: PathBuf,
    /// where per-session Claude settings files are written
    run_dir: PathBuf,
    /// path to the `fleetterm-hook` binary
    hook_bin: PathBuf,
    /// The session a UI is currently viewing (for live grid push).
    /// Updated by [`Daemon::request_grid`]; cleared when the session is closed.
    watched: Mutex<Option<SessionId>>,
}

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

impl Daemon {
    pub fn new(sock_path: PathBuf, run_dir: PathBuf, hook_bin: PathBuf) -> Arc<Self> {
        Arc::new(Daemon {
            reg: Arc::new(Registry::new()),
            ptys: Mutex::new(HashMap::new()),
            sock_path,
            run_dir,
            hook_bin,
            watched: Mutex::new(None),
        })
    }

    pub fn pty(&self, id: &SessionId) -> Option<Arc<PtySession>> {
        self.ptys.lock().unwrap().get(id).cloned()
    }

    /// Spawn a new session per `spec`, register it, and stream its output.
    pub fn spawn(self: &Arc<Self>, spec: SpawnSpec) -> Result<SessionId> {
        let id = self.reg.alloc_id();
        let mut cmd = self.build_command(&spec, id.clone())?;

        cmd.env("FLEETTERM_SESSION", id.0.to_string());
        cmd.env("FLEETTERM_SOCK", self.sock_path.to_string_lossy().to_string());
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        // Best-effort shell integration: inject OSC 133 hooks for bash/zsh.
        // We intentionally ignore errors — a failure here must not abort the spawn.
        let _shell_init = if matches!(spec.tool, Tool::Shell) {
            shellinit::apply(&mut cmd, id.0)
                .unwrap_or_else(|e| {
                    tracing::warn!(session = %id, "shell integration unavailable: {e}");
                    None
                })
        } else {
            None
        };

        let reg = self.reg.clone();
        let cb_id = id.clone();
        let block_reg = self.reg.clone();
        let block_id = id.clone();
        let session = PtySession::spawn(
            cmd,
            DEFAULT_COLS,
            DEFAULT_ROWS,
            move |chunk| {
                reg.emit_output(cb_id.clone(), chunk);
            },
            move |marker| {
                block_reg.emit_block(block_id.clone(), marker);
            },
        )?;

        self.ptys.lock().unwrap().insert(id.clone(), session.clone());

        let name = spec.name.clone().unwrap_or_else(|| default_name(spec.tool, &id));
        self.reg.insert(Session {
            id: id.clone(),
            name,
            tool: spec.tool,
            state: State::Working,
            autonomy: spec.autonomy,
            branch: spec.worktree_from.clone(),
            activity: "starting…".into(),
            cost_usd: 0.0,
            context_frac: None,
        });

        if let Some(opening) = &spec.opening {
            let mut line = opening.clone();
            line.push('\r');
            let _ = session.write_input(line.as_bytes());
        }

        // Live-grid watcher: while this session is the one the UI is watching,
        // push a fresh Event::Grid every ~80 ms so the focused terminal updates
        // in real-time without the UI needing to poll via RequestGrid.
        //
        // Safety note: grid_snapshot() acquires the Term lock. The reader thread
        // in PtySession::spawn releases the Term lock *before* calling on_output
        // (see pty.rs: the `{ let mut t = term.lock()... }` block closes before
        // `on_output(chunk.to_vec())` on the next line), so there is no deadlock
        // between the reader thread and this task calling grid_snapshot().
        {
            // `spawn()` takes `self: &Arc<Self>`, so cloning `self` gives an Arc clone.
            let watch_daemon = self.clone();
            let watch_session = session.clone();
            let watch_id = id.clone();
            let watch_reg = self.reg.clone();
            tokio::spawn(async move {
                let interval = tokio::time::Duration::from_millis(80);
                loop {
                    tokio::time::sleep(interval).await;

                    // Stop if the session has been removed from the registry.
                    if watch_reg.get(&watch_id).is_none() {
                        break;
                    }

                    // Only emit a grid if this session is the one being watched.
                    let is_watched = {
                        let w = watch_daemon.watched.lock().unwrap();
                        w.as_ref() == Some(&watch_id)
                    };
                    if !is_watched {
                        continue;
                    }

                    let (cols, rows, cursor_col, cursor_row, cells) =
                        watch_session.grid_snapshot();
                    watch_reg.emit_grid(
                        watch_id.clone(),
                        cols,
                        rows,
                        cursor_col,
                        cursor_row,
                        cells,
                    );
                }
            });
        }

        // For tools that lack hook support (anything except Claude), spawn a
        // background task that polls the terminal grid every 1500 ms and runs
        // the heuristic state-inference logic.
        if !matches!(spec.tool, Tool::Claude) {
            let poll_reg = self.reg.clone();
            let poll_session = session.clone();
            let poll_id = id.clone();
            let poll_tool = spec.tool;
            tokio::spawn(async move {
                let poll_interval = tokio::time::Duration::from_millis(1500);
                let mut last_line = String::new();
                let mut last_change = std::time::Instant::now();

                loop {
                    tokio::time::sleep(poll_interval).await;

                    // Stop polling if the session is gone from the registry.
                    if poll_reg.get(&poll_id).is_none() {
                        break;
                    }

                    let current_line = poll_session.last_nonempty_line();
                    if current_line != last_line {
                        last_line = current_line.clone();
                        last_change = std::time::Instant::now();
                    }
                    let idle = last_change.elapsed();

                    if let Some(new_state) = tools::infer_state(poll_tool, &current_line, idle) {
                        let activity = match &new_state {
                            State::Stuck => "no progress detected".to_string(),
                            State::NeedsInput(DecisionKind::Question { prompt }) => {
                                format!("waiting: {}", prompt)
                            }
                            _ => String::new(),
                        };
                        poll_reg.set_state(&poll_id, new_state, activity);
                    }
                }
            });
        }

        Ok(id)
    }

    fn build_command(&self, spec: &SpawnSpec, id: SessionId) -> Result<CommandBuilder> {
        match spec.tool {
            Tool::Shell => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".into());
                Ok(CommandBuilder::new(shell))
            }
            Tool::Claude => {
                let settings = claude::write_settings(&self.run_dir, id, &self.hook_bin)?;
                let mut cmd = CommandBuilder::new("claude");
                cmd.arg("--settings");
                cmd.arg(settings.to_string_lossy().to_string());
                if let Some(model) = &spec.model {
                    cmd.arg("--model");
                    cmd.arg(model);
                }
                Ok(cmd)
            }
            // codex/aider/gemini get first-class adapters in Phase 4; for now spawn bare.
            Tool::Codex => Ok(CommandBuilder::new("codex")),
            Tool::Aider => Ok(CommandBuilder::new("aider")),
            Tool::Gemini => Ok(CommandBuilder::new("gemini")),
            Tool::Other => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".into());
                Ok(CommandBuilder::new(shell))
            }
        }
    }

    pub fn input(&self, target: &Target, data: &[u8]) {
        for id in self.reg.resolve_targets(target) {
            if let Some(p) = self.pty(&id) {
                let _ = p.write_input(data);
            }
        }
    }

    pub fn resize(&self, id: &SessionId, cols: u16, rows: u16) {
        if let Some(p) = self.pty(id) {
            let _ = p.resize(cols, rows);
        }
    }

    pub fn pause(&self, target: &Target) {
        for id in self.reg.resolve_targets(target) {
            if let Some(p) = self.pty(&id) {
                p.pause();
            }
            self.reg.set_state(&id, State::Working, "paused");
        }
    }

    pub fn resume(&self, target: &Target) {
        for id in self.reg.resolve_targets(target) {
            if let Some(p) = self.pty(&id) {
                p.resume();
            }
            self.reg.set_state(&id, State::Working, "resumed");
        }
    }

    /// Build and emit an [`Event::Grid`] for the given session from its current PTY grid.
    ///
    /// Also marks this session as the **watched** session, activating the live-grid
    /// push task (see [`Daemon::spawn`]) so subsequent output changes are pushed to
    /// subscribers at ~80 ms cadence without further `RequestGrid` calls.
    pub fn request_grid(&self, id: &SessionId) {
        // Mark this session as the one the UI is currently viewing.
        *self.watched.lock().unwrap() = Some(id.clone());

        if let Some(p) = self.pty(id) {
            let (cols, rows, cursor_col, cursor_row, cells) = p.grid_snapshot();
            self.reg.emit_grid(id.clone(), cols, rows, cursor_col, cursor_row, cells);
        }
    }

    pub fn stop(&self, target: &Target) {
        for id in self.reg.resolve_targets(target) {
            if let Some(p) = self.pty(&id) {
                p.kill();
            }
            self.reg.set_state(&id, State::Dead, "stopped");
        }
    }

    pub fn close(&self, id: &SessionId) {
        if let Some(p) = self.ptys.lock().unwrap().remove(id) {
            p.kill();
        }
        // If this session was being watched, clear the watch so the live-grid task
        // stops emitting (it also stops on reg.get() returning None, but clearing here
        // is immediate and avoids one spurious emit after removal).
        {
            let mut w = self.watched.lock().unwrap();
            if w.as_ref() == Some(id) {
                *w = None;
            }
        }
        self.reg.remove(id);
    }
}

fn default_name(tool: Tool, id: &SessionId) -> String {
    let prefix = match tool {
        Tool::Shell => "shell",
        Tool::Claude => "claude",
        Tool::Codex => "codex",
        Tool::Aider => "aider",
        Tool::Gemini => "gemini",
        Tool::Other => "agent",
    };
    format!("{prefix}-{}", id.0)
}
