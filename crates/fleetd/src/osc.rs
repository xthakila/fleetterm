//! OSC 133 shell-integration sequence scanner.
//!
//! Shells that source the FleetTerm shell-integration snippet (see [`crate::shellinit`])
//! emit OSC 133 sequences at prompt/command boundaries. This module provides a stateful
//! [`Scanner`] that can be driven with raw PTY byte chunks and returns the
//! [`BlockMarker`]s found in each chunk.
//!
//! # OSC 133 sequence grammar
//!
//! ```text
//! ESC ] 1 3 3 ; <param> <ST>
//! ```
//!
//! Where:
//! * `ESC ]` is `0x1B 0x5D` — the OSC introducer.
//! * `<param>` is one of `A`, `B`, `C`, or `D[;exitcode]`.
//! * `<ST>` is either `BEL` (`0x07`) or `ESC \` (`0x1B 0x5C`).
//!
//! The scanner handles chunk boundaries transparently: if a sequence straddles a `read()`
//! boundary the partial escape is buffered and completed when the next chunk arrives.

use protocol::BlockMarker;

/// Maximum bytes buffered for a partial OSC 133 param. Any sequence whose param grows
/// beyond this is discarded — real OSC 133 params are at most a few bytes (`D;127`).
const MAX_PARAM: usize = 32;

/// Maximum bytes buffered while probing the OSC preamble for `"133;"`. Real preambles
/// are exactly 4 bytes, so any longer sequence is not an OSC 133.
const MAX_PREAMBLE: usize = 8;

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Normal: looking for ESC.
    Ground,
    /// Saw ESC — next byte determines what kind of escape sequence follows.
    Esc,
    /// Saw `ESC ]` — collecting OSC preamble bytes until we confirm/reject `"133;"`.
    OscPreamble,
    /// Confirmed `"133;"` — collecting the param bytes until ST or BEL.
    Param,
    /// Saw ESC while in Param — checking for the two-byte ST `ESC \`.
    ParamEsc,
    /// Inside an OSC whose preamble does NOT start with `"133;"` — skip to ST.
    OscSkip,
    /// Saw ESC while skipping a non-133 OSC — checking for `ESC \` to resume Ground.
    OscSkipEsc,
}

// ---------------------------------------------------------------------------
// Public scanner
// ---------------------------------------------------------------------------

/// Stateful OSC 133 byte-stream scanner.
///
/// Create one instance per PTY session and feed every raw output chunk through
/// [`Scanner::scan`].  Sequences split across `read()` calls are reassembled
/// automatically via the internal buffer.
///
/// ```
/// use fleetd::osc::Scanner;
/// use protocol::BlockMarker;
///
/// let mut s = Scanner::new();
/// // ESC ] 1 3 3 ; A BEL
/// let bytes = b"\x1b]133;A\x07";
/// assert_eq!(s.scan(bytes), vec![BlockMarker::PromptStart]);
/// ```
#[derive(Debug)]
pub struct Scanner {
    state: State,
    /// Bytes collected during OscPreamble, waiting to match `"133;"`.
    preamble: Vec<u8>,
    /// Param bytes collected after the `"133;"` prefix.
    param: Vec<u8>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    /// Create a fresh scanner in the Ground state.
    pub fn new() -> Self {
        Scanner {
            state: State::Ground,
            preamble: Vec::new(),
            param: Vec::new(),
        }
    }

    /// Feed a raw PTY chunk and return any [`BlockMarker`]s detected in order.
    ///
    /// The scanner retains its state between calls so sequences that span two
    /// chunks are reassembled correctly.
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<BlockMarker> {
        let mut out = Vec::new();
        for &b in bytes {
            self.feed(b, &mut out);
        }
        out
    }

