//! A PTY-backed terminal session: spawns a child on a pseudo-terminal, feeds its output
//! through the alacritty VT engine into a server-side [`Term`] grid (so reattach is
//! correct, not byte-replay), and exposes input/resize/snapshot.
//!
//! API verified against alacritty_terminal 0.26.0 / portable-pty 0.9.0 / vte 0.15.0.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

/// Our own [`Dimensions`] (alacritty's `TermSize` is test-only). Visible screen only;
/// scrollback history is governed by [`Config::scrolling_history`].
#[derive(Clone, Copy)]
struct GridDims {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for GridDims {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

/// A live terminal session. The VT grid is shared (`Arc<Mutex<Term>>`) so the daemon can
/// read it for state heuristics while the reader thread keeps applying output.
pub struct PtySession {
    pub term: Arc<Mutex<Term<VoidListener>>>,
    // Mutex-wrapped so PtySession is Sync (dyn MasterPty is Send but not Sync), which is
    // required for Arc<Daemon> to cross tokio::spawn boundaries.
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    cols: u16,
    rows: u16,
}

impl PtySession {
    /// Spawn `cmd` on a fresh PTY of the given size. `on_output` is called from the
    /// reader thread with each raw chunk (used to stream [`protocol::Event::Output`] to
    /// subscribed UIs); the grid is updated before the callback fires.
    pub fn spawn(
        cmd: CommandBuilder,
        cols: u16,
        rows: u16,
        on_output: impl Fn(Vec<u8>) + Send + 'static,
    ) -> anyhow::Result<Arc<PtySession>> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let child = pair.slave.spawn_command(cmd)?;
        // Drop the slave so the only handle to the PTY is the master; otherwise EOF never
        // arrives when the child exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let term = Arc::new(Mutex::new(Term::new(
            Config::default(),
            &GridDims {
                columns: cols as usize,
                screen_lines: rows as usize,
            },
            VoidListener,
        )));

        let session = Arc::new(PtySession {
            term: term.clone(),
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            cols,
            rows,
        });

        // Reader thread: PTY bytes -> VT parser -> grid, then notify.
        std::thread::spawn(move || {
            let mut parser: Processor = Processor::new();
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        {
                            let mut t = term.lock().unwrap();
                            parser.advance(&mut *t, &buf[..n]);
                        }
                        on_output(buf[..n].to_vec());
                    }
                }
            }
        });

        Ok(session)
    }

    /// Write bytes (keystrokes / a prompt) to the child's stdin.
    pub fn write_input(&self, data: &[u8]) -> std::io::Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.write_all(data)?;
        w.flush()
    }

    /// Resize both the PTY and the VT grid (must stay in lock-step).
    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.master.lock().unwrap().resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.term.lock().unwrap().resize(GridDims {
            columns: cols as usize,
            screen_lines: rows as usize,
        });
        Ok(())
    }

    /// Best-effort terminate the child.
    pub fn kill(&self) {
        let _ = self.child.lock().unwrap().kill();
    }

    pub fn dims(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Render the visible screen as plain text (trailing blanks trimmed per row).
    /// Used by tests and the state heuristics; the UI renders styled cells separately.
    pub fn screen_text(&self) -> String {
        let t = self.term.lock().unwrap();
        let grid = t.grid();
        let rows = grid.screen_lines();
        let cols = grid.columns();
        let mut out = String::with_capacity(rows * (cols + 1));
        for r in 0..rows {
            let mut line = String::with_capacity(cols);
            for c in 0..cols {
                line.push(grid[Line(r as i32)][Column(c)].c);
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    /// The last non-empty visible line — a cheap "what's it showing now" for activity.
    pub fn last_nonempty_line(&self) -> String {
        self.screen_text()
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn shell_output_lands_in_the_grid() {
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", "printf 'fleetterm-grid-ok\\n'; sleep 1"]);
        let sess = PtySession::spawn(cmd, 80, 24, |_| {}).expect("spawn");

        // Poll the grid until the marker shows up (or time out).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if sess.screen_text().contains("fleetterm-grid-ok") {
                break;
            }
            assert!(Instant::now() < deadline, "marker never appeared in grid");
            std::thread::sleep(Duration::from_millis(25));
        }
        sess.kill();
    }

    #[test]
    fn input_is_echoed_through_the_pty() {
        let mut cmd = CommandBuilder::new("cat");
        let sess = PtySession::spawn(cmd_with_no_args(&mut cmd), 80, 24, |_| {}).expect("spawn");
        sess.write_input(b"roundtrip\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if sess.screen_text().contains("roundtrip") {
                break;
            }
            assert!(Instant::now() < deadline, "echoed input never appeared");
            std::thread::sleep(Duration::from_millis(25));
        }
        sess.kill();
    }

    fn cmd_with_no_args(c: &mut CommandBuilder) -> CommandBuilder {
        c.clone()
    }
}
