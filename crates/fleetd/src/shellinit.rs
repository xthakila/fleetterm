//! Shell-integration snippets that emit OSC 133 sequences.
//!
//! For sessions spawned with [`protocol::Tool::Shell`] the daemon writes a small
//! rc-file to a temporary path and passes it to the shell on launch.  This is
//! completely transparent to the user and best-effort: if the shell cannot load the
//! file (e.g. non-bash/zsh shells) it simply starts without block-marking.
//!
//! # Shell-specific approach
//!
//! | Shell | Mechanism |
//! |-------|-----------|
//! | bash  | `--rcfile <tmpfile>` replaces `~/.bashrc`; the snippet sources `~/.bashrc` first. |
//! | zsh   | `ZDOTDIR=<tmpdir>` env var; the temp dir holds `.zshrc` that sources `$HOME/.zshrc`. |
//! | other | No modification; the shell starts without OSC 133 support. |
//!
//! # OSC 133 hooks
//!
//! * **bash**: `PROMPT_COMMAND` wrapper emits A (prompt start) + B (command start).
//!   A `DEBUG` trap emits C (output start) on first invocation per command.
//!   D (command end, with exit code) is emitted at the top of the next `precmd`.
//! * **zsh**: `precmd` hook emits D + A + B; `preexec` hook emits C.
//!
//! The OSC sequence format is `ESC ] 133 ; <param> BEL` (`\a`).

use std::io::Write;
use std::path::{Path, PathBuf};

use portable_pty::CommandBuilder;

/// Opaque handle for the temporary files written for a session's shell integration.
/// The daemon can keep this alive for the session lifetime or let it drop — the OS
/// will reclaim `/tmp` files; we do not unlink them explicitly.
pub struct ShellInitFiles {
    /// The path the shell was pointed at (rc-file for bash, ZDOTDIR dir for zsh).
    pub init_path: PathBuf,
}

/// Set up OSC 133 shell integration for a shell spawn.
///
/// Modifies `cmd` in-place (adds args / env vars) and writes temporary init files.
/// Returns `Ok(Some(files))` for bash/zsh, `Ok(None)` for unsupported shells
/// (no modification to `cmd` is made in that case).
///
/// All errors are I/O errors writing the temp file — the caller should log and
/// proceed without integration rather than aborting the session.
pub fn apply(cmd: &mut CommandBuilder, session_id: u64) -> std::io::Result<Option<ShellInitFiles>> {
    let shell_prog = std::env::var("SHELL").unwrap_or_default();
    let shell_name = Path::new(&shell_prog)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("sh")
        .to_string();

    match shell_name.as_str() {
        "bash" => apply_bash(cmd, session_id).map(Some),
        "zsh" => apply_zsh(cmd, session_id).map(Some),
        _ => Ok(None), // fish, sh, dash, tcsh, etc. — no-op
    }
}

// ---------------------------------------------------------------------------
// Bash integration
// ---------------------------------------------------------------------------

/// OSC 133 integration snippet for bash, loaded via `--rcfile`.
///
/// We source `~/.bashrc` first so the user's aliases, PATH, and prompt are
/// preserved.  Then we install our hooks without replacing any existing
/// `PROMPT_COMMAND` or `DEBUG` trap.
const BASH_SNIPPET: &str = concat!(
    "# FleetTerm shell integration (OSC 133)\n",
    "if [ -f \"$HOME/.bashrc\" ] && [ \"$_FLEETTERM_SOURCING\" != \"1\" ]; then\n",
    "    _FLEETTERM_SOURCING=1 source \"$HOME/.bashrc\"\n",
    "fi\n",
    "\n",
    "_ft_initialized=0\n",
    "_ft_cmd_running=0\n",
    "\n",
    "# Emit an OSC 133 sequence: ESC ] 133 ; <param> BEL\n",
    "_ft_osc() { printf '\\033]133;%s\\007' \"$1\"; }\n",
    "\n",
    "# Runs before every prompt (via PROMPT_COMMAND).\n",
    "_ft_precmd() {\n",
    "    local exit_code=$?\n",
    "    if [ \"$_ft_initialized\" = \"1\" ] && [ \"$_ft_cmd_running\" = \"1\" ]; then\n",
    "        _ft_osc \"D;${exit_code}\"\n",
    "        _ft_cmd_running=0\n",
    "    fi\n",
    "    _ft_initialized=1\n",
    "    _ft_osc \"A\"\n",
    "}\n",
    "\n",
    "# Runs before every command (via DEBUG trap). Only fires once per command.\n",
    "_ft_preexec() {\n",
    "    if [ \"$_ft_cmd_running\" = \"0\" ] && [ \"$_ft_initialized\" = \"1\" ]; then\n",
    "        _ft_osc \"C\"\n",
    "        _ft_cmd_running=1\n",
    "    fi\n",
    "}\n",
    "\n",
    "# Wire hooks — preserve any existing PROMPT_COMMAND.\n",
    "if [ -z \"$PROMPT_COMMAND\" ]; then\n",
    "    PROMPT_COMMAND=\"_ft_precmd; _ft_osc B\"\n",
    "else\n",
    "    PROMPT_COMMAND=\"_ft_precmd; _ft_osc B; ${PROMPT_COMMAND}\"\n",
    "fi\n",
    "\n",
    "# Install DEBUG trap only if none already exists (e.g. bash-preexec).\n",
    "if [ -z \"$(trap -p DEBUG)\" ]; then\n",
    "    trap '_ft_preexec' DEBUG\n",
    "fi\n",
);

