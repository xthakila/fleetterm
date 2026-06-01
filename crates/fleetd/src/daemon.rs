//! The `Daemon` ties the [`Registry`] (metadata + events) to the live [`PtySession`]
//! map, and turns protocol requests into process actions (spawn / input / resize / stop).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use portable_pty::CommandBuilder;
use protocol::{Session, SessionId, SpawnSpec, State, Target, Tool};

use crate::claude;
use crate::pty::PtySession;
use crate::registry::Registry;

pub struct Daemon {
    pub reg: Arc<Registry>,
    ptys: Mutex<HashMap<SessionId, Arc<PtySession>>>,
    pub sock_path: PathBuf,
    /// where per-session Claude settings files are written
    run_dir: PathBuf,
    /// path to the `fleetterm-hook` binary
    hook_bin: PathBuf,
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

        let reg = self.reg.clone();
        let cb_id = id.clone();
        let session = PtySession::spawn(cmd, DEFAULT_COLS, DEFAULT_ROWS, move |chunk| {
            reg.emit_output(cb_id.clone(), chunk);
        })?;

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
