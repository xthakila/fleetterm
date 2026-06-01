//! Generate the per-session Claude Code settings file that wires every hook to
//! `fleetterm-hook`. We pass this via `claude --settings <file>` so the user's global
//! `~/.claude/settings.json` is never touched. `FLEETTERM_SESSION`/`FLEETTERM_SOCK` are
//! set in the child env and inherited by the hook subprocess.
//!
//! Schema verified against Claude Code's hooks docs (grounding research):
//! `{ "hooks": { "<Event>": [ { "matcher": "", "hooks": [ { "type":"command", "command": ... } ] } ] } }`.

use std::path::{Path, PathBuf};

use protocol::SessionId;
use serde_json::{json, Value};

/// Build the settings JSON registering `hook_bin` for every lifecycle event we consume.
pub fn settings_json(hook_bin: &Path) -> Value {
    let cmd = hook_bin.to_string_lossy().to_string();
    // matcher "" matches all tools / all notifications.
    let group = |matcher: &str| {
        json!({
            "matcher": matcher,
            "hooks": [ { "type": "command", "command": cmd } ]
        })
    };
    json!({
        "hooks": {
            "PreToolUse":       [ group("") ],
            "PostToolUse":      [ group("") ],
            "Notification":     [ group("") ],
            "Stop":             [ group("") ],
            "SessionEnd":       [ group("") ],
            "UserPromptSubmit": [ group("") ]
        }
    })
}

/// Write the settings file for a session under `dir`, returning its path.
pub fn write_settings(dir: &Path, session: SessionId, hook_bin: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("session-{}.settings.json", session.0));
    let body = serde_json::to_vec_pretty(&settings_json(hook_bin))?;
    std::fs::write(&path, body)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_register_pretooluse_command() {
        let v = settings_json(Path::new("/usr/local/bin/fleetterm-hook"));
        let cmd = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert_eq!(cmd, "/usr/local/bin/fleetterm-hook");
        assert_eq!(
            v["hooks"]["PreToolUse"][0]["hooks"][0]["type"].as_str().unwrap(),
            "command"
        );
    }
}
