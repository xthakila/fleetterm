//! Terminal cell-grid renderer + keyboard forwarding for FleetTerm.
//!
//! Implements the B-hybrid architecture from `docs/terminal-element-design.md`:
//!   * Layer 1 — explicit, pixel-snapped background quads (seam-free).
//!   * Layer 2 — `ShapedLine::paint` with `background_color: None` on every run
//!               (glyphs follow shaper positions; acceptable for ASCII + common box-drawing).
//!   * Layer 3 — block cursor (focused) / hollow cursor (unfocused).
//!
//! `encode_key` converts a GPUI `Keystroke` into PTY bytes (VT100 / xterm encoding).

use gpui::{
    App, Bounds, Element, ElementId, FocusHandle, FontWeight, GlobalElementId,
    Hsla, InspectorElementId, IntoElement, Keystroke, LayoutId, Modifiers, Pixels,
    Rgba, SharedString, Style, TextRun, Window, fill, font, point, px,
};
use protocol::CellSnap;

// ── sentinel colour ──────────────────────────────────────────────────────────

/// Packed colour value the daemon uses to mean "use the terminal default".
pub const DEFAULT_SENTINEL: u32 = 0xFF00_0000;

// Theme fallback colours (matching the v2 palette in main.rs).
// color::TERM = 0x1a1b26, color::TEXT = 0xc0caf5
const THEME_BG_R: f32 = 0x1a as f32 / 255.0;
const THEME_BG_G: f32 = 0x1b as f32 / 255.0;
const THEME_BG_B: f32 = 0x26 as f32 / 255.0;

const THEME_FG_R: f32 = 0xc0 as f32 / 255.0;
const THEME_FG_G: f32 = 0xca as f32 / 255.0;
const THEME_FG_B: f32 = 0xf5 as f32 / 255.0;

// Cursor colour (#7dcfff — bright cyan on the Tokyo Night palette).
const CURSOR_R: f32 = 0x7d as f32 / 255.0;
const CURSOR_G: f32 = 0xcf as f32 / 255.0;
const CURSOR_B: f32 = 0xff as f32 / 255.0;

// ── colour helpers ────────────────────────────────────────────────────────────

/// Convert a packed 0x00RRGGBB daemon colour to `Hsla`.
/// The sentinel 0xFF000000 maps to the supplied theme default (r, g, b).
fn cell_color(packed: u32, default_r: f32, default_g: f32, default_b: f32) -> Hsla {
    if packed == DEFAULT_SENTINEL {
        Rgba {
            r: default_r,
            g: default_g,
            b: default_b,
            a: 1.0,
        }
        .into()
    } else {
        let r = ((packed >> 16) & 0xFF) as f32 / 255.0;
        let g = ((packed >> 8) & 0xFF) as f32 / 255.0;
        let b = (packed & 0xFF) as f32 / 255.0;
        Rgba { r, g, b, a: 1.0 }.into()
    }
}

fn cursor_color() -> Hsla {
    Rgba {
        r: CURSOR_R,
        g: CURSOR_G,
        b: CURSOR_B,
        a: 1.0,
    }
    .into()
}

/// Exact equality for `Hsla` (bit-level) so run-merging is deterministic.
fn colors_equal(a: Hsla, b: Hsla) -> bool {
    a.h.to_bits() == b.h.to_bits()
        && a.s.to_bits() == b.s.to_bits()
        && a.l.to_bits() == b.l.to_bits()
        && a.a.to_bits() == b.a.to_bits()
}

// ── grid state ────────────────────────────────────────────────────────────────

/// The last received `Event::Grid` for a session — everything needed to render.
#[derive(Clone, Debug)]
pub struct GridState {
    pub cols: u16,
    pub rows: u16,
    /// Zero-based (cursor_col, cursor_row).
    pub cursor: (u16, u16),
    /// Row-major: index = row * cols + col.
    pub cells: Vec<CellSnap>,
}

impl GridState {
    pub fn new(
        cols: u16,
        rows: u16,
        cursor_col: u16,
        cursor_row: u16,
        cells: Vec<CellSnap>,
    ) -> Self {
        Self {
            cols,
            rows,
            cursor: (cursor_col, cursor_row),
            cells,
        }
    }

    fn cell_at(&self, col: u16, row: u16) -> &CellSnap {
        let idx = row as usize * self.cols as usize + col as usize;
        if idx < self.cells.len() {
            &self.cells[idx]
        } else {
            static BLANK: CellSnap = CellSnap {
                ch: ' ',
                fg: DEFAULT_SENTINEL,
                bg: DEFAULT_SENTINEL,
                bold: false,
                inverse: false,
            };
            &BLANK
        }
    }
}