fn apply_bash(cmd: &mut CommandBuilder, session_id: u64) -> std::io::Result<ShellInitFiles> {
    let path = write_temp_file(
        &format!("fleetterm-bash-{session_id}.sh"),
        BASH_SNIPPET,
    )?;
    cmd.arg("--rcfile");
    cmd.arg(path.to_string_lossy().to_string());
    Ok(ShellInitFiles { init_path: path })
}

// ---------------------------------------------------------------------------
// Zsh integration
// ---------------------------------------------------------------------------

/// OSC 133 integration snippet for zsh, written as `.zshrc` inside a temp `ZDOTDIR`.
///
/// We source `$HOME/.zshrc` first to preserve the user's environment, then
/// install hooks via `add-zsh-hook`.
const ZSH_SNIPPET: &str = concat!(
    "# FleetTerm shell integration (OSC 133)\n",
    "if [ -f \"$HOME/.zshrc\" ] && [ \"$_FLEETTERM_SOURCING\" != \"1\" ]; then\n",
    "    _FLEETTERM_SOURCING=1 source \"$HOME/.zshrc\"\n",
    "fi\n",
    "\n",
    "autoload -Uz add-zsh-hook\n",
    "\n",
    "_ft_initialized=0\n",
    "_ft_osc() { printf '\\033]133;%s\\007' \"$1\"; }\n",
    "\n",
    "_ft_precmd() {\n",
    "    local exit_code=$?\n",
    "    if [ \"$_ft_initialized\" = \"1\" ]; then\n",
    "        _ft_osc \"D;${exit_code}\"\n",
    "    fi\n",
    "    _ft_initialized=1\n",
    "    _ft_osc \"A\"\n",
    "    _ft_osc \"B\"\n",
    "}\n",
    "\n",
    "_ft_preexec() {\n",
    "    _ft_osc \"C\"\n",
    "}\n",
    "\n",
    "add-zsh-hook precmd _ft_precmd\n",
    "add-zsh-hook preexec _ft_preexec\n",
);

fn apply_zsh(cmd: &mut CommandBuilder, session_id: u64) -> std::io::Result<ShellInitFiles> {
    let dir = std::env::temp_dir().join(format!("fleetterm-zsh-{session_id}"));
    std::fs::create_dir_all(&dir)?;
    let rc_path = dir.join(".zshrc");
    {
        let mut f = std::fs::File::create(&rc_path)?;
        f.write_all(ZSH_SNIPPET.as_bytes())?;
    }
    cmd.env("ZDOTDIR", dir.to_string_lossy().to_string());
    Ok(ShellInitFiles { init_path: dir })
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn write_temp_file(name: &str, content: &str) -> std::io::Result<PathBuf> {
    let path = std::env::temp_dir().join(name);
    let mut f = std::fs::File::create(&path)?;
    f.write_all(content.as_bytes())?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_snippet_sources_user_bashrc() {
        assert!(
            BASH_SNIPPET.contains("source \"$HOME/.bashrc\""),
            "bash snippet must source the user's .bashrc"
        );
    }

    #[test]
    fn bash_snippet_has_prompt_command_hook() {
        assert!(BASH_SNIPPET.contains("PROMPT_COMMAND"));
        assert!(BASH_SNIPPET.contains("_ft_precmd"));
    }

    #[test]
    fn bash_snippet_emits_all_osc133_params() {
        for param in &["A", "B", "C", "D;"] {
            assert!(
                BASH_SNIPPET.contains(param),
                "bash snippet must emit OSC 133 param '{param}'"
            );
        }
    }

    #[test]
    fn bash_snippet_uses_bel_terminator() {
        // \007 = BEL in octal escape.
        assert!(
            BASH_SNIPPET.contains("\\007"),
            "bash snippet must use BEL (\\007) as OSC terminator"
        );
    }

    #[test]
    fn zsh_snippet_sources_user_zshrc() {
        assert!(
            ZSH_SNIPPET.contains("source \"$HOME/.zshrc\""),
            "zsh snippet must source the user's .zshrc"
        );
    }

    #[test]
    fn zsh_snippet_uses_add_zsh_hook() {
        assert!(ZSH_SNIPPET.contains("add-zsh-hook precmd _ft_precmd"));
        assert!(ZSH_SNIPPET.contains("add-zsh-hook preexec _ft_preexec"));
    }

    #[test]
    fn zsh_snippet_emits_all_osc133_params() {
        for param in &["A", "B", "C", "D;"] {
            assert!(
                ZSH_SNIPPET.contains(param),
                "zsh snippet must emit OSC 133 param '{param}'"
            );
        }
    }

    #[test]
    fn zsh_snippet_uses_bel_terminator() {
        assert!(
            ZSH_SNIPPET.contains("\\007"),
            "zsh snippet must use BEL (\\007) as OSC terminator"
        );
    }
}
