# FleetTerm — Terminal Cell-Grid Renderer + Keyboard Forwarding (design)

> Source-grounded GPUI 0.2.2 design from the phases workflow. For the next iteration (replaces the v1 text-pane terminal).

GLM confirms my key concerns and validates the refined decomposition (explicit pixel-snapped background quads + glyphs-only shaped runs). This is the architecturally correct approach for a terminal. I now have everything needed to write the implementation-ready spec.

---

## Architecture: FleetTerm GPUI Terminal — Cell-Grid Renderer + Keyboard Input Forwarding

All signatures below are quoted verbatim from gpui 0.2.2 at `/home/mmt-aeoru/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/gpui-0.2.2/`. File references are absolute.

### Context

FleetTerm renders remote PTY sessions. Each frame the view receives a full `Event::Grid { cols, rows, cursor, cells: Vec<CellSnap{ch,fg,bg,bold,inverse}> }` (row-major). Two pieces are hard in GPUI: (1) painting the cell grid efficiently and seam-free, and (2) capturing keystrokes on a focused element and encoding them to PTY bytes for `Request::Input{ target: Session(id), data }`. GPUI is immediate-mode: the element tree is rebuilt every frame and the renderer is a custom `impl Element` (the `&'static str`/`input.rs` `TextElement` pattern), not a tree of `div`s.

---

## PART 1 — TERMINAL CELL-GRID RENDERER

### Real APIs this design is built on (quoted)

Element trait — `src/element.rs:51-104`:
```rust
pub trait Element: 'static + IntoElement {
    type RequestLayoutState: 'static;
    type PrepaintState: 'static;
    fn id(&self) -> Option<ElementId>;
    fn source_location(&self) -> Option<&'static panic::Location<'static>>;
    fn request_layout(&mut self, id: Option<&GlobalElementId>, inspector_id: Option<&InspectorElementId>,
        window: &mut Window, cx: &mut App) -> (LayoutId, Self::RequestLayoutState);
    fn prepaint(&mut self, id: Option<&GlobalElementId>, inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>, request_layout: &mut Self::RequestLayoutState,
        window: &mut Window, cx: &mut App) -> Self::PrepaintState;
    fn paint(&mut self, id: Option<&GlobalElementId>, inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>, request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState, window: &mut Window, cx: &mut App);
}
```

Cell measuring — `src/text_system.rs`:
- `pub fn em_advance(&self, font_id: FontId, font_size: Pixels) -> Result<Pixels>` (`:218`, advance width of `m`).
- `pub fn advance(&self, font_id: FontId, font_size: Pixels, ch: char) -> Result<Size<Pixels>>` (`:197`).
- `pub fn bounding_box(&self, font_id: FontId, font_size: Pixels) -> Bounds<Pixels>` (`:173`).
- `pub fn resolve_font(&self, font: &Font) -> FontId` (`:150`) — Font → FontId.
- `pub fn shape_line(&self, text: SharedString, font_size: Pixels, runs: &[TextRun], force_width: Option<Pixels>) -> ShapedLine` (`:365`).

`WindowTextSystem` (`src/text_system.rs:333`) holds `#[deref] text_system: Arc<TextSystem>`, so `window.text_system()` exposes both `shape_line` (its own) and `resolve_font`/`advance`/`em_advance`/`bounding_box` via Deref. `window.text_system()` returns `&Arc<WindowTextSystem>` (`src/window.rs:1435`).

`TextRun` — `src/text_system.rs:733`:
```rust
pub struct TextRun {
    pub len: usize,                          // utf-8 BYTES, not chars
    pub font: Font,
    pub color: Hsla,
    pub background_color: Option<Hsla>,
    pub underline: Option<UnderlineStyle>,
    pub strikethrough: Option<StrikethroughStyle>,
}
```