// ── background span ───────────────────────────────────────────────────────────

struct BgSpan {
    start_col: u16,
    end_col: u16, // exclusive
    color: Hsla,
}

/// For one row, merge adjacent cells with identical effective bg into spans.
fn bg_spans(grid: &GridState, row: u16) -> Vec<BgSpan> {
    let mut spans: Vec<BgSpan> = Vec::new();
    for col in 0..grid.cols {
        let cell = grid.cell_at(col, row);
        let effective_bg = if cell.inverse {
            // Inverse: use fg packed colour as the background.
            cell_color(cell.fg, THEME_FG_R, THEME_FG_G, THEME_FG_B)
        } else {
            cell_color(cell.bg, THEME_BG_R, THEME_BG_G, THEME_BG_B)
        };

        if let Some(last) = spans.last_mut() {
            if colors_equal(last.color, effective_bg) {
                last.end_col = col + 1;
                continue;
            }
        }
        spans.push(BgSpan {
            start_col: col,
            end_col: col + 1,
            color: effective_bg,
        });
    }
    spans
}

// ── text run builder ──────────────────────────────────────────────────────────

/// For one row, build the display string + `TextRun` vec (fg + bold merged; bg = None).
///
/// Using `font("Zed Mono")` which is the monospace family FleetTerm uses.
/// The font name falls back to the theme monospace via GPUI's font stack.
fn build_row_runs(grid: &GridState, row: u16) -> (SharedString, Vec<TextRun>) {
    let base_font = font("Zed Mono");
    let mut text = String::with_capacity(grid.cols as usize);
    let mut runs: Vec<TextRun> = Vec::new();

    for col in 0..grid.cols {
        let cell = grid.cell_at(col, row);
        // Null characters represent empty cells; render as space.
        let ch = if cell.ch == '\0' { ' ' } else { cell.ch };

        let effective_fg: Hsla = if cell.inverse {
            // Inverse: fg drawn in bg colour.
            cell_color(cell.bg, THEME_BG_R, THEME_BG_G, THEME_BG_B)
        } else {
            cell_color(cell.fg, THEME_FG_R, THEME_FG_G, THEME_FG_B)
        };

        let font_for_cell = if cell.bold {
            base_font.clone().bold()
        } else {
            base_font.clone()
        };

        let ch_bytes = ch.len_utf8();
        text.push(ch);

        // Merge with the previous run if fg colour + bold match exactly.
        let is_bold = cell.bold;
        if let Some(last) = runs.last_mut() {
            let last_is_bold = last.font.weight == FontWeight::BOLD;
            if is_bold == last_is_bold && colors_equal(last.color, effective_fg) {
                last.len += ch_bytes;
                continue;
            }
        }

        runs.push(TextRun {
            len: ch_bytes,
            font: font_for_cell,
            color: effective_fg,
            background_color: None, // Layer 1 handles bg; no shaper bg quads.
            underline: None,
            strikethrough: None,
        });
    }

    (text.into(), runs)
}

// ── prepaint state ────────────────────────────────────────────────────────────

struct RowShaped {
    line: gpui::ShapedLine,
    bg: Vec<BgSpan>,
}

pub struct GridPrepaint {
    rows: Vec<RowShaped>,
    cell_w: Pixels,
    line_h: Pixels,
}

// ── GridElement ───────────────────────────────────────────────────────────────

/// Custom GPUI `Element` that renders a `GridState` (terminal cell grid).
///
/// Hosted inside a `div` in `FleetTermApp::render` that carries the `FocusHandle`
/// and `on_key_down` listener.  The element itself only paints; key handling is in
/// the parent `div`.
pub struct GridElement {
    pub grid: GridState,
    pub focus_handle: FocusHandle,
}

