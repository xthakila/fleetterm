//! Autonomy safety: classify how dangerous a proposed action is, and map that against
//! a session's [`Autonomy`] level to decide whether FleetTerm may auto-approve it or
//! must escalate to the human.
//!
//! Trust invariant: **`Risk::NeverAuto` always escalates, at every autonomy level,
//! including `Auto`.** No setting can make FleetTerm silently run an irreversible
//! command. This module is heavily unit-tested because it is the line between
//! "helpful" and "deleted the repo".

use crate::Autonomy;
use serde::{Deserialize, Serialize};

/// How risky an action is, judged conservatively from the tool + command text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Risk {
    /// Read-only / trivially reversible (ls, cat, grep, build, test, git status/diff).
    Safe,
    /// Mutating but recoverable, or reaching the network/system (installs, sudo,
    /// git push, writes). Auto-approved only under [`Autonomy::Auto`].
    Risky,
    /// Irreversible or catastrophic (rm -rf, force-push, dd, mkfs, fork bomb,
    /// pipe-to-shell, disk writes). NEVER auto-approved — always escalates.
    NeverAuto,
}

/// The outcome of consulting autonomy: either FleetTerm may proceed without the human,
/// or it must surface a decision and wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Auto-approve; `reason` explains why (for the audit log).
    Allow(&'static str),
    /// Escalate to the human; `reason` explains why.
    Escalate(&'static str),
}

impl Outcome {
    pub fn reason(&self) -> &'static str {
        match self {
            Outcome::Allow(r) | Outcome::Escalate(r) => r,
        }
    }
    pub fn is_allow(&self) -> bool {
        matches!(self, Outcome::Allow(_))
    }
}

/// Substrings that mark an irreversible/catastrophic command. Matched case-insensitively
/// against a normalized (whitespace-collapsed) command line. Conservative by design —
/// false positives merely ask the human; false negatives could destroy data.
const NEVER_AUTO_PATTERNS: &[&str] = &[
    "rm -rf",
    "rm -fr",
    "rm -r -f",
    "rm -f -r",
    "rm --recursive --force",
    "sudo rm",
    "git push --force",
    "git push -f",
    "push --force-with-lease", // still destructive to shared history
    "git reset --hard",
    "git clean -fd",
    "git clean -df",
    "mkfs",
    "dd if=",
    "dd of=",
    " > /dev/sd",
    " > /dev/nvme",
    "shred ",
    "truncate -s 0",
    ":(){",        // fork bomb
    ":(){:|:&};:",
    "chmod -r 777 /",
    "chmod 777 /",
    "chown -r ",
    "drop table",
    "drop database",
    "truncate table",
    "delete from",  // SQL without our knowing the WHERE — escalate
    "format ",
    "diskpart",
    "shutdown",
    "reboot",
    "halt",
    "init 0",
    "init 6",
];

/// Substrings that mark a piped/remote-exec or system-mutating action: recoverable-ish
/// but must not run unattended under Guarded.
const RISKY_PATTERNS: &[&str] = &[
    "curl ",
    "wget ",
    "| sh",
    "| bash",
    "|sh",
    "|bash",
    "sudo ",
    "apt install",
    "apt-get install",
    "dnf install",
    "yum install",
    "pacman -s",
    "npm install -g",
    "npm i -g",
    "pip install",
    "cargo install",
    "brew install",
    "git push",
    "docker run",
    "docker rm",
    "systemctl",
    "kill ",
    "killall",
    "pkill",
    "mv ",
    "chmod ",
    "chown ",
    "ssh ",
    "scp ",
];

