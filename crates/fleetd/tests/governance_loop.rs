//! End-to-end proof of the agent-governance loop over the real unix socket:
//! a UI subscribes, a (simulated) hook fires PreToolUse, the daemon either auto-allows
//! (safe) or escalates → emits DecisionPending → the UI decides → the parked hook is
//! unblocked with the matching decision. This is the core of Phases 2–3.

use std::path::PathBuf;
use std::time::Duration;

use fleetd::daemon::Daemon;
use fleetd::{framed, server};
use protocol::{
    Autonomy, Event, Frame, HookDecision, HookEnvelope, HookKind, HookReply, Request, Session,
    SessionId, State, Tool,
};
use tokio::net::UnixStream;
use tokio::time::timeout;

fn temp_daemon(tag: &str) -> (std::sync::Arc<Daemon>, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("fleetterm-it-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("d.sock");
    let daemon = Daemon::new(sock.clone(), dir.join("sessions"), PathBuf::from("fleetterm-hook"));
    (daemon, sock, dir)
}

fn guarded_session(id: u64) -> Session {
    Session {
        id: SessionId(id),
        name: format!("s{id}"),
        tool: Tool::Claude,
        state: State::Working,
        autonomy: Autonomy::Guarded,
        branch: None,
        activity: String::new(),
        cost_usd: 0.0,
        context_frac: None,
    }
}

async fn await_socket(sock: &PathBuf) {
    for _ in 0..400 {
        if sock.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("daemon socket never appeared");
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
async fn escalation_then_human_approval_unblocks_the_hook() {
    let (daemon, sock, dir) = temp_daemon("escalate");
    daemon.reg.insert(guarded_session(1));
    let d = daemon.clone();
    tokio::spawn(async move {
        let _ = server::serve(d).await;
    });
    await_socket(&sock).await;

    // UI subscribes and reads the initial snapshot.
    let mut ui = UnixStream::connect(&sock).await.unwrap();
    framed::write_frame(&mut ui, &Frame::Request(Request::Subscribe))
        .await
        .unwrap();
    let snap: Event = framed::read_frame(&mut ui).await.unwrap();
    assert!(matches!(snap, Event::Snapshot { .. }));

    // A hook fires PreToolUse for an irreversible command → must escalate (never-auto).
    let mut hook = UnixStream::connect(&sock).await.unwrap();
    framed::write_frame(
        &mut hook,
        &Frame::Hook(HookEnvelope {
            session: SessionId(1),
            kind: HookKind::PreToolUse,
            payload_json: pretooluse("rm -rf /tmp/whatever"),
        }),
    )
    .await
    .unwrap();

    // UI observes a DecisionPending for session 1.
    let mut saw_pending = false;
    for _ in 0..30 {
        let ev: Event = timeout(Duration::from_secs(2), framed::read_frame(&mut ui))
            .await
            .expect("event timeout")
            .expect("event");
        if let Event::DecisionPending { session, .. } = ev {
            assert_eq!(session, SessionId(1));
            saw_pending = true;
            break;
        }
    }
    assert!(saw_pending, "UI never received DecisionPending");

    // The hook must still be parked (not yet answered).
    assert!(
        timeout(Duration::from_millis(100), framed::read_frame::<_, HookReply>(&mut hook))
            .await
            .is_err(),
        "hook returned before the human decided"
    );

    // UI approves → parked hook unblocks with Allow.
    framed::write_frame(
        &mut ui,
        &Frame::Request(Request::Decide {
            session: SessionId(1),
            approve: true,
            instruction: None,
        }),
    )
    .await
    .unwrap();

    let reply: HookReply = timeout(Duration::from_secs(2), framed::read_frame(&mut hook))
        .await
        .expect("reply timeout")
        .expect("reply");
    assert_eq!(reply.decision, Some(HookDecision::Allow));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn safe_command_auto_allows_without_a_human() {
    let (daemon, sock, dir) = temp_daemon("safe");
    daemon.reg.insert(guarded_session(7));
    let d = daemon.clone();
    tokio::spawn(async move {
        let _ = server::serve(d).await;
    });
    await_socket(&sock).await;

    // No UI needed: a safe command under Guarded should be auto-allowed immediately.
    let mut hook = UnixStream::connect(&sock).await.unwrap();
    framed::write_frame(
        &mut hook,
        &Frame::Hook(HookEnvelope {
            session: SessionId(7),
            kind: HookKind::PreToolUse,
            payload_json: pretooluse("ls -la"),
        }),
    )
    .await
    .unwrap();

    let reply: HookReply = timeout(Duration::from_secs(2), framed::read_frame(&mut hook))
        .await
        .expect("reply timeout")
        .expect("reply");
    assert_eq!(reply.decision, Some(HookDecision::Allow));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn denial_carries_the_instruction_back_to_the_agent() {
    let (daemon, sock, dir) = temp_daemon("deny");
    daemon.reg.insert(guarded_session(3));
    let d = daemon.clone();
    tokio::spawn(async move {
        let _ = server::serve(d).await;
    });
    await_socket(&sock).await;

    let mut ui = UnixStream::connect(&sock).await.unwrap();
    framed::write_frame(&mut ui, &Frame::Request(Request::Subscribe))
        .await
        .unwrap();
    let _snap: Event = framed::read_frame(&mut ui).await.unwrap();

    let mut hook = UnixStream::connect(&sock).await.unwrap();
    framed::write_frame(
        &mut hook,
        &Frame::Hook(HookEnvelope {
            session: SessionId(3),
            kind: HookKind::PreToolUse,
            payload_json: pretooluse("git push --force origin main"),
        }),
    )
    .await
    .unwrap();

    // drain until pending
    for _ in 0..30 {
        let ev: Event = timeout(Duration::from_secs(2), framed::read_frame(&mut ui))
            .await
            .unwrap()
            .unwrap();
        if matches!(ev, Event::DecisionPending { .. }) {
            break;
        }
    }

    framed::write_frame(
        &mut ui,
        &Frame::Request(Request::Decide {
            session: SessionId(3),
            approve: false,
            instruction: Some("use a normal push to a feature branch instead".into()),
        }),
    )
    .await
    .unwrap();

    let reply: HookReply = timeout(Duration::from_secs(2), framed::read_frame(&mut hook))
        .await
        .unwrap()
        .unwrap();
    match reply.decision {
        Some(HookDecision::Deny { reason }) => {
            assert!(reason.contains("feature branch"), "instruction lost: {reason}")
        }
        other => panic!("expected Deny with instruction, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}