impl IntoElement for GridElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for GridElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<GridPrepaint>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Size the layout block to exactly cols×rows cells.
        let style = window.text_style();
        let font_id = window.text_system().resolve_font(&style.font());
        let font_size = style.font_size.to_pixels(window.rem_size());
        let cell_w = window
            .text_system()
            .em_advance(font_id, font_size)
            .unwrap_or(px(8.0));
        let line_h = window.line_height();

        let mut layout_style = Style::default();
        // Pixels * f32 → Pixels (via impl Mul<f32>); then Pixels → Length via From chain.
        layout_style.size.width = (cell_w * self.grid.cols as f32).into();
        layout_style.size.height = (line_h * self.grid.rows as f32).into();

        (window.request_layout(layout_style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        // Measure once.
        let style = window.text_style();
        let font_id = window.text_system().resolve_font(&style.font());
        let font_size = style.font_size.to_pixels(window.rem_size());
        let cell_w = window
            .text_system()
            .em_advance(font_id, font_size)
            .unwrap_or(px(8.0));
        let line_h = window.line_height();

        let grid = &self.grid;
        let mut shaped_rows = Vec::with_capacity(grid.rows as usize);

        for row in 0..grid.rows {
            let bg = bg_spans(grid, row);
            let (row_text, runs) = build_row_runs(grid, row);

            // Shape the line; bg is None on all runs (Layer 1 handles it).
            let line = window
                .text_system()
                .shape_line(row_text, font_size, &runs, None);

            shaped_rows.push(RowShaped { line, bg });
        }

        Some(GridPrepaint {
            rows: shaped_rows,
            cell_w,
            line_h,
        })
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(state) = prepaint.as_mut() else {
            return;
        };

        let sf = window.scale_factor();
        let cell_w = state.cell_w;
        let line_h = state.line_h;
        let grid = &self.grid;

        // Device-pixel snap: round to the nearest physical pixel to kill seams.
        // Uses From<Pixels> for f32 to avoid accessing the pub(crate) inner field.
        let snap = |p: Pixels| px((f32::from(p) * sf).round() / sf);

        // ── LAYER 1: Background quads (pixel-snapped, seam-free) ─────────────
        for (r, row_data) in state.rows.iter().enumerate() {
            let y0 = snap(bounds.origin.y + line_h * r as f32);
            let y1 = snap(y0 + line_h);
            for span in &row_data.bg {
                let x0 = snap(bounds.origin.x + cell_w * span.start_col as f32);
                let x1 = snap(bounds.origin.x + cell_w * span.end_col as f32);
                window.paint_quad(fill(
                    Bounds::from_corners(point(x0, y0), point(x1, y1)),
                    span.color,
                ));
            }
        }

        // ── LAYER 2: Glyphs via ShapedLine::paint ────────────────────────────
        // All TextRun.background_color == None so the shaper won't paint bg quads.
        for (r, row_data) in state.rows.iter().enumerate() {
            let y0 = snap(bounds.origin.y + line_h * r as f32);
            let origin = point(snap(bounds.origin.x), y0);
            // Non-fatal: glyph atlas overflows are recoverable next frame.
            let _ = row_data.line.paint(origin, line_h, window, cx);
        }

        // ── LAYER 3: Cursor ───────────────────────────────────────────────────
        let (cur_col, cur_row) = grid.cursor;
        let in_bounds =
            (cur_row as usize) < state.rows.len() && (cur_col as usize) < grid.cols as usize;
        if in_bounds {
            let cx0 = snap(bounds.origin.x + cell_w * cur_col as f32);
            let cy0 = snap(bounds.origin.y + line_h * cur_row as f32);
            let cx1 = snap(cx0 + cell_w);
            let cy1 = snap(cy0 + line_h);

            if self.focus_handle.is_focused(window) {
                // Solid block cursor.
                window.paint_quad(fill(
                    Bounds::from_corners(point(cx0, cy0), point(cx1, cy1)),
                    cursor_color(),
                ));
            } else {
                // Hollow box cursor — four 1-device-pixel-wide quads.
                let bw = px(1.0 / sf);
                // top
                window.paint_quad(fill(
                    Bounds::from_corners(point(cx0, cy0), point(cx1, snap(cy0 + bw))),
                    cursor_color(),
                ));
                // bottom
                window.paint_quad(fill(
                    Bounds::from_corners(point(cx0, snap(cy1 - bw)), point(cx1, cy1)),
                    cursor_color(),
                ));
                // left
                window.paint_quad(fill(
                    Bounds::from_corners(point(cx0, cy0), point(snap(cx0 + bw), cy1)),
                    cursor_color(),
                ));
                // right
                window.paint_quad(fill(
                    Bounds::from_corners(point(snap(cx1 - bw), cy0), point(cx1, cy1)),
                    cursor_color(),
                ));
            }
        }
    }
}

// ── Keystroke → PTY byte encoding ────────────────────────────────────────────

