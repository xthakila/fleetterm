//! A PTY-backed terminal session: spawns a child on a pseudo-terminal, feeds its output
//! through the alacritty VT engine into a server-side [`Term`] grid (so reattach is
//! correct, not byte-replay), and exposes input/resize/snapshot.
//!
//! API verified against alacritty_terminal 0.26.0 / portable-pty 0.9.0 / vte 0.15.0.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use protocol::{BlockMarker, CellSnap};

use crate::osc::Scanner as OscScanner;

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
    /// OS pid of the child process, captured at spawn for SIGSTOP/SIGCONT.
    child_pid: Option<u32>,
}

impl PtySession {
    /// Spawn `cmd` on a fresh PTY of the given size.
    ///
    /// * `on_output` — called from the reader thread with each raw PTY chunk; the
    ///   VT grid is updated before this fires.  Used to stream
    ///   [`protocol::Event::Output`] to subscribed UIs.
    /// * `on_block` — called for every [`BlockMarker`] found in the raw output by
    ///   the OSC 133 scanner.  May be called zero or more times per chunk, before
    ///   `on_output` returns.  Pass a no-op closure (`|_| {}`) to disable.
    pub fn spawn(
        cmd: CommandBuilder,
        cols: u16,
        rows: u16,
        on_output: impl Fn(Vec<u8>) + Send + 'static,
        on_block: impl Fn(BlockMarker) + Send + 'static,
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

        // Capture the OS pid now, before locking child behind a Mutex.
        let child_pid = child.process_id();

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
            child_pid,
        });

        // Reader thread: PTY bytes -> OSC 133 scanner + VT parser -> grid, then notify.
        std::thread::spawn(move || {
            let mut parser: Processor = Processor::new();
            let mut osc = OscScanner::new();
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        // Run the OSC 133 scanner first — it's a pure byte scan, no lock needed.
                        for marker in osc.scan(chunk) {
                            on_block(marker);
                        }
                        {
                            let mut t = term.lock().unwrap();
                            parser.advance(&mut *t, chunk);
                        }
                        on_output(chunk.to_vec());
                    }
                }
            }
        });

        Ok(session)
    }

    /// Write bytes (keystrokes / a prompt) to the child's stdin. Typing snaps the
    /// viewport back to the live screen so you're never stuck scrolled up in history.
    pub fn write_input(&self, data: &[u8]) -> std::io::Result<()> {
        self.term.lock().unwrap().scroll_display(Scroll::Bottom);
        let mut w = self.writer.lock().unwrap();
        w.write_all(data)?;
        w.flush()
    }

    /// Scroll the viewport through scrollback. Positive = up (history), negative = down.
    pub fn scroll(&self, lines: i32) {
        self.term.lock().unwrap().scroll_display(Scroll::Delta(lines));
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

    /// True once the child process has exited (used to mark sessions Done + fire pipelines).
    pub fn has_exited(&self) -> bool {
        self.child
            .lock()
            .unwrap()
            .try_wait()
            .map(|s| s.is_some())
            .unwrap_or(false)
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

    /// Render the visible screen as styled cells for the UI.
    ///
    /// Returns `(cols, rows, cursor_col, cursor_row, cells)` where `cells` is row-major
    /// (row 0 first, then row 1, …) and has `cols * rows` entries.
    pub fn grid_snapshot(&self) -> (u16, u16, u16, u16, Vec<CellSnap>) {
        let t = self.term.lock().unwrap();
        let grid = t.grid();
        let rows = grid.screen_lines();
        let cols = grid.columns();
        let cursor = grid.cursor.point;
        // Line(i32) — visible rows are 0..rows as i32.
        let cursor_row = cursor.line.0.max(0) as u16;
        let cursor_col = cursor.column.0 as u16;

        let mut cells = Vec::with_capacity(rows * cols);
        for r in 0..rows {
            for c in 0..cols {
                let cell = &grid[Line(r as i32)][Column(c)];
                let bold = cell.flags.contains(Flags::BOLD);
                let inverse = cell.flags.contains(Flags::INVERSE);
                cells.push(CellSnap {
                    ch: cell.c,
                    fg: color_to_rgb24(cell.fg),
                    bg: color_to_rgb24(cell.bg),
                    bold,
                    inverse,
                });
            }
        }
        (cols as u16, rows as u16, cursor_col, cursor_row, cells)
    }

    /// Send SIGSTOP to the child process group (pause output without killing).
    pub fn pause(&self) {
        if let Some(pid) = self.child_pid {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGSTOP);
        }
    }

    /// Send SIGCONT to resume a paused child.
    pub fn resume(&self) {
        if let Some(pid) = self.child_pid {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGCONT);
        }
    }
}

