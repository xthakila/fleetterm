//! Passive watcher: subscribe to fleetd and print session state changes for ~25s.
//! Used to verify the heuristic no longer flaps idle sessions to "Stuck".

use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use protocol::{codec, Event, Frame, Request};

fn main() {
    let sock = std::env::var("FLEETTERM_SOCK").unwrap_or_else(|_| {
        let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        format!("{base}/fleetterm/fleetd.sock")
    });
    let mut s = UnixStream::connect(&sock).expect("connect");
    s.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    codec::write_frame(&mut s, &Frame::Request(Request::Subscribe)).unwrap();

    let deadline = Instant::now() + Duration::from_secs(25);
    let mut updates = 0u32;
    while Instant::now() < deadline {
        match codec::read_frame::<_, Event>(&mut s) {
            Ok(Event::SessionUpdate(sess)) => {
                updates += 1;
                println!("[update] {:<14} {:?}", sess.name, sess.state);
            }
            Ok(_) => {}
            Err(codec::CodecError::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    println!("[watch] {updates} SessionUpdate events in 25s (low + settled = good)");
}
