//! Translate Claude Code hook events into session-state transitions and autonomy
//! decisions. For `PreToolUse` the daemon either auto-allows (per [`protocol::safety`])
//! or escalates: it publishes a pending decision and parks the hook on a oneshot until a
//! human resolves it via `Request::Decide`.

use std::sync::Arc;

use protocol::{safety, DecisionKind, HookDecision, HookEnvelope, HookKind, HookReply, SessionId, State, Tool};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::registry::Registry;

pub async fn handle_hook(reg: &Arc<Registry>, env: HookEnvelope) -> HookReply {
    let v: Value = serde_json::from_str(&env.payload_json).unwrap_or(Value::Null);
    let session = reg.ensure(&env.session, None, Tool::Claude);

    match env.kind {
        HookKind::PreToolUse => {
            let tool = v
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("Bash")
                .to_string();
            let command = extract_command(&tool, v.get("tool_input"));
            let risk = safety::classify(&tool, &command);
            let outcome = safety::decide(session.autonomy, risk);
            let kind = DecisionKind::Permission {
                tool: tool.clone(),
                command: command.clone(),
            };

            if outcome.is_allow() {
                reg.set_state(
                    &env.session,
                    State::Working,
                    format!("running {}: {}", tool, short(&command)),
                );
                reg.emit_auto(env.session.clone(), kind, true, outcome.reason());
                HookReply {
                    decision: Some(HookDecision::Allow),
                }
            } else {
                reg.set_state(
                    &env.session,
                    State::NeedsInput(kind.clone()),
                    format!("wants {}: {}", tool, short(&command)),
                );
                reg.emit_decision_pending(env.session.clone(), kind);

                let (tx, rx) = oneshot::channel();
                reg.register_pending(env.session.clone(), tx);
                match rx.await {
                    Ok(decision) => HookReply {
                        decision: Some(decision),
                    },
                    // sender dropped (session closed / superseded) → safest is to deny.
                    Err(_) => HookReply {
                        decision: Some(HookDecision::Deny {
                            reason: "decision channel closed".into(),
                        }),
                    },
                }
            }
        }

        HookKind::Notification => {
            maybe_update_cost(reg, &env.session, &v);
            let msg = v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("waiting for input")
                .to_string();
            reg.set_state(
                &env.session,
                State::NeedsInput(DecisionKind::Question { prompt: msg.clone() }),
                msg,
            );
            HookReply { decision: None }
        }

        HookKind::PostToolUse => {
            maybe_update_cost(reg, &env.session, &v);
            reg.set_state(&env.session, State::Working, "working");
            HookReply { decision: None }
        }

        HookKind::Stop => {
            maybe_update_cost(reg, &env.session, &v);
            reg.set_state(&env.session, State::Done, "finished — awaiting next prompt");
            HookReply { decision: None }
        }

        HookKind::SessionEnd => {
            reg.remove(&env.session);
            HookReply { decision: None }
        }

        HookKind::UserPromptSubmit => {
            reg.set_state(&env.session, State::Working, "thinking…");
            HookReply { decision: None }
        }
    }
}

/// Attempt to extract a cost figure from a hook payload and update the session.
///
/// Checked locations (first match wins):
/// 1. `total_cost_usd` — top-level numeric field (Claude Code Stop / PostToolUse)
/// 2. `cost` — top-level numeric field
/// 3. `usage.total_cost_usd` — nested under `usage`
///
/// Silently does nothing if none of the paths resolves to a number.
fn maybe_update_cost(reg: &Arc<Registry>, id: &SessionId, v: &Value) {
    let cost = v
        .get("total_cost_usd")
        .and_then(Value::as_f64)
        .or_else(|| v.get("cost").and_then(Value::as_f64))
        .or_else(|| {
            v.get("usage")
                .and_then(|u| u.get("total_cost_usd"))
                .and_then(Value::as_f64)
        });
    if let Some(usd) = cost {
        reg.set_cost(id, usd);
    }
}