/// Encode a GPUI `Keystroke` to PTY bytes (VT100 / xterm).
///
/// Returns `None` if no bytes should be sent (e.g. bare modifier press).
/// Reads `keystroke.key` for named keys and `keystroke.key_char` for the
/// actual typed character (handles shifted / IME / dead-key input).
pub fn encode_key(k: &Keystroke) -> Option<Vec<u8>> {
    let m = &k.modifiers;
    let mut out: Vec<u8> = Vec::new();

    match k.key.as_str() {
        // ── Named keys ───────────────────────────────────────────────────────
        "enter" | "return" => out.push(0x0D),
        "backspace" => out.push(0x7F),
        "tab" => {
            if m.shift {
                out.extend_from_slice(b"\x1b[Z"); // CSI Z = Shift-Tab (back-tab)
            } else {
                out.push(0x09);
            }
        }
        "escape" => out.push(0x1B),
        "delete" => out.extend_from_slice(b"\x1b[3~"),
        "insert" => out.extend_from_slice(b"\x1b[2~"),
        "home" => out.extend_from_slice(b"\x1b[H"),
        "end" => out.extend_from_slice(b"\x1b[F"),
        "pageup" => out.extend_from_slice(b"\x1b[5~"),
        "pagedown" => out.extend_from_slice(b"\x1b[6~"),

        // ── Arrow keys (modified variants: ESC [ 1 ; n <final>) ──────────────
        "up" => out.extend(csi_arrow(b'A', m)),
        "down" => out.extend(csi_arrow(b'B', m)),
        "right" => out.extend(csi_arrow(b'C', m)),
        "left" => out.extend(csi_arrow(b'D', m)),

        // ── Function keys ────────────────────────────────────────────────────
        "f1" => out.extend_from_slice(b"\x1bOP"),
        "f2" => out.extend_from_slice(b"\x1bOQ"),
        "f3" => out.extend_from_slice(b"\x1bOR"),
        "f4" => out.extend_from_slice(b"\x1bOS"),
        "f5" => out.extend_from_slice(b"\x1b[15~"),
        "f6" => out.extend_from_slice(b"\x1b[17~"),
        "f7" => out.extend_from_slice(b"\x1b[18~"),
        "f8" => out.extend_from_slice(b"\x1b[19~"),
        "f9" => out.extend_from_slice(b"\x1b[20~"),
        "f10" => out.extend_from_slice(b"\x1b[21~"),
        "f11" => out.extend_from_slice(b"\x1b[23~"),
        "f12" => out.extend_from_slice(b"\x1b[24~"),

        // ── Single-character keys (printable + Ctrl / Alt combos) ─────────────
        single if single.chars().count() == 1 => {
            let ch = single.chars().next().unwrap();

            if m.control && m.alt {
                // Ctrl+Alt: ESC prefix + ctrl byte
                if let Some(b) = ctrl_byte(ch) {
                    out.push(0x1B);
                    out.push(b);
                }
            } else if m.control {
                // Ctrl-<key> → C0 control character
                if let Some(b) = ctrl_byte(ch) {
                    out.push(b);
                }
            } else if m.alt {
                // Alt = ESC prefix (meta mode)
                out.push(0x1B);
                let s = k.key_char.as_deref().unwrap_or(single);
                out.extend_from_slice(s.as_bytes());
            } else {
                // Plain printable: prefer key_char (carries shifted/IME char).
                let s = k.key_char.as_deref().unwrap_or(single);
                out.extend_from_slice(s.as_bytes());
            }
        }

        // ── Catch-all: forward key_char if present ────────────────────────────
        _ => {
            if let Some(kc) = &k.key_char {
                out.extend_from_slice(kc.as_bytes());
            }
        }
    }

    if out.is_empty() { None } else { Some(out) }
}

/// Map Ctrl+<char> to a C0 control byte (normalised to upper-case).
fn ctrl_byte(ch: char) -> Option<u8> {
    let u = ch.to_ascii_uppercase() as u8;
    match u {
        b'@' => Some(0x00),            // Ctrl-@ / Ctrl-Space = NUL
        b'A'..=b'Z' => Some(u & 0x1F), // Ctrl-A..Z = 0x01..0x1A
        b'[' => Some(0x1B),            // Ctrl-[ = ESC
        b'\\' => Some(0x1C),           // Ctrl-backslash = FS
        b']' => Some(0x1D),            // Ctrl-] = GS
        b'^' => Some(0x1E),            // Ctrl-^ = RS
        b'_' | b'?' => Some(0x1F),     // Ctrl-_ / Ctrl-? = US
        _ => None,
    }
}

/// Build an arrow key byte sequence with xterm modifier encoding.
///
/// Plain:    ESC [ <final>
/// Modified: ESC [ 1 ; <mod_n> <final>   (mod_n = 1 + shift + 2*alt + 4*ctrl)
fn csi_arrow(final_byte: u8, m: &Modifiers) -> Vec<u8> {
    let mod_n: u8 = 1 + (m.shift as u8) + 2 * (m.alt as u8) + 4 * (m.control as u8);
    if mod_n > 1 {
        format!("\x1b[1;{}{}", mod_n, final_byte as char).into_bytes()
    } else {
        vec![0x1B, b'[', final_byte]
    }
}
