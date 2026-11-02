## DNEW-I29 The GUI toolkit's architecture: windows and input behind the platform layer, a CPU rasterizer first, our own text, a canvas in the browser, a retained widget tree with a virtualized grid, and a separate `eggdb-gui` binary — Decided by the build loop (by judgement, 2026-11-02); extends D191, refines D6, D8, D15 and D30

Issue #29 asks for the client toolkit's architecture to be settled before the screens M5 and M6e
need, and for a first slice: a native window on Linux, Windows and macOS showing the connect card
of `docs/ux/first-screen.md` (Claude Fable's design) from the design tokens, in light, dark and
high contrast, with a real focus ring and keyboard order. D191 already built the drawing half
(`eggdb-gui`: a software rasterizer, our own TrueType reader over the system's fonts, a PNG
encoder, frames tested offscreen). This decision keeps all of D191 and adds the rest.

- **The platform layer owns windows, input, the clipboard and accessibility (D8).**
  `eggdb_platform::window` is the one interface: `open_window(&WindowOptions)` returns a
  `Box<dyn Window>` with `size()` and `scale()`, `present(width, height, rgba)` and
  `next_event(timeout)` giving `WindowEvent::{Key, Resized, ScaleChanged, Redraw, Focused,
  CloseRequested}`; keys arrive already translated (`Key`, `Modifiers`, `KeyPress`). Each OS's
  backend speaks its own system interface with our own declarations and no third-party code
  (D1): on Linux and the BSDs the X11 protocol directly over the display's Unix socket (from the
  public X Window System Protocol specification, with `MIT-MAGIC-COOKIE-1` from the Xauthority
  file); Win32 on Windows through our own `user32`/`gdi32` declarations; Cocoa on macOS through
  the Objective-C runtime, with AppKit, Core Graphics and Core Foundation loaded by `dlopen`
  only when a window opens, so the server's binary gains no framework load commands; Wayland
  after them. The OS clipboard and the accessibility bridges (UI Automation, NSAccessibility, AT-SPI2, ARIA in the
  browser) join the same module; the toolkit keeps one accessibility tree per window and the
  platform layer exposes it. The engine never depends on any of this.
- **A CPU rasterizer first, the GPU later.** Every frame is drawn by D191's antialiased software
  rasterizer into an RGBA framebuffer and presented whole (`PutImage` on X11, a DIB section on
  Windows, a `CGImage` layer on macOS, `putImageData` in the browser). It runs on every OS, every
  CPU and in `no_std`-adjacent targets, has nothing to install, and is exact enough for pixel
  tests. A GPU path (Metal, Direct3D, Vulkan, WebGPU through each OS's API, our own bindings) is
  an optimization added behind the same `present` boundary when a benchmark shows the CPU path
  limits the 10M-row grid's scrolling (D7); damage rectangles come before it.
- **Text is ours (D30).** D191's TrueType reader over the system's fonts, with no font shipped;
  shaping (kerning, ligatures, complex scripts through our own OpenType `GSUB`/`GPOS` reader),
  bidirectional text and right-to-left mirroring, CFF outlines and hinting are added in the
  toolkit, never through a system or third-party shaper, so every OS draws the same glyph
  positions.
- **The browser draws to a canvas.** The wasm build (`eggdb-wasm`'s target) runs the same
  toolkit and presents the same RGBA frame into a `<canvas>`, with a minimal HTML/JS loader
  (D16's exception) and a DOM of positioned, visually empty ARIA elements over it that carry the
  focus and the screen-reader tree (`first-screen.md`).
- **A retained widget tree, a virtualized grid.** Screens are state plus pure functions: a frame
  is `draw(state, theme, faces, size, scale) -> Canvas` and input is `handle_key(state, key) ->
  (state, action)` (the connect card, `eggdb_gui::connect_screen`), so every frame and every key
  is tested offscreen. The widget tree that holds that state is retained between frames (focus,
  selection, scroll offsets, the accessibility tree need identity), and the result grid draws
  only the visible rows and columns from a cursor-backed page cache, so 10M rows cost what one
  screen costs.
- **The size budget (D6, D34).** The GUI is the separate binary `eggdb-gui` (the crate's own
  `main.rs`), never linked into `eggdb`: the server's size, idle memory and start time do not
  change, and the window code in `eggdb-platform` is unreferenced by the server and removed at
  link time. `eggdb-gui --png <file>` renders a screen offscreen for scripts and CI; with no
  arguments or `--window` it opens the native window. Whether `eggdb gui` later starts this
  binary or a `gui` build profile links it in is decided with the command model (D191's open
  point), keeping the server binary free of it.
- **Themes.** `ThemeKind::{Light, Dark, HighContrast}` with the token columns of
  `first-screen.md`, including the roles it adds (`border_control`, `accent_hover`,
  `accent_pressed`, `focus_ring`, `error`, `warning`); a test checks every text pair at 4.5:1
  (7:1 in high contrast) and every control boundary and the ring at 3:1. `--theme` overrides;
  following the OS's dark and contrast settings comes with each backend.

Accessibility, as built: the screen produces its tree as a pure function of its state
(`connect_screen::accessibility_tree`: stable ids, parents, generic roles, names, values and
states, logical bounds) and hands it to `Window::set_accessibility_tree` after each frame. On
macOS the backend keeps one `NSAccessibilityElement` per node, updated in place while the tree's
shape is unchanged, and posts the focus and value notifications. In the browser, `eggdb-wasm`'s
`gui` feature (off by default, so the database module's size is unchanged) renders the same
card for `web/card.js`, which puts the frame on a canvas and mirrors the tree into real
`<input>`, `<button>` and `<a>` elements with ARIA roles over it, carrying the DOM focus.

Left: Wayland, the clipboard, the UI Automation (Windows) and AT-SPI2 (Linux) bridges, pointer
input in the native windows (hover, pressed, mouse focus without a ring), caret blinking,
remembered window size and position, reading the OS theme, text in the browser without a font
URL, and golden frames with pinned fonts (D191).