/// Pull the human-meaningful command/target out of a tool_input object.
fn extract_command(tool: &str, tool_input: Option<&Value>) -> String {
    let Some(input) = tool_input else {
        return String::new();
    };
    match tool {
        "Bash" => input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => input
            .get("file_path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "Read" | "Glob" | "Grep" => input
            .get("file_path")
            .or_else(|| input.get("pattern"))
            .or_else(|| input.get("path"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => input.to_string(),
    }
}

fn short(s: &str) -> String {
    const MAX: usize = 80;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(MAX - 1).collect();
        t.push('…');
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Autonomy, Session, SessionId};

    fn reg_with(autonomy: Autonomy) -> (Arc<Registry>, SessionId) {
        let reg = Arc::new(Registry::new());
        let id = SessionId(1);
        reg.insert(Session {
            id: id.clone(),
            name: "t".into(),
            tool: Tool::Claude,
            state: State::Working,
            autonomy,
            branch: None,
            activity: String::new(),
            cost_usd: 0.0,
            context_frac: None,
        });
        (reg, id)
    }

    fn pretooluse(cmd: &str) -> String {
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": cmd }
        })
        .to_string()
    }

    #[tokio::test]
    async fn guarded_auto_allows_a_safe_command() {
        let (reg, id) = reg_with(Autonomy::Guarded);
        let reply = handle_hook(
            &reg,
            HookEnvelope {
                session: id.clone(),
                kind: HookKind::PreToolUse,
                payload_json: pretooluse("ls -la"),
            },
        )
        .await;
        assert_eq!(reply.decision, Some(HookDecision::Allow));
    }

    #[tokio::test]
    async fn guarded_escalates_rm_rf_and_resolves_to_human_decision() {
        let (reg, id) = reg_with(Autonomy::Guarded);
        let reg2 = reg.clone();
        let id2 = id.clone();
        // Simulate the human denying shortly after the hook parks.
        let resolver = tokio::spawn(async move {
            // wait until the hook has registered its pending sender
            for _ in 0..200 {
                if let Some(tx) = reg2.take_pending(&id2) {
                    tx.send(HookDecision::Deny {
                        reason: "nope".into(),
                    })
                    .unwrap();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            panic!("hook never registered a pending decision");
        });

        let reply = handle_hook(
            &reg,
            HookEnvelope {
                session: id,
                kind: HookKind::PreToolUse,
                payload_json: pretooluse("rm -rf /"),
            },
        )
        .await;
        resolver.await.unwrap();
        assert_eq!(
            reply.decision,
            Some(HookDecision::Deny {
                reason: "nope".into()
            })
        );
    }

    #[tokio::test]
    async fn auto_still_escalates_rm_rf() {
        // Even Auto must not auto-approve an irreversible command: the hook should park
        // (not return Allow immediately). We assert it does NOT resolve without a human.
        let (reg, id) = reg_with(Autonomy::Auto);
        let fut = handle_hook(
            &reg,
            HookEnvelope {
                session: id.clone(),
                kind: HookKind::PreToolUse,
                payload_json: pretooluse("rm -rf build"),
            },
        );
        tokio::pin!(fut);
        // It must still be pending after a beat (parked awaiting the human).
        let parked = tokio::time::timeout(std::time::Duration::from_millis(50), &mut fut).await;
        assert!(parked.is_err(), "Auto must escalate rm -rf, not auto-allow");
        // clean up: resolve so the task can finish
        if let Some(tx) = reg.take_pending(&id) {
            let _ = tx.send(HookDecision::Deny { reason: "cleanup".into() });
        }
        let _ = fut.await;
    }

    #[tokio::test]
    async fn stop_hook_updates_cost_from_total_cost_usd() {
        let (reg, id) = reg_with(Autonomy::Guarded);
        handle_hook(
            &reg,
            HookEnvelope {
                session: id.clone(),
                kind: HookKind::Stop,
                payload_json: serde_json::json!({ "total_cost_usd": 0.042 }).to_string(),
            },
        )
        .await;
        let sess = reg.get(&id).expect("session gone");
        assert!(
            (sess.cost_usd - 0.042).abs() < 1e-9,
            "cost not updated: {}",
            sess.cost_usd
        );
    }

    #[tokio::test]
    async fn stop_hook_updates_cost_from_nested_usage() {
        let (reg, id) = reg_with(Autonomy::Guarded);
        handle_hook(
            &reg,
            HookEnvelope {
                session: id.clone(),
                kind: HookKind::Stop,
                payload_json: serde_json::json!({ "usage": { "total_cost_usd": 1.23 } })
                    .to_string(),
            },
        )
        .await;
        let sess = reg.get(&id).expect("session gone");
        assert!(
            (sess.cost_usd - 1.23).abs() < 1e-9,
            "cost not updated: {}",
            sess.cost_usd
        );
    }

    #[tokio::test]
    async fn stop_hook_with_no_cost_field_is_silent() {
        let (reg, id) = reg_with(Autonomy::Guarded);
        handle_hook(
            &reg,
            HookEnvelope {
                session: id.clone(),
                kind: HookKind::Stop,
                payload_json: serde_json::json!({ "message": "done" }).to_string(),
            },
        )
        .await;
        let sess = reg.get(&id).expect("session gone");
        assert_eq!(sess.cost_usd, 0.0, "cost should remain 0 when absent");
    }
}