    fn feed(&mut self, b: u8, out: &mut Vec<BlockMarker>) {
        match self.state {
            // ----------------------------------------------------------------
            State::Ground => {
                if b == 0x1B {
                    self.state = State::Esc;
                }
                // All other bytes: not relevant, stay in Ground.
            }

            // ----------------------------------------------------------------
            State::Esc => {
                match b {
                    // ESC ] → start of an OSC sequence.
                    0x5D => {
                        self.preamble.clear();
                        self.param.clear();
                        self.state = State::OscPreamble;
                    }
                    // Another ESC → the previous one was bare, stay in Esc.
                    0x1B => {}
                    // ESC \ while in Ground+Esc is a lone String Terminator — not a sequence start.
                    0x5C => {
                        self.state = State::Ground;
                    }
                    // Any other byte after ESC (CSI etc.) — not an OSC.
                    _ => {
                        self.state = State::Ground;
                    }
                }
            }

            // ----------------------------------------------------------------
            // Collecting the preamble bytes until we can confirm/reject "133;".
            State::OscPreamble => {
                match b {
                    // BEL terminates the OSC before we confirmed "133;".
                    0x07 => {
                        self.preamble.clear();
                        self.state = State::Ground;
                    }
                    // ESC inside an OSC might be part of ST (ESC \).
                    0x1B => {
                        self.preamble.clear();
                        self.state = State::OscSkipEsc;
                    }
                    _ => {
                        self.preamble.push(b);
                        let len = self.preamble.len();
                        if len == 4 {
                            if self.preamble.as_slice() == b"133;" {
                                // Confirmed — switch to param collection.
                                self.preamble.clear();
                                self.param.clear();
                                self.state = State::Param;
                            } else {
                                // Different OSC number — skip the rest.
                                self.preamble.clear();
                                self.state = State::OscSkip;
                            }
                        } else if len > MAX_PREAMBLE {
                            // Runaway preamble — bail.
                            self.preamble.clear();
                            self.state = State::Ground;
                        }
                    }
                }
            }

            // ----------------------------------------------------------------
            // Collecting param bytes after "133;".
            State::Param => {
                match b {
                    // BEL — sequence complete.
                    0x07 => {
                        self.emit_marker(out);
                        self.state = State::Ground;
                    }
                    // ESC — possible ST (ESC \).
                    0x1B => {
                        self.state = State::ParamEsc;
                    }
                    _ => {
                        self.param.push(b);
                        if self.param.len() > MAX_PARAM {
                            // Param too long to be a valid OSC 133 — discard.
                            self.param.clear();
                            self.state = State::Ground;
                        }
                    }
                }
            }

            // ----------------------------------------------------------------
            // Saw ESC inside the param — checking for ST (ESC \).
            State::ParamEsc => {
                match b {
                    // ESC \ — String Terminator — sequence complete.
                    0x5C => {
                        self.emit_marker(out);
                        self.state = State::Ground;
                    }
                    // BEL after ESC (unusual but treat as terminator).
                    0x07 => {
                        self.emit_marker(out);
                        self.state = State::Ground;
                    }
                    // Another ESC — the previous one was not an ST start; stay in ParamEsc.
                    0x1B => {}
                    // Any other byte: the ESC was not a valid ST — discard the partial sequence.
                    _ => {
                        self.param.clear();
                        self.state = State::Ground;
                    }
                }
            }

            // ----------------------------------------------------------------
            // Skipping a non-133 OSC until its ST or BEL.
            State::OscSkip => {
                match b {
                    0x07 => {
                        self.state = State::Ground;
                    }
                    0x1B => {
                        self.state = State::OscSkipEsc;
                    }
                    _ => { /* keep skipping */ }
                }
            }

            // ----------------------------------------------------------------
            // Saw ESC while skipping — checking for ST (ESC \) to exit the OSC.
            State::OscSkipEsc => {
                match b {
                    0x5C => {
                        self.state = State::Ground;
                    }
                    0x1B => { /* another ESC — stay in OscSkipEsc */ }
                    _ => {
                        // Not ST — back to skipping.
                        self.state = State::OscSkip;
                    }
                }
            }
        }
    }

    /// Parse `self.param` into a [`BlockMarker`] and push it; always clears param.
    fn emit_marker(&mut self, out: &mut Vec<BlockMarker>) {
        if let Some(m) = parse_marker(&self.param) {
            out.push(m);
        }
        self.param.clear();
    }
}

// ---------------------------------------------------------------------------
// Param parser
// ---------------------------------------------------------------------------