Painting — `src/window.rs`:
- `pub fn paint_quad(&mut self, quad: PaintQuad)` (`:2839`).
- `pub fn paint_glyph(&mut self, origin: Point<Pixels>, font_id: FontId, glyph_id: GlyphId, font_size: Pixels, color: Hsla) -> Result<()>` (`:2948`) — "The y component of the origin is the baseline of the glyph."
- `pub fn fill(bounds: impl Into<Bounds<Pixels>>, background: impl Into<Background>) -> PaintQuad` (`:5078`).
- `pub fn line_height(&self) -> Pixels` (`:1868`); `pub fn text_style(&self) -> TextStyle` (`:1440`); `TextStyle::font()` → `Font` (`src/style.rs:459`); `pub fn rem_size(&self) -> Pixels` (`:1821`); `style.font_size.to_pixels(window.rem_size())` (per `input.rs:487`).
- `ShapedLine::paint(&self, origin: Point<Pixels>, line_height: Pixels, window, cx) -> Result<()>` (`src/text_system/line.rs:63`). **It paints glyphs AND, separately, run backgrounds** (`paint_line` paints glyphs; `paint_line_background` paints bg quads from glyph positions). `ShapedLine::layout` is `Arc<LineLayout>` with `runs: Vec<ShapedRun>`, each `ShapedGlyph { id, position, index, is_emoji }` (`src/text_system/line_layout.rs:16-54`), plus `x_for_index` (`:105`).

### Three approaches compared

**(a) Per-row flex_row of styled `div` spans (merged runs).**
`div().bg(bg).text_color(fg).child(run_text)` inside `flex().flex_row()`, one `div` per row. Pros: trivial; uses only the high-level API (`text.rs`/`painting.rs` style). Cons: For 200×50 with worst-case alternating styles, you generate thousands of `div`s + taffy layout nodes **every frame** (tree is rebuilt each frame per `element.rs:11-14`). Taffy flex layout over ~10k nodes per frame is the dominant cost and unbounded by content churn. Cell x-positions are decided by flex/text layout, **not** pinned to a monospace grid → box-drawing chars won't tile, backgrounds are flex-box rects with rounding seams. Rejected: doesn't scale and gives up grid control.

**(c) `StyledText`/`TextRun` with per-line highlight ranges.**
`StyledText::new(row).with_highlights([...])` (`text.rs:75`). Pros: one element per row, run styling for free. Cons: `StyledText` is a layout-flow text element — it does **not** force a monospace cell grid, gives no per-cell x control, and (like (a)) leaves background fill and box-drawing tiling to the shaper. Same alignment/seam failure as (a) with less control. Rejected.

**(b) Custom `impl Element`, prepaint shapes / paint draws — RECOMMENDED, in a refined "B-hybrid" form.**

The naive B (one `shape_line` per row with `force_width = Some(cell_width*cols)`, let `ShapedLine::paint` draw backgrounds) is WRONG for a terminal — confirmed by stress-test:

