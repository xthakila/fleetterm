//! End-to-end proof of the agent-governance loop over the real unix socket:
//! a UI subscribes, a (simulated) hook fires PreToolUse, the daemon either auto-allows
//! (safe) or escalates → emits DecisionPending → the UI decides → the parked hook is
//! unblocked with the matching decision. This is the core of Phases 2–3.
//!
//! Also contains the live-grid push test: after `RequestGrid` marks a session as
//! watched, `Event::Grid` events must arrive on the broadcast channel without further
//! explicit requests.

use std::path::PathBuf;
use std::time::Duration;

use fleetd::daemon::Daemon;
use fleetd::{framed, server};
use protocol::{
    Autonomy, Event, Frame, HookDecision, HookEnvelope, HookKind, HookReply, Request, Session,
    SessionId, SpawnSpec, State, Target, Tool,
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

// ---------------------------------------------------------------------------
// Live-grid push test
// ---------------------------------------------------------------------------

/// After `RequestGrid` marks a session as *watched*, the daemon must autonomously
/// push `Event::Grid` events at ~80 ms cadence as the session produces output —
/// without the UI sending additional `RequestGrid` requests.
///
/// We spawn a real shell that continuously prints text, subscribe a UI, send one
/// `RequestGrid`, then wait to receive at least one more `Event::Grid` that arrived
/// without any further request from us.
#[tokio::test]
async fn watched_session_pushes_live_grid_updates() {
    let (daemon, sock, dir) = temp_daemon("livegrid");
    let d = daemon.clone();
    tokio::spawn(async move {
        let _ = server::serve(d).await;
    });
    await_socket(&sock).await;

    // Connect as a UI and subscribe to the event stream.
    let mut ui = UnixStream::connect(&sock).await.unwrap();
    framed::write_frame(&mut ui, &Frame::Request(Request::Subscribe))
        .await
        .unwrap();
    // Consume the initial snapshot.
    let snap: Event = framed::read_frame(&mut ui).await.unwrap();
    assert!(matches!(snap, Event::Snapshot { .. }), "expected Snapshot");

    // Spawn a shell session that emits output continuously.
    framed::write_frame(
        &mut ui,
        &Frame::Request(Request::Spawn(SpawnSpec {
            name: Some("live-grid-test".into()),
            tool: Tool::Shell,
            model: None,
            cwd: None,
            worktree_from: None,
            autonomy: Autonomy::Auto,
            // Print a line every 50 ms — fast enough to produce output within our window.
            opening: Some("while true; do printf 'tick\\n'; sleep 0.05; done".into()),
            env: vec![],
        })),
    )
    .await
    .unwrap();

    // Drain events until we learn the spawned session's id from SessionUpdate.
    let session_id = {
        let mut found = None;
        for _ in 0..40 {
            let ev: Event = timeout(Duration::from_secs(2), framed::read_frame(&mut ui))
                .await
                .expect("timeout waiting for SessionUpdate")
                .expect("frame error");
            if let Event::SessionUpdate(s) = ev {
                found = Some(s.id);
                break;
            }
        }
        found.expect("never received SessionUpdate for spawned session")
    };

    // Give the shell a moment to start printing.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send ONE RequestGrid — this marks the session as watched.
    framed::write_frame(
        &mut ui,
        &Frame::Request(Request::RequestGrid(session_id.clone())),
    )
    .await
    .unwrap();

    // Drain until we see the first Grid event (triggered by our RequestGrid).
    let mut got_first_grid = false;
    for _ in 0..60 {
        let ev: Event = timeout(Duration::from_secs(3), framed::read_frame(&mut ui))
            .await
            .expect("timeout waiting for first Grid")
            .expect("frame error");
        if matches!(&ev, Event::Grid { session, .. } if *session == session_id) {
            got_first_grid = true;
            break;
        }
    }
    assert!(got_first_grid, "never received the first Event::Grid from RequestGrid");

    // Now wait for a SECOND Event::Grid without sending another RequestGrid.
    // The live-grid watcher task must emit it within ~400 ms (a few 80 ms ticks).
    let mut got_live_grid = false;
    for _ in 0..80 {
        match timeout(Duration::from_millis(400), framed::read_frame::<_, Event>(&mut ui)).await {
            Ok(Ok(ev)) => {
                if matches!(&ev, Event::Grid { session, .. } if *session == session_id) {
                    got_live_grid = true;
                    break;
                }
                // Other events (Output, SessionUpdate) are fine — keep draining.
            }
            Ok(Err(_)) => break,
            Err(_) => break, // timeout — no more events in time
        }
    }
    assert!(
        got_live_grid,
        "live-grid watcher never pushed a second Event::Grid autonomously"
    );

    // Cleanup: stop the session.
    framed::write_frame(
        &mut ui,
        &Frame::Request(Request::Stop(Target::Session(session_id))),
    )
    .await
    .unwrap();

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// P6 inter-agent pipeline: spawn B once A finishes
// ---------------------------------------------------------------------------

fn shell_spec(name: &str, opening: &str) -> SpawnSpec {
    SpawnSpec {
        name: Some(name.into()),
        tool: Tool::Shell,
        model: None,
        cwd: None,
        worktree_from: None,
        autonomy: Autonomy::Auto,
        opening: Some(opening.into()),
        env: vec![],
    }
}

fn session_names(daemon: &Daemon) -> Vec<String> {
    match daemon.reg.snapshot_event() {
        Event::Snapshot { sessions, .. } => sessions.into_iter().map(|s| s.name).collect(),
        _ => vec![],
    }
}

#[tokio::test]
async fn pipeline_spawns_dependent_after_predecessor_finishes() {
    let (daemon, _sock, dir) = temp_daemon("pipeline");
    daemon.start_pipeline_watcher();

    // A: a shell told to exit immediately → its process ends → poller marks it Done.
    let a = daemon.spawn(shell_spec("pipe-A", "exit")).expect("spawn A");
    // Queue B to start only after A is Done.
    daemon.spawn_after(a.clone(), shell_spec("pipe-B", "echo from B"));

    // B must NOT exist yet.
    assert!(!session_names(&daemon).iter().any(|n| n == "pipe-B"));

    // Within a few seconds (poller detects A's exit, watcher fires B), B appears.
    let mut appeared = false;
    for _ in 0..100 {
        if session_names(&daemon).iter().any(|n| n == "pipe-B") {
            appeared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(appeared, "pipeline never spawned pipe-B after pipe-A finished");

    let _ = std::fs::remove_dir_all(&dir);
}