/// Parse the raw param bytes that follow `"133;"` into a [`BlockMarker`].
///
/// Returns `None` for unrecognised params (future-proofing; unknown variants
/// are silently ignored rather than crashing).
fn parse_marker(param: &[u8]) -> Option<BlockMarker> {
    match param {
        b"A" => Some(BlockMarker::PromptStart),
        b"B" => Some(BlockMarker::CommandStart),
        b"C" => Some(BlockMarker::OutputStart),
        b"D" => Some(BlockMarker::CommandEnd { exit: None }),
        _ if param.starts_with(b"D;") => {
            let s = std::str::from_utf8(&param[2..]).ok()?;
            let code: i32 = s.trim().parse().ok()?;
            Some(BlockMarker::CommandEnd { exit: Some(code) })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an OSC 133 sequence with BEL terminator.
    fn osc133_bel(param: &str) -> Vec<u8> {
        let mut v = vec![0x1B, 0x5D]; // ESC ]
        v.extend_from_slice(b"133;");
        v.extend_from_slice(param.as_bytes());
        v.push(0x07); // BEL
        v
    }

    /// Build an OSC 133 sequence with ST (ESC \) terminator.
    fn osc133_st(param: &str) -> Vec<u8> {
        let mut v = vec![0x1B, 0x5D]; // ESC ]
        v.extend_from_slice(b"133;");
        v.extend_from_slice(param.as_bytes());
        v.extend_from_slice(&[0x1B, 0x5C]); // ESC \
        v
    }

    // ------------------------------------------------------------------
    // Basic marker recognition — BEL terminator

    #[test]
    fn detects_prompt_start_bel() {
        let mut s = Scanner::new();
        assert_eq!(s.scan(&osc133_bel("A")), vec![BlockMarker::PromptStart]);
    }

    #[test]
    fn detects_command_start_bel() {
        let mut s = Scanner::new();
        assert_eq!(s.scan(&osc133_bel("B")), vec![BlockMarker::CommandStart]);
    }

    #[test]
    fn detects_output_start_bel() {
        let mut s = Scanner::new();
        assert_eq!(s.scan(&osc133_bel("C")), vec![BlockMarker::OutputStart]);
    }

    #[test]
    fn detects_command_end_no_exit_bel() {
        let mut s = Scanner::new();
        assert_eq!(
            s.scan(&osc133_bel("D")),
            vec![BlockMarker::CommandEnd { exit: None }]
        );
    }

    #[test]
    fn detects_command_end_exit_zero_bel() {
        let mut s = Scanner::new();
        assert_eq!(
            s.scan(&osc133_bel("D;0")),
            vec![BlockMarker::CommandEnd { exit: Some(0) }]
        );
    }

    #[test]
    fn detects_command_end_exit_nonzero_bel() {
        let mut s = Scanner::new();
        assert_eq!(
            s.scan(&osc133_bel("D;127")),
            vec![BlockMarker::CommandEnd { exit: Some(127) }]
        );
    }

    #[test]
    fn negative_exit_code_bel() {
        // Some shells emit D;-1 for signal-terminated commands.
        let mut s = Scanner::new();
        assert_eq!(
            s.scan(&osc133_bel("D;-1")),
            vec![BlockMarker::CommandEnd { exit: Some(-1) }]
        );
    }

    // ------------------------------------------------------------------
    // ST terminator (ESC \)

    #[test]
    fn detects_prompt_start_st() {
        let mut s = Scanner::new();
        assert_eq!(s.scan(&osc133_st("A")), vec![BlockMarker::PromptStart]);
    }

    #[test]
    fn detects_command_end_exit_st() {
        let mut s = Scanner::new();
        assert_eq!(
            s.scan(&osc133_st("D;1")),
            vec![BlockMarker::CommandEnd { exit: Some(1) }]
        );
    }

    // ------------------------------------------------------------------
    // Multiple markers in a single chunk

    #[test]
    fn multiple_markers_in_one_chunk() {
        let mut s = Scanner::new();
        let mut data = Vec::new();
        data.extend(osc133_bel("A")); // prompt start
        data.extend_from_slice(b"$ "); // prompt text
        data.extend(osc133_bel("B")); // command start
        data.extend_from_slice(b"ls\r\n"); // user input
        data.extend(osc133_bel("C")); // output start
        data.extend_from_slice(b"file.txt\r\n"); // command output
        data.extend(osc133_bel("D;0")); // command end, exit 0
        assert_eq!(
            s.scan(&data),
            vec![
                BlockMarker::PromptStart,
                BlockMarker::CommandStart,
                BlockMarker::OutputStart,
                BlockMarker::CommandEnd { exit: Some(0) },
            ]
        );
    }

    // ------------------------------------------------------------------
    // Chunk-boundary robustness

    #[test]
    fn sequence_split_across_two_chunks_bel() {
        // Try every possible split point within a D;42 sequence.
        let full = osc133_bel("D;42");
        for split in 1..full.len() {
            let mut s = Scanner::new();
            let mut got: Vec<BlockMarker> = s.scan(&full[..split]);
            got.extend(s.scan(&full[split..]));
            assert_eq!(
                got,
                vec![BlockMarker::CommandEnd { exit: Some(42) }],
                "BEL split at byte {split}"
            );
        }
    }

    #[test]
    fn sequence_split_across_two_chunks_st() {
        let full = osc133_st("B");
        for split in 1..full.len() {
            let mut s = Scanner::new();
            let mut got: Vec<BlockMarker> = s.scan(&full[..split]);
            got.extend(s.scan(&full[split..]));
            assert_eq!(
                got,
                vec![BlockMarker::CommandStart],
                "ST split at byte {split}"
            );
        }
    }

    #[test]
    fn full_cycle_byte_by_byte() {
        // Drive the scanner one byte at a time through a full A/B/C/D;0 cycle.
        let mut data = Vec::new();
        data.extend(osc133_bel("A"));
        data.extend(osc133_bel("B"));
        data.extend(osc133_bel("C"));
        data.extend(osc133_bel("D;0"));

        let mut s = Scanner::new();
        let mut all = Vec::new();
        for byte in &data {
            all.extend(s.scan(std::slice::from_ref(byte)));
        }
        assert_eq!(
            all,
            vec![
                BlockMarker::PromptStart,
                BlockMarker::CommandStart,
                BlockMarker::OutputStart,
                BlockMarker::CommandEnd { exit: Some(0) },
            ]
        );
    }

    // ------------------------------------------------------------------
    // Noise immunity

    #[test]
    fn ignores_other_osc_sequences() {
        // OSC 0 (window title set) must not produce markers.
        let mut data = vec![0x1B, 0x5D]; // ESC ]
        data.extend_from_slice(b"0;My terminal title");
        data.push(0x07); // BEL
        let mut s = Scanner::new();
        assert!(s.scan(&data).is_empty(), "non-133 OSC should yield no markers");
    }

    #[test]
    fn normal_csi_escape_not_confused() {
        // Bold-on SGR sequence must not trigger marker parsing.
        let data = b"\x1B[1mbold\x1B[0m";
        let mut s = Scanner::new();
        assert!(s.scan(data).is_empty());
    }

    #[test]
    fn lone_escape_at_chunk_boundary_then_normal_output() {
        // ESC at end of chunk, then a CSI byte — must not produce markers.
        let mut s = Scanner::new();
        let m1 = s.scan(&[0x1B]);
        let m2 = s.scan(b"[1m"); // '[' is 0x5B, not 0x5D (']')
        assert!(m1.is_empty() && m2.is_empty());
    }

    #[test]
    fn unknown_param_yields_no_marker() {
        // Unrecognised params (forward-compat) are silently discarded.
        let mut s = Scanner::new();
        assert!(
            s.scan(&osc133_bel("Z")).is_empty(),
            "unknown param should not produce a marker"
        );
    }

    #[test]
    fn mixed_bel_and_st_terminators_in_sequence() {
        // Real shells may mix terminators; we handle both.
        let mut data = Vec::new();
        data.extend(osc133_bel("A"));  // BEL
        data.extend(osc133_st("B"));   // ST
        data.extend(osc133_bel("C"));  // BEL
        data.extend(osc133_st("D;0")); // ST
        let mut s = Scanner::new();
        assert_eq!(
            s.scan(&data),
            vec![
                BlockMarker::PromptStart,
                BlockMarker::CommandStart,
                BlockMarker::OutputStart,
                BlockMarker::CommandEnd { exit: Some(0) },
            ]
        );
    }
}
