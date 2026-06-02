//! Live end-to-end demo of the Claude hook → autonomy loop, headless (no GUI needed).
//!
//! Start `fleetd`, then run this. It subscribes, spawns a REAL `claude` session with an
//! opening prompt that makes Claude use its Bash tool, and prints the daemon events as
//! the hooks fire — so you can watch state transitions + autonomy AutoDecisions (a safe
//! `ls` is auto-allowed under Guarded) come through wire-to-wire from an actual agent.
//!
//! Usage: target/debug/fleetd &   then   target/debug/examples/claudedemo

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use protocol::{codec, Autonomy, Event, Frame, Request, SpawnSpec, Tool};

fn sock() -> String {
    std::env::var("FLEETTERM_SOCK").unwrap_or_else(|_| {
        let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        format!("{base}/fleetterm/fleetd.sock")
    })
}

fn main() {
    let path = sock();
    let mut s = UnixStream::connect(&path).expect("connect to fleetd");
    s.set_read_timeout(Some(Duration::from_millis(500))).unwrap();

    // Subscribe, then spawn a real Claude session that will exercise a Bash tool.
    codec::write_frame(&mut s, &Frame::Request(Request::Subscribe)).unwrap();
    let spec = SpawnSpec {
        name: Some("claude-demo".into()),
        tool: Tool::Claude,
        model: None,
        cwd: std::env::var("HOME").ok(),
        worktree_from: None,
        autonomy: Autonomy::Guarded,
        opening: Some(
            "Use your Bash tool to run exactly `ls -la` once, show the output, then stop. \
             Do not run anything else."
                .into(),
        ),
        env: vec![],
    };
    codec::write_frame(&mut s, &Frame::Request(Request::Spawn(spec))).unwrap();
    println!("[demo] subscribed + spawned a real `claude` session (Guarded). Watching events for 90s…\n");

    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        match codec::read_frame::<_, Event>(&mut s) {
            Ok(ev) => print_event(&ev),
            Err(codec::CodecError::Io(e))
                if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
            {
                continue
            }
            Err(codec::CodecError::Closed) => {
                println!("[demo] daemon closed the connection");
                break;
            }
            Err(e) => {
                eprintln!("[demo] read error: {e}");
                break;
            }
        }
    }
    println!("\n[demo] done.");
}

fn print_event(ev: &Event) {
    match ev {
        Event::Snapshot { sessions, .. } => {
            println!("[snapshot] {} session(s)", sessions.len());
        }
        Event::SessionUpdate(s) => {
            println!("[state] {:<14} {:?}  — {}", s.name, s.state, s.activity);
        }
        Event::DecisionPending { session, kind } => {
            println!("[NEEDS YOU] session {session}: {kind:?}");
        }
        Event::AutoDecision { session, kind, approved, reason } => {
            println!(
                "[autonomy] session {session}: {} {kind:?}  ({reason})",
                if *approved { "AUTO-ALLOWED" } else { "AUTO-DENIED" }
            );
        }
        Event::SessionRemoved(id) => println!("[gone] session {id}"),
        Event::Error { message } => println!("[error] {message}"),
        // Output/Grid/Block are noisy; skip in this demo.
        _ => {}
    }
}