// ---------------------------------------------------------------------------
// Colour helpers
// ---------------------------------------------------------------------------

/// Sentinel value for "use the terminal theme default colour" (transparent to the UI).
/// Chosen as a value outside the 0x00RRGGBB range (bit 24 set).
const COLOR_DEFAULT: u32 = 0xFF00_0000;

/// Map an alacritty [`Color`] to a packed 24-bit RGB value (0x00RRGGBB).
///
/// * `Color::Spec(rgb)` — directly packed.
/// * `Color::Named(n)` — mapped to the standard VGA/xterm 16-color palette; the special
///   `Foreground` / `Background` named colours use the sentinel `COLOR_DEFAULT` so the
///   UI can apply its own theme.
/// * `Color::Indexed(i)` — the xterm-256 palette: 0-15 follow VGA, 16-231 are the 6×6×6
///   colour cube, 232-255 are 24 evenly-spaced greys.
fn color_to_rgb24(color: Color) -> u32 {
    match color {
        Color::Spec(rgb) => pack_rgb(rgb.r, rgb.g, rgb.b),
        Color::Named(named) => named_to_rgb24(named),
        Color::Indexed(i) => indexed_to_rgb24(i),
    }
}

fn pack_rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Standard VGA / xterm 16-color palette.
const VGA16: [(u8, u8, u8); 16] = [
    (0,   0,   0  ), // 0  Black
    (170, 0,   0  ), // 1  Red
    (0,   170, 0  ), // 2  Green
    (170, 85,  0  ), // 3  Yellow (dark)
    (0,   0,   170), // 4  Blue
    (170, 0,   170), // 5  Magenta
    (0,   170, 170), // 6  Cyan
    (170, 170, 170), // 7  White (light grey)
    (85,  85,  85 ), // 8  BrightBlack (dark grey)
    (255, 85,  85 ), // 9  BrightRed
    (85,  255, 85 ), // 10 BrightGreen
    (255, 255, 85 ), // 11 BrightYellow
    (85,  85,  255), // 12 BrightBlue
    (255, 85,  255), // 13 BrightMagenta
    (85,  255, 255), // 14 BrightCyan
    (255, 255, 255), // 15 BrightWhite
];

fn named_to_rgb24(named: NamedColor) -> u32 {
    match named {
        // Standard 8 colours.
        NamedColor::Black        => pack_rgb(VGA16[0].0,  VGA16[0].1,  VGA16[0].2),
        NamedColor::Red          => pack_rgb(VGA16[1].0,  VGA16[1].1,  VGA16[1].2),
        NamedColor::Green        => pack_rgb(VGA16[2].0,  VGA16[2].1,  VGA16[2].2),
        NamedColor::Yellow       => pack_rgb(VGA16[3].0,  VGA16[3].1,  VGA16[3].2),
        NamedColor::Blue         => pack_rgb(VGA16[4].0,  VGA16[4].1,  VGA16[4].2),
        NamedColor::Magenta      => pack_rgb(VGA16[5].0,  VGA16[5].1,  VGA16[5].2),
        NamedColor::Cyan         => pack_rgb(VGA16[6].0,  VGA16[6].1,  VGA16[6].2),
        NamedColor::White        => pack_rgb(VGA16[7].0,  VGA16[7].1,  VGA16[7].2),
        // Bright variants.
        NamedColor::BrightBlack   => pack_rgb(VGA16[8].0,  VGA16[8].1,  VGA16[8].2),
        NamedColor::BrightRed     => pack_rgb(VGA16[9].0,  VGA16[9].1,  VGA16[9].2),
        NamedColor::BrightGreen   => pack_rgb(VGA16[10].0, VGA16[10].1, VGA16[10].2),
        NamedColor::BrightYellow  => pack_rgb(VGA16[11].0, VGA16[11].1, VGA16[11].2),
        NamedColor::BrightBlue    => pack_rgb(VGA16[12].0, VGA16[12].1, VGA16[12].2),
        NamedColor::BrightMagenta => pack_rgb(VGA16[13].0, VGA16[13].1, VGA16[13].2),
        NamedColor::BrightCyan    => pack_rgb(VGA16[14].0, VGA16[14].1, VGA16[14].2),
        NamedColor::BrightWhite   => pack_rgb(VGA16[15].0, VGA16[15].1, VGA16[15].2),
        // Dim variants — use the non-dim colour at half brightness.
        NamedColor::DimBlack    => pack_rgb(0,   0,   0  ),
        NamedColor::DimRed      => pack_rgb(85,  0,   0  ),
        NamedColor::DimGreen    => pack_rgb(0,   85,  0  ),
        NamedColor::DimYellow   => pack_rgb(85,  42,  0  ),
        NamedColor::DimBlue     => pack_rgb(0,   0,   85 ),
        NamedColor::DimMagenta  => pack_rgb(85,  0,   85 ),
        NamedColor::DimCyan     => pack_rgb(0,   85,  85 ),
        NamedColor::DimWhite    => pack_rgb(85,  85,  85 ),
        // Theme-managed — let the UI supply its own colour.
        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground
        | NamedColor::Background | NamedColor::Cursor => COLOR_DEFAULT,
    }
}

