//! Diagnostic for the "terminal formatting fucked up" bug. Answers, with data:
//!  1. After Subscribe (no RequestGrid), does any Event::Grid arrive on its own?
//!  2. Does Event::Output for a session contain raw ANSI escape bytes (\x1b)?
//!     (If the UI falls back to rendering term_text, those escapes show as garbage.)
//!  3. After an explicit RequestGrid, is the Grid data sane (cols/rows + readable chars)?

use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use protocol::{codec, Event, Frame, Request, SessionId};

fn main() {
    let sock = std::env::var("FLEETTERM_SOCK").unwrap_or_else(|_| {
        let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        format!("{base}/fleetterm/fleetd.sock")
    });
    let mut s = UnixStream::connect(&sock).expect("connect");
    s.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    codec::write_frame(&mut s, &Frame::Request(Request::Subscribe)).unwrap();

    let mut first: Option<SessionId> = None;
    let mut grids_unsolicited = 0u32;
    let mut output_chunks = 0u32;
    let mut output_with_esc = 0u32;

    // Phase 1: 2.5s, NO RequestGrid — observe what arrives unsolicited.
    let p1 = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < p1 {
        match codec::read_frame::<_, Event>(&mut s) {
            Ok(Event::Snapshot { sessions, .. }) => {
                first = sessions.first().map(|x| x.id.clone());
                println!("[snapshot] {} sessions; first={:?}", sessions.len(), first);
            }
            Ok(Event::Grid { session, .. }) => {
                grids_unsolicited += 1;
                println!("[grid-UNSOLICITED] for {session}");
            }
            Ok(Event::Output { data, .. }) => {
                output_chunks += 1;
                if data.contains(&0x1b) {
                    output_with_esc += 1;
                }
            }
            _ => {}
        }
    }
    println!(
        "\nPHASE 1 (no RequestGrid): unsolicited grids={grids_unsolicited}, output_chunks={output_chunks}, output_with_ANSI_escapes={output_with_esc}"
    );
    println!("  → if grids=0 and output_with_ANSI>0, the focused pane falls back to raw-ANSI text = garbled.\n");

    // Phase 2: explicitly request a grid and inspect it.
    if let Some(id) = first {
        codec::write_frame(&mut s, &Frame::Request(Request::RequestGrid(id.clone()))).unwrap();
        let p2 = Instant::now() + Duration::from_millis(2500);
        while Instant::now() < p2 {
            if let Ok(Event::Grid { session, cols, rows, cursor_col, cursor_row, cells }) =
                codec::read_frame::<_, Event>(&mut s)
            {
                println!("[grid for {session}] cols={cols} rows={rows} cursor=({cursor_col},{cursor_row}) cells={}", cells.len());
                println!("  cells.len()==cols*rows? {} ({} vs {})", cells.len() == cols as usize * rows as usize, cells.len(), cols as usize * rows as usize);
                // Reconstruct first few non-empty rows from cells (row-major).
                for r in 0..rows.min(8) as usize {
                    let mut line = String::new();
                    for c in 0..cols as usize {
                        let idx = r * cols as usize + c;
                        let ch = cells.get(idx).map(|cc| cc.ch).unwrap_or(' ');
                        line.push(if ch == '\0' { ' ' } else { ch });
                    }
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() {
                        println!("  row {r:>2}: |{trimmed}|");
                    }
                }
                break;
            }
        }
    }
    println!("\n[griddump] done");
}
