//! The `Daemon` ties the [`Registry`] (metadata + events) to the live [`PtySession`]
//! map, and turns protocol requests into process actions (spawn / input / resize / stop).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    /// Tracks git worktrees created for sessions: session id → (repo root, worktree dir).
    /// Populated in [`Daemon::spawn`] when `SpawnSpec::worktree_from` is set;
    /// consumed in [`Daemon::close`] for best-effort cleanup.
    worktrees: Mutex<HashMap<SessionId, (PathBuf, PathBuf)>>,
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
            worktrees: Mutex::new(HashMap::new()),
        })
    }

    pub fn pty(&self, id: &SessionId) -> Option<Arc<PtySession>> {
        self.ptys.lock().unwrap().get(id).cloned()
    }

    /// Spawn a new session per `spec`, register it, and stream its output.
    pub fn spawn(self: &Arc<Self>, spec: SpawnSpec) -> Result<SessionId> {
        let id = self.reg.alloc_id();
        let mut cmd = self.build_command(&spec, id.clone())?;

        // --- git worktree setup (best-effort; failure falls back to spec.cwd) -----------
        //
        // When `worktree_from` is set, we create a linked worktree for this session so
        // parallel agents each get an isolated working copy.  All errors warn + fall back;
        // they must never abort the spawn.
        let (effective_cwd, session_branch) = if let Some(ref base) = spec.worktree_from {
            // Determine the repo root: spec.cwd if given, else current dir.
            let repo = spec
                .cwd
                .as_ref()
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

            // Verify it's inside a git repo.
            match run_git(&["-C", &repo.to_string_lossy(), "rev-parse", "--is-inside-work-tree"]) {
                Ok(out) if out.trim() == "true" => {
                    // Build worktree dir and branch name.
                    let sname = spec
                        .name
                        .as_deref()
                        .unwrap_or("agent")
                        .to_string();
                    let safe_name = sanitize_name(&sname);
                    let branch = format!("fleetterm/{}-{}", safe_name, id.0);
                    let worktree_dir = repo
                        .join(".fleetterm")
                        .join("worktrees")
                        .join(format!("{}-{}", safe_name, id.0));

                    // Create parent dirs.
                    if let Err(e) = std::fs::create_dir_all(worktree_dir.parent().unwrap_or(&worktree_dir)) {
                        tracing::warn!(session = %id, "worktree: could not create parent dir: {e}");
                        (spec.cwd.as_ref().map(PathBuf::from), spec.worktree_from.clone())
                    } else {
                        // git worktree add -b <branch> <dir> <base>
                        match run_git(&[
                            "-C",
                            &repo.to_string_lossy(),
                            "worktree",
                            "add",
                            "-b",
                            &branch,
                            &worktree_dir.to_string_lossy(),
                            base,
                        ]) {
                            Ok(_) => {
                                tracing::info!(
                                    session = %id,
                                    worktree = %worktree_dir.display(),
                                    branch = %branch,
                                    "git worktree created"
                                );
                                self.worktrees
                                    .lock()
                                    .unwrap()
                                    .insert(id.clone(), (repo, worktree_dir.clone()));
                                (Some(worktree_dir), Some(branch))
                            }
                            Err(e) => {
                                tracing::warn!(
                                    session = %id,
                                    "worktree add failed ({e}); falling back to spec.cwd"
                                );
                                (spec.cwd.as_ref().map(PathBuf::from), spec.worktree_from.clone())
                            }
                        }
                    }
                }
                Ok(out) => {
                    tracing::warn!(
                        session = %id,
                        "worktree: not inside a git work tree (got {:?}); falling back",
                        out.trim()
                    );
                    (spec.cwd.as_ref().map(PathBuf::from), spec.worktree_from.clone())
                }
                Err(e) => {
                    tracing::warn!(
                        session = %id,
                        "worktree: git rev-parse failed ({e}); falling back to spec.cwd"
                    );
                    (spec.cwd.as_ref().map(PathBuf::from), spec.worktree_from.clone())
                }
            }
        } else {
            (spec.cwd.as_ref().map(PathBuf::from), None)
        };
        // ---------------------------------------------------------------------------------

        cmd.env("FLEETTERM_SESSION", id.0.to_string());
        cmd.env("FLEETTERM_SOCK", self.sock_path.to_string_lossy().to_string());
        if let Some(ref cwd) = effective_cwd {
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
            // Use the actual branch we created (or None if worktree setup was not
            // requested / fell back).  This is the real worktree branch name when
            // created successfully, the original `worktree_from` value on fallback,
            // or None when no worktree was requested.
            branch: session_branch,
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

        // Best-effort worktree cleanup.  We only remove the worktree if it is clean
        // (no uncommitted changes).  On any failure we warn and leave the directory
        // intact — never destroy uncommitted work.
        if let Some((repo, worktree_dir)) = self.worktrees.lock().unwrap().remove(id) {
            cleanup_worktree(id, &repo, &worktree_dir);
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

// ---------------------------------------------------------------------------
// Git worktree helpers
// ---------------------------------------------------------------------------

/// Run a git command, passing all `args` verbatim.
///
/// This intentionally takes the `-C <dir>` argument as part of `args` so callers
/// can include it (or omit it) explicitly without additional indirection.
///
/// Returns `Ok(stdout)` on exit-code 0, or an error containing stderr.
fn run_git(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("git exec failed: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(anyhow::anyhow!(
            "git {} exited {}: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Replace characters that are invalid in git branch name path components with `-`,
/// then collapse runs of `-` and strip leading/trailing `-`.
///
/// Git branch names must not contain `..`, `~`, `^`, `:`, `?`, `*`, `[`, `\`,
/// spaces, control chars, or start/end with `.` or `/`.  This function keeps only
/// ASCII alphanumeric chars and `.` (safe inside a component), replacing everything
/// else with `-`.
fn sanitize_name(name: &str) -> String {
    let raw: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Collapse consecutive dashes and trim.
    let mut result = String::with_capacity(raw.len());
    let mut last_dash = true; // treat start as dash to strip leading dashes
    for c in raw.chars() {
        if c == '-' {
            if !last_dash {
                result.push('-');
                last_dash = true;
            }
        } else {
            result.push(c);
            last_dash = false;
        }
    }
    // Strip trailing dash.
    if result.ends_with('-') {
        result.pop();
    }
    if result.is_empty() {
        "agent".into()
    } else {
        result
    }
}

/// Best-effort worktree cleanup called from [`Daemon::close`].
///
/// Rules:
/// - If `git status --porcelain` returns empty output → worktree is clean → remove it.
/// - If there are uncommitted changes → warn and leave the directory untouched.
/// - On any git error → warn and leave (safe default).
fn cleanup_worktree(id: &SessionId, repo: &Path, worktree_dir: &Path) {
    // First check: does the worktree directory still exist?
    if !worktree_dir.exists() {
        return;
    }

    let dir_str = worktree_dir.to_string_lossy();

    // Check cleanliness.
    match run_git(&["-C", &dir_str, "status", "--porcelain"]) {
        Ok(output) if output.trim().is_empty() => {
            // Clean — safe to remove.
            match run_git(&[
                "-C",
                &repo.to_string_lossy(),
                "worktree",
                "remove",
                &dir_str,
            ]) {
                Ok(_) => {
                    tracing::info!(
                        session = %id,
                        worktree = %worktree_dir.display(),
                        "git worktree removed (clean)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        session = %id,
                        worktree = %worktree_dir.display(),
                        "could not remove worktree: {e}"
                    );
                }
            }
        }
        Ok(_) => {
            tracing::warn!(
                session = %id,
                "left dirty worktree {} — contains uncommitted changes, not removing",
                worktree_dir.display()
            );
        }
        Err(e) => {
            tracing::warn!(
                session = %id,
                "could not check worktree status for {}: {e}",
                worktree_dir.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // sanitize_name — pure function, no I/O
    // -----------------------------------------------------------------------

    #[test]
    fn sanitize_name_strips_special_chars() {
        assert_eq!(sanitize_name("my agent"), "my-agent");
        assert_eq!(sanitize_name("fix/auth~bug"), "fix-auth-bug");
        assert_eq!(sanitize_name("---leading"), "leading");
        assert_eq!(sanitize_name("trailing---"), "trailing");
        assert_eq!(sanitize_name("a--b"), "a-b");
        assert_eq!(sanitize_name(""), "agent");
        assert_eq!(sanitize_name("---"), "agent");
        assert_eq!(sanitize_name("hello"), "hello");
        assert_eq!(sanitize_name("v1.2.3"), "v1.2.3");
    }

    // -----------------------------------------------------------------------
    // Branch and worktree path construction
    // -----------------------------------------------------------------------

    #[test]
    fn branch_name_construction() {
        let id = SessionId(42);
        let safe_name = sanitize_name("my agent");
        let branch = format!("fleetterm/{}-{}", safe_name, id.0);
        assert_eq!(branch, "fleetterm/my-agent-42");
    }

    #[test]
    fn worktree_dir_construction() {
        let repo = PathBuf::from("/home/user/myproject");
        let id = SessionId(7);
        let safe_name = sanitize_name("fix/auth bug");
        let worktree_dir = repo
            .join(".fleetterm")
            .join("worktrees")
            .join(format!("{}-{}", safe_name, id.0));
        assert_eq!(
            worktree_dir,
            PathBuf::from("/home/user/myproject/.fleetterm/worktrees/fix-auth-bug-7")
        );
    }

    // -----------------------------------------------------------------------
    // run_git — integration test against a real temp repo.
    //
    // These tests use only `git` (already required to develop this project)
    // and temp dirs; they create no PTYs and no tokio runtime.
    // -----------------------------------------------------------------------

    /// Create a temporary git repo with one commit and return its path.
    fn init_temp_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().to_string_lossy().to_string();

        // git init
        std::process::Command::new("git")
            .args(["-C", &p, "init"])
            .output()
            .unwrap();
        // configure identity for the commit
        std::process::Command::new("git")
            .args(["-C", &p, "config", "user.email", "test@test.local"])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["-C", &p, "config", "user.name", "Test"])
            .output()
            .unwrap();
        // initial commit so HEAD is valid
        let readme = dir.path().join("README");
        std::fs::write(&readme, "init").unwrap();
        std::process::Command::new("git")
            .args(["-C", &p, "add", "README"])
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["-C", &p, "commit", "-m", "init"])
            .output()
            .unwrap();

        dir
    }

    #[test]
    fn run_git_succeeds_on_valid_repo() {
        let repo = init_temp_repo();
        let p = repo.path().to_string_lossy().to_string();
        let out = run_git(&["-C", &p, "rev-parse", "--is-inside-work-tree"]).unwrap();
        assert_eq!(out.trim(), "true");
    }

    #[test]
    fn run_git_fails_on_non_repo() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().to_string();
        let result = run_git(&["-C", &p, "rev-parse", "--is-inside-work-tree"]);
        assert!(result.is_err(), "expected error for non-git dir");
    }

    #[test]
    fn worktree_add_and_remove_lifecycle() {
        let repo_dir = init_temp_repo();
        let repo = repo_dir.path();
        let repo_str = repo.to_string_lossy().to_string();

        let id = SessionId(999);
        let safe_name = sanitize_name("test-agent");
        let branch = format!("fleetterm/{}-{}", safe_name, id.0);
        let worktree_dir = repo
            .join(".fleetterm")
            .join("worktrees")
            .join(format!("{}-{}", safe_name, id.0));

        std::fs::create_dir_all(worktree_dir.parent().unwrap()).unwrap();

        // Add the worktree.
        let result = run_git(&[
            "-C",
            &repo_str,
            "worktree",
            "add",
            "-b",
            &branch,
            &worktree_dir.to_string_lossy(),
            "HEAD",
        ]);
        assert!(result.is_ok(), "worktree add failed: {:?}", result);

        // The worktree directory must exist and be a linked worktree.
        assert!(worktree_dir.exists(), "worktree dir should exist");
        let list_out = run_git(&["-C", &repo_str, "worktree", "list", "--porcelain"]).unwrap();
        assert!(
            list_out.contains(&worktree_dir.to_string_lossy().to_string()),
            "worktree not in list"
        );

        // Status should be clean immediately after creation.
        let status = run_git(&["-C", &worktree_dir.to_string_lossy(), "status", "--porcelain"])
            .unwrap();
        assert!(status.trim().is_empty(), "fresh worktree should be clean");

        // Cleanup via cleanup_worktree.
        cleanup_worktree(&id, repo, &worktree_dir);

        // After clean removal the dir should no longer exist.
        assert!(
            !worktree_dir.exists(),
            "worktree dir should be removed after clean close"
        );
    }

    #[test]
    fn cleanup_worktree_leaves_dirty_worktree_intact() {
        let repo_dir = init_temp_repo();
        let repo = repo_dir.path();
        let repo_str = repo.to_string_lossy().to_string();

        let id = SessionId(888);
        let safe_name = sanitize_name("dirty-agent");
        let branch = format!("fleetterm/{}-{}", safe_name, id.0);
        let worktree_dir = repo
            .join(".fleetterm")
            .join("worktrees")
            .join(format!("{}-{}", safe_name, id.0));

        std::fs::create_dir_all(worktree_dir.parent().unwrap()).unwrap();

        run_git(&[
            "-C",
            &repo_str,
            "worktree",
            "add",
            "-b",
            &branch,
            &worktree_dir.to_string_lossy(),
            "HEAD",
        ])
        .unwrap();

        // Dirty the worktree: create an untracked file.
        std::fs::write(worktree_dir.join("dirty.txt"), "uncommitted").unwrap();

        // cleanup_worktree should leave it in place.
        cleanup_worktree(&id, repo, &worktree_dir);

        assert!(
            worktree_dir.exists(),
            "dirty worktree must NOT be removed — would lose uncommitted work"
        );
    }
}
