//! Demo of P6 inter-agent pipelines: spawn A, then chain B to start only after A
//! finishes. Start `fleetd`, then run this and watch B appear once A exits.

use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use protocol::{codec, Autonomy, Event, Frame, Request, SpawnSpec, Tool};

fn shell(name: &str, opening: &str) -> SpawnSpec {
    SpawnSpec {
        name: Some(name.into()),
        tool: Tool::Shell,
        model: None,
        cwd: std::env::var("HOME").ok(),
        worktree_from: None,
        autonomy: Autonomy::Auto,
        opening: Some(opening.into()),
        env: vec![],
    }
}

fn main() {
    let sock = std::env::var("FLEETTERM_SOCK").unwrap_or_else(|_| {
        let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        format!("{base}/fleetterm/fleetd.sock")
    });
    let mut s = UnixStream::connect(&sock).expect("connect to fleetd");
    s.set_read_timeout(Some(Duration::from_millis(400))).unwrap();

    codec::write_frame(&mut s, &Frame::Request(Request::Subscribe)).unwrap();
    // A: do some work, then exit so it reaches Done.
    codec::write_frame(&mut s, &Frame::Request(Request::Spawn(shell(
        "pipeline-A",
        "echo 'A: doing work'; sleep 1; echo 'A: done'; exit",
    ))))
    .unwrap();
    println!("[pipeline] spawned A; chaining B to start after A finishes…\n");

    let mut chained = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match codec::read_frame::<_, Event>(&mut s) {
            Ok(Event::SessionUpdate(sess)) => {
                println!("[state] {:<12} {:?}", sess.name, sess.state);
                // Once we learn A's id, queue B after it.
                if !chained && sess.name == "pipeline-A" {
                    chained = true;
                    codec::write_frame(
                        &mut s,
                        &Frame::Request(Request::SpawnAfter {
                            after: sess.id.clone(),
                            spec: shell("pipeline-B", "echo 'B: started only after A finished'"),
                        }),
                    )
                    .unwrap();
                    println!("[pipeline] B queued after A ({})\n", sess.id);
                }
            }
            Ok(_) => {}
            Err(codec::CodecError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    println!("\n[pipeline] done (expect: A → Done, then pipeline-B appears).");
}