- **`force_width` does not pin glyph N to `N*cell_width`.** It constrains *total* line width; with any non-uniform advance (CJK, emoji, fallback fonts, or cell_width slightly off the font's true em) the shaper distributes the delta across the line. Box-drawing glyphs (`│ ─ ┼ ┌`) only tile when each sits on an integer cell boundary; distributed width breaks corners and leaves seams.
- **Letting `ShapedLine` paint backgrounds** (`paint_line_background`, `src/text_system/line.rs:425`) derives bg-quad bounds from *glyph positions*. At fractional DPI (1.25/1.5/1.75) adjacent different-color run boundaries land on sub-pixel x → 1px bleed or overlap; row-to-row vertical seams appear if `line_height` isn't an integer device-pixel.

#### Recommended decomposition (B-hybrid: explicit grid backgrounds + glyphs-only shaped runs)

Compute the cell grid geometry yourself, pixel-snap it, paint backgrounds as your own quads, then shape each row for **glyphs only** (`background_color: None` on every `TextRun`).

Geometry, computed once and cached (recompute only when font_size / scale_factor change):
```
font     = window.text_style().font();                       // monospace family configured on the view
font_id  = window.text_system().resolve_font(&font);
font_size= window.text_style().font_size.to_pixels(window.rem_size());
cell_w   = window.text_system().em_advance(font_id, font_size)?;   // monospace advance
line_h   = window.line_height();
// baseline within a cell, matching gpui's own glyph baseline math (line.rs:207):
//   padding_top   = (line_h - ascent - descent) / 2
//   baseline_y    = padding_top + ascent
// snap cell origins to device pixels to kill seams:
let snap = |p: Pixels, sf: f32| px((p.0 * sf).round() / sf);
```

`request_layout` — request a fixed block sized to the grid (pattern from `input.rs:418`):
```rust
fn request_layout(&mut self, _id, _ins, window, cx) -> (LayoutId, ()) {
    let mut style = Style::default();
    style.size.width  = (self.cols as f32 * cell_w).into();
    style.size.height = (self.rows as f32 * line_h).into();
    (window.request_layout(style, [], cx), ())
}
```

`prepaint` — do all shaping here (prepaint is the shape phase; keeps `paint` cheap). For each row r:
1. Skip continuation/zero-width cells (see Risks: cell model must carry `width`). Concatenate base chars into a `String`, building `Vec<TextRun>` by **merging adjacent cells with identical (effective_fg, bold)** — color/bold only; **bg is NOT in the run** (`background_color: None`). `inverse` is resolved at build time by swapping fg/bg. `TextRun.len` = sum of **utf-8 byte lengths** (`ch.len_utf8()`), not char counts.
2. Build the per-row background spans separately: merge adjacent cells with identical effective bg into `(start_col, end_col, Hsla)` rects.
3. `let line = window.text_system().shape_line(row_str.into(), font_size, &runs, None);` — **no `force_width`**; we place glyphs ourselves via the cell grid, not via the line layout.
4. Stash `Vec<(ShapedLine, Vec<BgSpan>)>` plus a per-row content hash in `PrepaintState` for caching.

`paint` — three ordered layers inside `bounds`:
```rust
// LAYER 1: backgrounds — explicit, pixel-snapped quads (seam-free, full cell coverage)
for (r, spans) in rows {
  let y = snap(bounds.origin.y + r as f32 * line_h, sf);
  for span in spans {
    let x0 = snap(bounds.origin.x + span.start as f32 * cell_w, sf);
    let x1 = snap(bounds.origin.x + span.end   as f32 * cell_w, sf);
    window.paint_quad(fill(
        Bounds::from_corners(point(x0, y), point(x1, snap(y + line_h, sf))),
        span.color));
  }
}
// LAYER 2: glyphs — per cell, positioned on the grid, NOT on shaped x.
// For strict box-drawing tiling, paint each glyph at its own cell origin using paint_glyph:
let baseline = padding_top + ascent;
for r in rows {
  let line = &shaped[r];
  for run in &line.layout.runs {
    for g in &run.glyphs {
      let col = cell_col_for_byte_index(g.index);          // map utf8 byte index -> cell column
      let gx  = snap(bounds.origin.x + col as f32 * cell_w, sf);
      let gy  = bounds.origin.y + r as f32 * line_h + baseline;
      window.paint_glyph(point(gx, gy), run.font_id, g.id, font_size, run_color)?;
    }
  }
}
// LAYER 3: cursor
```
This `paint_glyph` loop is what guarantees box-drawing seams close: each glyph is pinned to `col*cell_w`, independent of the shaper's natural advance. (Simpler fallback for ASCII-only/dev builds: `line.paint(origin, line_h, window, cx)` after Layer 1 with `background_color: None` runs — acceptable but loses strict per-cell x; keep the `paint_glyph` path as the default.)

#### Cursor (Layer 3)
Cursor lives in `Event::Grid { cursor: (col,row) }`. Block cursor:
```rust
let cx0 = snap(bounds.origin.x + cursor.col as f32 * cell_w, sf);
let cy0 = snap(bounds.origin.y + cursor.row as f32 * line_h, sf);
if focused {  // focus_handle.is_focused(window) — see Part 2
  window.paint_quad(fill(Bounds::from_corners(point(cx0,cy0),
      point(snap(cx0+cell_w,sf), snap(cy0+line_h,sf))), cursor_color));  // solid block
  // re-paint the cell's glyph inverted, on TOP of the block:
  let cell_line = window.text_system().shape_line(cell_ch.to_string().into(), font_size,
      &[TextRun{len: cell_ch.len_utf8(), font: font.clone(), color: bg_color_under_cursor,
                background_color: None, underline: None, strikethrough: None}], None);
  if let Some(run)=cell_line.layout.runs.first() { if let Some(g)=run.glyphs.first() {
      window.paint_glyph(point(cx0, cy0+baseline), run.font_id, g.id, font_size, bg_color_under_cursor)?; }}
} else {
  // unfocused: hollow box — four thin paint_quads (top/bottom/left/right), 1px snapped.
}
```
Bar (`│`, width `px(2.)` like `input.rs:499`) and underline cursors are paint_quad variants. Re-shaping one char for the cursor is cheap and avoids extracting a single glyph from the row line. Blink: a timer toggles a `cursor_visible` bool and calls `cx.notify()`; the row didn't change but cursor rows must repaint, so the dirty set is keyed on content hash **and** "contains cursor".

#### Performance (80×24 .. 200×50)
- Shaping cost dominates; gpui caches shaped lines and rasterized glyphs internally (atlas, `paint_glyph` at `:2977`). Worst case 50 shapes/frame is sub-millisecond, but: **cache per row by content hash** and re-shape only changed rows. Unstable run-merging (e.g. fg colors differing by float epsilon) busts the cache every frame — quantize/compare `Hsla` exactly and merge deterministically.
- Reuse a `String::with_capacity(cols)` and `Vec<TextRun>` across rows to avoid per-frame allocation.
- The `paint_glyph` Layer-2 loop is O(cells) primitives but they're cheap atlas blits; backgrounds are O(runs) quads. Both are bounded and GPU-batched.

---

## PART 2 — KEYBOARD INPUT FORWARDING

### Focus + key context (quoted)

From `examples/input.rs` (the authoritative pattern) and `src/elements/div.rs`:
- `fn track_focus(mut self, focus_handle: &FocusHandle) -> Self` (`div.rs:616`) — sets `focusable = true` and `tracked_focus_handle`.
- `fn key_context<C,E>(mut self, key_context: C) -> Self where C: TryInto<KeyContext, Error=E>` (`div.rs:658`).
- `fn on_key_down(mut self, listener: impl Fn(&KeyDownEvent, &mut Window, &mut App) + 'static) -> Self` (`div.rs:881`); fires only in `DispatchPhase::Bubble` (`div.rs:393`).
- `fn on_action<A: Action>(mut self, listener: impl Fn(&A, &mut Window, &mut App) + 'static) -> Self` (`div.rs:854`).
- View implements `Focusable`: `fn focus_handle(&self, _: &App) -> FocusHandle` returning a stored handle created with `cx.focus_handle()` (`input.rs:603-607,704`).
- `focus_handle.is_focused(window) -> bool` (used at `input.rs:552`) gates cursor + input handling.
- Focus the view: `window.focus(&view.focus_handle(cx))` (`input.rs:739`).

`KeyDownEvent` — `src/interactive.rs:22`:
```rust
pub struct KeyDownEvent { pub keystroke: Keystroke, pub is_held: bool }
```
`Keystroke` — `src/platform/keystroke.rs:18`:
```rust
pub struct Keystroke {
    pub modifiers: Modifiers,      // control, alt, shift, platform, function (all bool)
    pub key: String,               // e.g. "a", "enter", "left", "tab", "escape"
    pub key_char: Option<String>,  // the actually-typed char incl. IME/dead-keys: "a", "ß", ...
}
```
`Modifiers` — `src/platform/keystroke.rs:447`: `control, alt, shift, platform, function: bool`.

Named keys gpui emits (`Keystroke::is_printable_key` allow-list, `keystroke.rs:~400-428`): `backspace delete left right up down pageup pagedown insert home end escape tab enter`/`return`, `f1..f35`, etc.

### Architecture

The terminal view owns a `FocusHandle` and a `session_id`. Its `render()` wraps the custom grid Element in a `div` that is the focus + key sink. Two encoding strategies — **recommended: a single `on_key_down` that encodes every keystroke to bytes**, because a terminal must forward *all* keys verbatim (Ctrl-C, arrows, function keys) rather than route them through gpui Actions. (Actions/`KeyBinding` — `input.rs:680` — are right for app commands like Copy/Paste/SplitPane, which we DO bind via `on_action`; raw terminal input goes through `on_key_down`.)

```rust
impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle { self.focus_handle.clone() }
}

impl Render for TerminalView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
          .track_focus(&self.focus_handle(cx))      // makes the div focusable + tracked
          .key_context("Terminal")                   // scopes app KeyBindings to terminal
          .on_key_down(cx.listener(Self::on_key_down))
          // app-level commands still use actions (not forwarded to PTY):
          .on_action(cx.listener(Self::copy))
          .on_action(cx.listener(Self::paste))
          .size_full()
          .child(GridElement { session_id: self.session_id, grid: self.grid.clone(),
                               focus_handle: self.focus_handle.clone() })
    }
}
```
`cx.listener` adapts a `fn(&mut Self, &KeyDownEvent, &mut Window, &mut Context<Self>)` to the required `Fn(&KeyDownEvent,&mut Window,&mut App)` (same pattern as every `on_action`/`on_mouse_down` in `input.rs`).

Inside `GridElement::paint`, register the IME/input handler exactly like `input.rs:540` so dead-keys/IME composition (CJK) reach the PTY too:
```rust
window.handle_input(&self.focus_handle, ElementInputHandler::new(bounds, self.view.clone()), cx);
```
`pub fn handle_input(&mut self, focus_handle: &FocusHandle, input_handler: impl InputHandler, cx: &App)` (`window.rs:3400`) — only active when focused. The view then implements `EntityInputHandler` (`input.rs:261`) and routes committed text from `replace_text_in_range` into `Request::Input{...}`. `on_key_down` handles non-text control keys; `EntityInputHandler` handles composed printable text (avoid double-sending: in `on_key_down`, only encode keys you recognize as control/named or modifier-combined; let plain printable text flow through the input handler — or, simpler for v1, send `key_char` directly from `on_key_down` and skip the IME handler until CJK is needed).

### Keystroke → PTY byte encoding

The handler reads `event.keystroke` (`k.key`, `k.key_char`, `k.modifiers`) and pushes bytes:
```rust
fn on_key_down(&mut self, ev: &KeyDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
    let k = &ev.keystroke;
    let m = &k.modifiers;
    let mut out: Vec<u8> = Vec::new();
    match k.key.as_str() {
        "enter" | "return"   => out.push(0x0D),                       // CR
        "backspace"          => out.push(0x7F),                       // DEL
        "tab"                => out.push(if m.shift { /* see CSI Z */ } else {0x09} ),
        "escape"             => out.push(0x1B),
        "delete"             => out.extend_from_slice(b"\x1b[3~"),
        "up"                 => out.extend_from_slice(csi_arrow(b'A', m)),
        "down"               => out.extend_from_slice(csi_arrow(b'B', m)),
        "right"              => out.extend_from_slice(csi_arrow(b'C', m)),
        "left"               => out.extend_from_slice(csi_arrow(b'D', m)),
        "home"               => out.extend_from_slice(b"\x1b[H"),
        "end"                => out.extend_from_slice(b"\x1b[F"),
        "pageup"             => out.extend_from_slice(b"\x1b[5~"),
        "pagedown"           => out.extend_from_slice(b"\x1b[6~"),
        "f1"                 => out.extend_from_slice(b"\x1bOP"),      // SS3 P
        "f2"                 => out.extend_from_slice(b"\x1bOQ"),
        "f3"                 => out.extend_from_slice(b"\x1bOR"),
        "f4"                 => out.extend_from_slice(b"\x1bOS"),
        "f5"                 => out.extend_from_slice(b"\x1b[15~"),
        // f6..f12: \x1b[17~ 18~ 19~ 20~ 21~ 23~ 24~
        single if single.chars().count() == 1 => {
            let ch = single.chars().next().unwrap();
            if m.control {
                // Ctrl-<key>: control char = key byte & 0x1F for @A-Z[\]^_ ; map letters case-insensitively
                if let Some(b) = ctrl_byte(ch) { out.push(b); }
            } else if m.alt {
                out.push(0x1B);                                       // Alt = ESC prefix (meta)
                out.extend_from_slice(k.key_char.as_deref().unwrap_or(single).as_bytes());
            } else {
                // printable: prefer key_char (carries shifted/IME char), fall back to key
                out.extend_from_slice(k.key_char.as_deref().unwrap_or(single).as_bytes());
            }
        }
        _ => { if let Some(kc) = &k.key_char { out.extend_from_slice(kc.as_bytes()); } }
    }
    if !out.is_empty() {
        cx.emit(/* or call */ Request::Input { target: Session(self.session_id), data: out });
        cx.stop_propagation();   // keep terminal keys from bubbling to app shortcuts
    }
}

fn ctrl_byte(ch: char) -> Option<u8> {       // Ctrl-A..Ctrl-Z => 0x01..0x1A, plus the C0 set
    let u = ch.to_ascii_uppercase() as u8;
    match u {
        b'@'                      => Some(0x00),  // Ctrl-Space / Ctrl-@
        b'A'..=b'Z'               => Some(u & 0x1F),
        b'['                      => Some(0x1B),  // Ctrl-[
        b'\\'                     => Some(0x1C),
        b']'                      => Some(0x1D),
        b'^'                      => Some(0x1E),
        b'_' | b'?'               => Some(0x1F), // Ctrl-_ ; Ctrl-/ often maps here too
        _                         => None,
    }
}
fn csi_arrow(final_byte: u8, m: &Modifiers) -> Vec<u8> {
    // modified arrows: ESC [ 1 ; <mod> <final> ; plain: ESC [ <final>
    let modn = 1 + (m.shift as u8) + 2*(m.alt as u8) + 4*(m.control as u8);
    if modn > 1 { format!("\x1b[1;{}{}", modn, final_byte as char).into_bytes() }
    else        { vec![0x1b, b'[', final_byte] }
}
```

### Mapping table

| Input | Modifiers | PTY bytes | Notes |
|---|---|---|---|
| Enter / return | — | `0x0D` (CR) | apps in newline mode may want `\n`; CR is the default. LNM toggled by app. |
| Backspace | — | `0x7F` (DEL) | the conventional Unix terminal backspace. |
| Tab | — / Shift | `0x09` / `\x1b[Z` | Shift-Tab = CSI Z (back-tab). |
| Escape | — | `0x1B` | |
| Delete (fwd) | — | `\x1b[3~` | |
| Up/Down/Right/Left | — | `\x1b[A` / `B` / `C` / `D` | DECCKM app-mode variant: `\x1bO?` — track app cursor-key mode if FleetTerm parses it. |
| Up/Down/Right/Left | with mods | `\x1b[1;<n><A..D>` | n = 1 + shift + 2·alt + 4·ctrl. |
| Home / End | — | `\x1b[H` / `\x1b[F` | |
| PageUp / PageDown | — | `\x1b[5~` / `\x1b[6~` | |
| Insert | — | `\x1b[2~` | |
| F1–F4 | — | `\x1bOP/Q/R/S` (SS3) | |
| F5–F12 | — | `\x1b[15~ 17~ 18~ 19~ 20~ 21~ 23~ 24~` | |
| printable `x` | — | UTF-8 of `key_char` (fallback `key`) | `key_char` carries shifted/IME char. |
| `x` | Ctrl | `ctrl_byte(x)` | A–Z→0x01–0x1A; `@[\]^_?` → 0x00,0x1B–0x1F. |
| `x` | Alt | `0x1B` + UTF-8(`x`) | meta = ESC prefix. |
| `x` | Ctrl+Alt | `0x1B` + `ctrl_byte(x)` | combine both rules. |

Key correctness notes: encode from `k.key` for named keys (stable across layouts) and from `k.key_char` for printable text (correct for shifted/IME/dead-key input — `keystroke.rs:29-32`). Always `cx.stop_propagation()` after forwarding so terminal keystrokes don't trigger app `KeyBinding`s. App commands (Copy/Paste/new-tab) are bound with `cx.bind_keys([KeyBinding::new("cmd-c", Copy, Some("Terminal"))])` + `.on_action(...)` and are intentionally NOT forwarded.

### Risks

- **Cell model is the #1 correctness risk.** `CellSnap{ch: char}` (one Unicode scalar) cannot represent: CJK wide glyphs (1 char, **2 cells**), zero-width combining marks (1 char, **0 cells**, must attach to the prior base in the same shape segment), and ZWJ emoji (multiple chars, 1 grapheme). **Mitigation: extend the protocol cell to `{ text: SmallString, width: u8, fg, bg, bold, inverse }`** where `width ∈ {0,1,2}`; renderer skips `width==0`/continuation cells when building the row string and advances the cell column by `width`. Map `ShapedGlyph.index` (utf8 byte index) back to cell column via a prebuilt `byte→col` table.
- **Seams at fractional DPI** — mitigated by computing/snapping cell rects to device pixels and painting backgrounds as your own quads (Layer 1), with `background_color: None` on runs so the shaper never paints overlapping bg.
- **Cache busting** from non-deterministic run merging → re-shape every frame. Mitigation: exact `Hsla` comparison + content-hash per row; only re-shape dirty rows.
- **Double-send** if both `on_key_down` and `EntityInputHandler` emit for the same printable key. Mitigation for v1: skip `handle_input` and send everything from `on_key_down` using `key_char`; add the IME path only when CJK composition is required, and at that point gate `on_key_down` to control/named keys only.
- **Cursor-key / keypad app modes (DECCKM, DECKPAM)**: real terminals switch arrows between `\x1b[A` and `\x1bOA`. If FleetTerm's parser honors these, thread the current mode flags into `csi_arrow`/arrow encoding.

### Implementation Order

1. Geometry + measurement: cache `font_id`, `cell_w` (`em_advance`), `line_h`, `baseline`; recompute on font/scale change.
2. `GridElement` skeleton: `impl Element` with `request_layout` sizing to `cols*cell_w × rows*line_h`; empty `prepaint`/`paint`.
3. Layer 1 backgrounds: per-row bg-span merge + pixel-snapped `paint_quad`.
4. Layer 2 glyphs: per-row `shape_line` (runs = fg+bold merged, `background_color: None`) in `prepaint`; `paint_glyph` per glyph at `col*cell_w` in `paint`. Add per-row content-hash cache.
5. Layer 3 cursor: block (focused) / hollow (unfocused), inverted glyph re-paint, blink timer + dirty-row gating on focus via `focus_handle.is_focused(window)`.
6. Focus wiring: `Focusable` for the view, `track_focus` + `key_context` + `window.focus(...)` on open.
7. `on_key_down` encoder + `ctrl_byte`/`csi_arrow` helpers → `Request::Input{ target: Session(id), data }`; `cx.stop_propagation()`.
8. App-command actions (`KeyBinding` + `on_action`) for Copy/Paste/split, scoped to `"Terminal"` context.
9. (When CJK needed) extend cell model to carry `width`/grapheme text; add `handle_input` + `EntityInputHandler` and de-dupe against `on_key_down`.