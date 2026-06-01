//! `fleetterm-hook` — the tiny binary Claude Code invokes for each registered hook.
//!
//! FleetTerm spawns `claude --settings <generated.json>` where every hook's `command`
//! is this binary, and sets `FLEETTERM_SESSION` + `FLEETTERM_SOCK` in the child env.
//! On each hook firing Claude pipes the event JSON on our stdin; we:
//!   1. read the payload + the `hook_event_name` field,
//!   2. forward a [`Frame::Hook`] to the daemon over the unix socket,
//!   3. block for a [`HookReply`]; for `PreToolUse` the daemon may hold this open until
//!      the human (or the autonomy engine) decides, then we translate the decision into
//!      the JSON Claude expects on stdout.
//!
//! Fail-open: if anything goes wrong (no daemon, bad env), we exit 0 emitting nothing,
//! so Claude falls back to its own native permission prompt rather than hanging.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use protocol::codec;
use protocol::{Frame, HookDecision, HookEnvelope, HookKind, HookReply, SessionId};

fn main() {
    // Never let a hook failure break the agent: any error path → exit 0, no output.
    if let Err(e) = run() {
        eprintln!("fleetterm-hook: {e}");
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut payload = String::new();
    std::io::stdin().read_to_string(&mut payload)?;

    let session = match std::env::var("FLEETTERM_SESSION").ok().and_then(|s| s.parse::<u64>().ok()) {
        Some(id) => SessionId(id),
        None => return Ok(()), // not launched by FleetTerm — no-op
    };
    let sock = match std::env::var("FLEETTERM_SOCK") {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };

    let kind = parse_event_kind(&payload).unwrap_or(HookKind::Notification);
    let envelope = HookEnvelope { session, kind, payload_json: payload };

    // Connect, send the envelope, await the reply.
    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(_) => return Ok(()), // daemon down → fall back to Claude's own prompt
    };
    stream.set_read_timeout(Some(Duration::from_secs(600)))?; // PreToolUse may wait on a human
    codec::write_frame(&mut stream, &Frame::Hook(envelope))?;

    let reply: HookReply = match codec::read_frame(&mut stream) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };

    if let Some(decision) = reply.decision {
        emit_decision(kind, decision);
    }
    Ok(())
}

/// Map Claude's `hook_event_name` to our [`HookKind`].
fn parse_event_kind(payload: &str) -> Option<HookKind> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let name = v.get("hook_event_name")?.as_str()?;
    Some(match name {
        "PreToolUse" => HookKind::PreToolUse,
        "PostToolUse" => HookKind::PostToolUse,
        "Notification" => HookKind::Notification,
        "Stop" | "SubagentStop" => HookKind::Stop,
        "SessionEnd" => HookKind::SessionEnd,
        "UserPromptSubmit" => HookKind::UserPromptSubmit,
        _ => HookKind::Notification,
    })
}

/// Print the JSON Claude Code reads to allow/deny a `PreToolUse`.
///
/// NOTE: field names follow the current Claude Code hook spec
/// (`hookSpecificOutput.permissionDecision` = "allow" | "deny", with
/// `permissionDecisionReason`). To be reconciled against the grounding research;
/// the decision itself originates in the daemon's autonomy engine.
fn emit_decision(kind: HookKind, decision: HookDecision) {
    if kind != HookKind::PreToolUse {
        return;
    }
    let out = match decision {
        HookDecision::Allow => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "permissionDecisionReason": "FleetTerm autonomy: approved"
            }
        }),
        HookDecision::Deny { reason } => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason
            }
        }),
    };
    println!("{out}");
}