fn normalize(command: &str) -> String {
    let lowered = command.trim().to_lowercase();
    // collapse runs of whitespace to single spaces so "rm   -rf" matches "rm -rf"
    let mut out = String::with_capacity(lowered.len());
    let mut prev_space = false;
    for ch in lowered.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

/// Classify a tool invocation. `tool` is the tool name (e.g. "Bash", "Edit", "Read");
/// `command` is the command line or primary argument. Non-Bash tools are judged by
/// their nature: reads are Safe, writes/edits are Risky (recoverable), etc.
pub fn classify(tool: &str, command: &str) -> Risk {
    let cmd = normalize(command);

    // Catastrophic markers win regardless of tool.
    if NEVER_AUTO_PATTERNS.iter().any(|p| cmd.contains(p)) {
        return Risk::NeverAuto;
    }

    match tool {
        // Read-only Claude tools.
        "Read" | "Glob" | "Grep" | "LS" | "NotebookRead" | "TodoWrite" | "WebFetch"
        | "WebSearch" => Risk::Safe,
        // File mutations: recoverable (git/undo) but not unattended under Guarded.
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => Risk::Risky,
        // Bash / everything else: judge by the command text.
        _ => {
            if RISKY_PATTERNS.iter().any(|p| cmd.contains(p)) {
                Risk::Risky
            } else if is_known_safe_bash(&cmd) {
                Risk::Safe
            } else {
                // Unknown command under an unknown tool: be conservative, treat as Risky.
                Risk::Risky
            }
        }
    }
}

/// A small allowlist of obviously read-only / reversible shell command heads.
fn is_known_safe_bash(cmd: &str) -> bool {
    const SAFE_HEADS: &[&str] = &[
        "ls", "cat", "head", "tail", "grep", "rg", "fd", "find", "echo", "pwd", "cd",
        "which", "type", "file", "stat", "wc", "diff", "tree", "env", "date", "whoami",
        "git status", "git diff", "git log", "git show", "git branch", "git add",
        "git commit", "git fetch", "git stash", "git checkout", "git switch",
        "cargo build", "cargo check", "cargo test", "cargo clippy", "cargo fmt",
        "npm test", "npm run", "pytest", "make", "go build", "go test",
    ];
    SAFE_HEADS.iter().any(|h| cmd == *h || cmd.starts_with(&format!("{h} ")))
}

/// The core decision: given the session's autonomy and the action's risk, may we proceed?
///
/// | risk \ level | Manual   | Guarded  | Auto     |
/// |--------------|----------|----------|----------|
/// | Safe         | Escalate | Allow    | Allow    |
/// | Risky        | Escalate | Escalate | Allow    |
/// | NeverAuto    | Escalate | Escalate | Escalate |
pub fn decide(autonomy: Autonomy, risk: Risk) -> Outcome {
    match risk {
        Risk::NeverAuto => Outcome::Escalate("irreversible action — always asks"),
        Risk::Safe => match autonomy {
            Autonomy::Manual => Outcome::Escalate("manual mode asks for everything"),
            Autonomy::Guarded | Autonomy::Auto => Outcome::Allow("safe action"),
        },
        Risk::Risky => match autonomy {
            Autonomy::Manual => Outcome::Escalate("manual mode asks for everything"),
            Autonomy::Guarded => Outcome::Escalate("risky action under guarded"),
            Autonomy::Auto => Outcome::Allow("risky action allowed under auto"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rm_rf_is_never_auto_even_in_auto_mode() {
        for variant in ["rm -rf build/", "rm   -rf  /", "RM -RF .", "sudo rm -rf x", "rm -fr node_modules"] {
            assert_eq!(classify("Bash", variant), Risk::NeverAuto, "{variant}");
            assert!(
                matches!(decide(Autonomy::Auto, classify("Bash", variant)), Outcome::Escalate(_)),
                "auto must still escalate: {variant}"
            );
        }
    }

    #[test]
    fn force_push_and_disk_ops_never_auto() {
        for c in ["git push --force origin main", "git push -f", "dd if=/dev/zero of=/dev/sda", "mkfs.ext4 /dev/sdb", ":(){:|:&};:"] {
            assert_eq!(classify("Bash", c), Risk::NeverAuto, "{c}");
        }
    }

    #[test]
    fn pipe_to_shell_and_installs_are_risky() {
        for c in ["curl https://x.sh | sh", "sudo apt install foo", "pip install requests", "git push origin feat"] {
            assert_eq!(classify("Bash", c), Risk::Risky, "{c}");
        }
    }

    #[test]
    fn reads_and_builds_are_safe() {
        assert_eq!(classify("Read", "/etc/hosts"), Risk::Safe);
        assert_eq!(classify("Grep", "TODO"), Risk::Safe);
        assert_eq!(classify("Bash", "ls -la"), Risk::Safe);
        assert_eq!(classify("Bash", "git status"), Risk::Safe);
        assert_eq!(classify("Bash", "cargo test -p protocol"), Risk::Safe);
    }

    #[test]
    fn edits_are_risky_not_safe() {
        assert_eq!(classify("Edit", "src/main.rs"), Risk::Risky);
        assert_eq!(classify("Write", "Cargo.toml"), Risk::Risky);
    }

    #[test]
    fn decision_table_is_correct() {
        use Autonomy::*;
        use Risk::*;
        assert!(decide(Guarded, Safe).is_allow());
        assert!(!decide(Manual, Safe).is_allow());
        assert!(!decide(Guarded, Risky).is_allow());
        assert!(decide(Auto, Risky).is_allow());
        assert!(!decide(Auto, NeverAuto).is_allow());
        assert!(!decide(Manual, NeverAuto).is_allow());
    }

    #[test]
    fn unknown_command_is_conservative() {
        // a command we don't recognize and that isn't on the safe head list → Risky, not Safe
        assert_eq!(classify("Bash", "frobnicate --all"), Risk::Risky);
    }
}