/// xterm-256 indexed palette → 0x00RRGGBB.
fn indexed_to_rgb24(idx: u8) -> u32 {
    if idx < 16 {
        // First 16 entries follow the VGA palette.
        let (r, g, b) = VGA16[idx as usize];
        pack_rgb(r, g, b)
    } else if idx < 232 {
        // 6×6×6 colour cube: indices 16-231.
        let i = idx - 16;
        let b_idx = i % 6;
        let g_idx = (i / 6) % 6;
        let r_idx = i / 36;
        let cube = |v: u8| if v == 0 { 0u8 } else { 55 + v * 40 };
        pack_rgb(cube(r_idx), cube(g_idx), cube(b_idx))
    } else {
        // 24 greyscale steps: indices 232-255.
        let level = 8 + (idx - 232) as u16 * 10;
        let g = level.min(255) as u8;
        pack_rgb(g, g, g)
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
        let sess = PtySession::spawn(cmd, 80, 24, |_| {}, |_| {}).expect("spawn");

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
        let sess = PtySession::spawn(cmd_with_no_args(&mut cmd), 80, 24, |_| {}, |_| {}).expect("spawn");
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

    #[test]
    fn grid_snapshot_returns_correct_dims_and_chars() {
        // Spawn a process that writes a known character then sleeps.
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", "printf 'X'; sleep 5"]);
        let sess = PtySession::spawn(cmd, 40, 10, |_| {}, |_| {}).expect("spawn");

        // Wait for 'X' to land in the grid.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if sess.screen_text().contains('X') {
                break;
            }
            assert!(Instant::now() < deadline, "marker never appeared");
            std::thread::sleep(Duration::from_millis(25));
        }

        let (cols, rows, _cx, _cy, cells) = sess.grid_snapshot();
        assert_eq!(cols, 40, "cols");
        assert_eq!(rows, 10, "rows");
        assert_eq!(cells.len(), 40 * 10, "cell count");
        // The 'X' must appear somewhere in the cells.
        assert!(cells.iter().any(|c| c.ch == 'X'), "X not found in grid cells");
        // fg and bg must be valid: either a packed colour or the sentinel.
        for cell in &cells {
            let valid = cell.fg <= 0x00_FF_FF_FF || cell.fg == COLOR_DEFAULT;
            assert!(valid, "fg out of range: 0x{:08X}", cell.fg);
        }
        sess.kill();
    }

    #[test]
    fn indexed_colour_palette_spot_checks() {
        // index 0 = black
        assert_eq!(indexed_to_rgb24(0), 0x000000);
        // index 15 = bright white
        assert_eq!(indexed_to_rgb24(15), 0xFFFFFF);
        // index 16 = first cube entry: r=0,g=0,b=0 => black again
        assert_eq!(indexed_to_rgb24(16), 0x000000);
        // index 17 = r=0,g=0,b=1 => rgb(0,0,95)
        assert_eq!(indexed_to_rgb24(17), pack_rgb(0, 0, 95));
        // index 231 = last cube entry: r=5,g=5,b=5 => all max => 255,255,255
        assert_eq!(indexed_to_rgb24(231), 0xFFFFFF);
        // index 232 = first grey: 8,8,8
        assert_eq!(indexed_to_rgb24(232), pack_rgb(8, 8, 8));
        // index 255 = last grey: 8 + 23*10 = 238
        assert_eq!(indexed_to_rgb24(255), pack_rgb(238, 238, 238));
    }
}
