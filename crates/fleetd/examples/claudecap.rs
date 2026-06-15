//! Diagnostic: spawn a real `claude` session via the daemon and DUMP its terminal
//! output (decoded) so we can see what it actually renders on launch — onboarding /
//! trust dialog / prompt / idle — to debug why hooks don't fire end-to-end.

use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use protocol::{codec, Autonomy, Event, Frame, Request, SpawnSpec, Tool};

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let send_prompt = std::env::var("CAP_NO_PROMPT").is_err();
    let sock = std::env::var("FLEETTERM_SOCK").unwrap_or_else(|_| {
        let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        format!("{base}/fleetterm/fleetd.sock")
    });
    let mut s = UnixStream::connect(&sock).expect("connect");
    s.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    codec::write_frame(&mut s, &Frame::Request(Request::Subscribe)).unwrap();

    let spec = SpawnSpec {
        name: Some("claude-cap".into()),
        tool: Tool::Claude,
        model: None,
        cwd: std::env::var("HOME").ok(),
        worktree_from: None,
        autonomy: Autonomy::Guarded,
        opening: if send_prompt {
            Some("Run the bash command `ls` once with your Bash tool, then stop.".into())
        } else {
            None
        },
        env: vec![],
    };
    codec::write_frame(&mut s, &Frame::Request(Request::Spawn(spec))).unwrap();
    eprintln!("[cap] spawned claude (prompt={send_prompt}); dumping output for {secs}s\n");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let stdout = std::io::stdout();
    while Instant::now() < deadline {
        match codec::read_frame::<_, Event>(&mut s) {
            Ok(Event::Output { data, .. }) => {
                let _ = stdout.lock().write_all(&data);
                let _ = stdout.lock().flush();
            }
            Ok(Event::SessionUpdate(se)) => eprintln!("\n[state] {:?} — {}", se.state, se.activity),
            Ok(Event::DecisionPending { kind, .. }) => eprintln!("\n[NEEDS YOU] {kind:?}"),
            Ok(Event::AutoDecision { approved, reason, .. }) => {
                eprintln!("\n[autonomy] approved={approved} ({reason})")
            }
            Ok(_) => {}
            Err(codec::CodecError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    eprintln!("\n[cap] done");
}
