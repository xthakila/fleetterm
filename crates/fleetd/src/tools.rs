//! Multi-tool heuristic state inference for agents that lack hook support
//! (codex, aider, gemini, plain shells, and any `Tool::Other`).
//!
//! Rather than tracking explicit lifecycle events (which Claude exposes via hooks but
//! other tools do not), the daemon polls the terminal's last visible line periodically
//! and uses conservative pattern-matching to guess the session's state. The daemon only
//! applies a result when it differs from the current state, so a steady state never spams.

use std::time::Duration;

use protocol::{DecisionKind, State, Tool};

// ---------------------------------------------------------------------------
// Public inference function
// ---------------------------------------------------------------------------

/// Infer a session state from the terminal's last non-empty line and idle duration.
/// Returns `Some(new_state)` if the heuristic fires, or `None` meaning "leave as-is".
///
/// Rules (first match wins):
/// 1. **NeedsInput(Question)** — the last line looks like a blocking prompt
///    (`(y/n)`, `[Y/n]`, `?`, "continue", "Press …"). An interactive prompt wins even
///    when also idle, because the session is genuinely waiting on the human.
/// 2. **Idle** — quiet for longer than [`IDLE_THRESHOLD`] with no prompt. This is the
///    calm "ready / nothing happening" state, NOT an alarm. We deliberately do **not**
///    infer `Stuck` from silence alone: a shell or agent sitting at a prompt is normal,
///    and crying "stuck" on every idle session is worse than staying quiet. (Genuine
///    hang detection needs a stronger signal — e.g. CPU-busy-but-frozen — and is left
///    to a future iteration.)
/// 3. Otherwise — `None` (recent output → leave whatever state it has).
pub fn infer_state(_tool: Tool, last_line: &str, idle: Duration) -> Option<State> {
    // Rule 1: a blocking prompt wins (even if also idle).
    if is_waiting_for_input(last_line) {
        return Some(State::NeedsInput(DecisionKind::Question {
            prompt: last_line.trim().to_string(),
        }));
    }

    // Rule 2: quiet for a while with no prompt → idle/ready.
    if idle > IDLE_THRESHOLD {
        return Some(State::Idle);
    }

    None
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// How long with no new output before a session is considered idle/ready.
const IDLE_THRESHOLD: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Return true if `line` looks like a blocking prompt the user must answer.
fn is_waiting_for_input(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_lowercase();
    if lower.ends_with("(y/n)")
        || lower.ends_with("[y/n]")
        || lower.ends_with("[y/n]:")
        || lower.ends_with("[yes/no]")
        || lower.ends_with('?')
    {
        return true;
    }

    if lower.contains("continue") || lower.contains("press ") {
        return true;
    }

    false
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_needs_input(line: &str) {
        let result = infer_state(Tool::Codex, line, Duration::from_secs(0));
        assert!(
            matches!(result, Some(State::NeedsInput(DecisionKind::Question { .. }))),
            "expected NeedsInput for line: {line:?}, got {result:?}"
        );
    }

    fn assert_none(line: &str) {
        let result = infer_state(Tool::Aider, line, Duration::from_secs(0));
        assert!(result.is_none(), "expected None for line: {line:?}, got {result:?}");
    }

    #[test]
    fn yn_prompt_triggers_needs_input() {
        assert_needs_input("Overwrite existing file? (y/n)");
        assert_needs_input("Are you sure? [Y/n]");
        assert_needs_input("Delete all? [y/N]");
    }

    #[test]
    fn question_mark_suffix_triggers_needs_input() {
        assert_needs_input("Do you want to continue?");
        assert_needs_input("Ready to proceed?");
    }

    #[test]
    fn continue_and_press_keywords_trigger_needs_input() {
        assert_needs_input("Press any key to continue...");
        assert_needs_input("Press Enter to confirm.");
    }

    #[test]
    fn long_idle_with_no_prompt_is_idle_not_stuck() {
        // Regression: a quiet session is Idle/ready, NOT "Stuck". (Found by launching:
        // seeded shells sitting at a prompt were wrongly flagged Stuck.)
        let result = infer_state(Tool::Gemini, "user@host:~$", IDLE_THRESHOLD + Duration::from_secs(1));
        assert_eq!(result, Some(State::Idle));
    }

    #[test]
    fn idle_under_threshold_and_no_pattern_returns_none() {
        let result = infer_state(Tool::Codex, "compiling main.rs", Duration::from_secs(5));
        assert!(result.is_none(), "expected None for recent output");
    }

    #[test]
    fn empty_line_returns_none() {
        assert_none("");
        assert_none("   ");
    }

    #[test]
    fn prompt_beats_idle_when_both_apply() {
        // A blocking prompt that is also idle → NeedsInput (the human must answer), not Idle.
        let result = infer_state(Tool::Other, "Continue? (y/n)", IDLE_THRESHOLD + Duration::from_millis(1));
        assert!(matches!(result, Some(State::NeedsInput(_))));
    }

    #[test]
    fn exactly_at_threshold_is_not_idle() {
        // Boundary: strictly greater-than, so exactly == threshold is not yet idle.
        let result = infer_state(Tool::Codex, "running tests", IDLE_THRESHOLD);
        assert!(result.is_none(), "exactly at threshold should not be idle, got {result:?}");
    }
}
