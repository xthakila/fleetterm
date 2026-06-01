//! Multi-tool heuristic state inference for agents that lack hook support
//! (codex, aider, gemini, and any `Tool::Other`).
//!
//! Rather than tracking explicit lifecycle events (which Claude exposes via
//! hooks but other tools do not), we poll the terminal grid periodically and
//! use conservative pattern-matching on the last visible line to guess the
//! agent's current state.  False positives are cheap (we merely ask a human);
//! false negatives leave the state as-is.

use std::time::Duration;

use protocol::{DecisionKind, State, Tool};

// ---------------------------------------------------------------------------
// Public inference function
// ---------------------------------------------------------------------------

/// Infer a session state from the terminal's last non-empty line and idle
/// duration.  Returns `Some(new_state)` if the heuristic fires, or `None`
/// meaning "leave the current state unchanged".
///
/// Rules (evaluated in order, first match wins):
/// 1. **Stuck** — `idle > 90s`: the grid has been frozen for a long time.
/// 2. **NeedsInput(Question)** — the last line matches a prompt pattern:
///    ends with `(y/n)`, `[Y/n]`, `[y/N]`, `?`, or contains the words
///    `"continue"`, `"Press"`.
/// 3. Otherwise — `None`.
pub fn infer_state(
    _tool: Tool,
    last_line: &str,
    idle: Duration,
) -> Option<State> {
    // Rule 1: stuck if idle too long with no new output.
    if idle > STUCK_THRESHOLD {
        return Some(State::Stuck);
    }

    // Rule 2: prompt patterns suggest the agent is waiting for user input.
    if is_waiting_for_input(last_line) {
        return Some(State::NeedsInput(DecisionKind::Question {
            prompt: last_line.trim().to_string(),
        }));
    }

    None
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// How long a frozen grid (no new output) must persist before we declare Stuck.
const STUCK_THRESHOLD: Duration = Duration::from_secs(90);

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Return true if `line` looks like a blocking prompt the user must answer.
fn is_waiting_for_input(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Suffix patterns: "(y/n)", "[Y/n]", "[y/N]", "[yes/no]", "?" …
    let lower = trimmed.to_lowercase();
    if lower.ends_with("(y/n)")
        || lower.ends_with("[y/n]")
        || lower.ends_with("[y/n]:")
        || lower.ends_with("[yes/no]")
        || lower.ends_with('?')
    {
        return true;
    }

    // Substring patterns.
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
        assert!(
            result.is_none(),
            "expected None for line: {line:?}, got {result:?}"
        );
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
    fn continue_keyword_triggers_needs_input() {
        assert_needs_input("Press any key to continue...");
        assert_needs_input("Do you want to continue with this operation?");
    }

    #[test]
    fn press_keyword_triggers_needs_input() {
        assert_needs_input("Press Enter to confirm.");
        assert_needs_input("Press q to quit.");
    }

    #[test]
    fn idle_over_threshold_triggers_stuck() {
        let result = infer_state(Tool::Gemini, "working on it", STUCK_THRESHOLD + Duration::from_secs(1));
        assert_eq!(result, Some(State::Stuck), "expected Stuck after long idle");
    }

    #[test]
    fn idle_under_threshold_and_no_pattern_returns_none() {
        let result = infer_state(Tool::Codex, "compiling main.rs", Duration::from_secs(30));
        assert!(result.is_none(), "expected None for normal working line");
    }

    #[test]
    fn empty_line_returns_none() {
        assert_none("");
        assert_none("   ");
    }

    #[test]
    fn stuck_beats_prompt_pattern_when_both_apply() {
        // If idle > threshold AND the line matches a prompt, Stuck fires first.
        let result = infer_state(
            Tool::Other,
            "Continue? (y/n)",
            STUCK_THRESHOLD + Duration::from_millis(1),
        );
        assert_eq!(result, Some(State::Stuck));
    }

    #[test]
    fn exactly_at_threshold_is_not_stuck() {
        // Boundary: strictly greater than, so exactly == threshold is not stuck.
        let result = infer_state(Tool::Codex, "running tests", STUCK_THRESHOLD);
        assert!(
            result.is_none(),
            "exactly at threshold should not be stuck, got {result:?}"
        );
    }
}
