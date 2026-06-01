//! Dev helper: connect to a running `fleetd` and spawn a few demo sessions so the UI has
//! something to render. Usage: start `fleetd`, then run this example.

use std::os::unix::net::UnixStream;
use std::time::Duration;

use protocol::{codec, Autonomy, Frame, Request, SpawnSpec, Tool};

fn spec(tool: Tool, name: &str, opening: &str, autonomy: Autonomy) -> SpawnSpec {
    SpawnSpec {
        name: Some(name.into()),
        tool,
        model: None,
        cwd: std::env::var("HOME").ok(),
        worktree_from: None,
        autonomy,
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

    let reqs = vec![
        Request::Spawn(spec(Tool::Shell, "api-refactor", "echo 'refactoring the router…'; ls", Autonomy::Manual)),
        Request::Spawn(spec(Tool::Shell, "db-migration", "echo 'running migration 0007_schema'; sleep 60", Autonomy::Guarded)),
        Request::Spawn(spec(Tool::Shell, "docs-pass", "echo 'writing README.md (+42)'; sleep 60", Autonomy::Auto)),
        Request::Spawn(spec(Tool::Shell, "changelog", "echo 'done — ready for review'", Autonomy::Guarded)),
    ];
    for r in reqs {
        codec::write_frame(&mut s, &Frame::Request(r)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(400));
    println!("seeded {} sessions on {sock}", 4);
}
