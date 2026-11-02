# The first screen: a native window with the static connect card

Designed by Claude Fable (D15) for issue #29's first slice: one window on macOS, Windows and Linux,
and the same pixels in a browser, showing the connect card of [gui.md](gui.md) ("The connect
screen") with nothing wired behind it yet. The layout, tokens, focus ring, keyboard order and the
accessibility tree are real; opening a file is the next slice. Everything here is drawn by
`eggdb-gui` (D191) through the platform layer (D8); no third-party code (D1).

All values are logical pixels at scale 1. Rects are `(x, y, w, h)`; a text position is the
top-left of its line box. `cx, cy` is the card's top-left; at the default window size it is
`(360, 140)`, so an absolute position is `cx + dx, cy + dy`.

## Tokens: gui.md's, with a third column
This screen reuses gui.md's tokens and the crate's names for them (`Theme { bg0, bg1, bg2,
border, border_strong, text_primary, text_secondary, text_disabled, accent, on_accent, selection,
selection_inactive, success, brand_yolk, … }`, `ThemeKind::{Light, Dark}`). It adds **no second
token set**: it adds the high-contrast column (`ThemeKind::HighContrast`), fills in the fields the
struct does not have yet (`accent_hover`, `accent_pressed`, `focus_ring`, `error`, `warning`), and
adds one token, `borderControl` (`border_control`), the outline of inputs, checkboxes and the
segmented control, because gui.md's `border` is 1.4:1 on `bg0` and a control's boundary needs 3:1
(WCAG 1.4.11). gui.md adopts these three changes in its next revision.

| Role | Light | Dark | High contrast | On this screen |
|------|-------|------|---------------|----------------|
| `bg0` | `#FFFFFF` | `#1E2227` | `#000000` | window background, card, inputs |
| `bg1` | `#F4F5F7` | `#23282E` | `#0A0A0A` | disabled button fill |
| `bg2` | `#E9EBEE` | `#2A3038` | `#141414` | the active segment, hover fill |
| `border` | `#D5D8DD` | `#363D47` | `#FFFFFF` | the card's outline, disabled button border |
| `borderStrong` | `#B8BCC4` | `#4A525E` | `#FFFFFF` | pressed button border |
| `borderControl` | `#6E7781` | `#8B949E` | `#FFFFFF` | input, checkbox and segment outlines |
| `textPrimary` | `#1B1F24` | `#E6E9EE` | `#FFFFFF` | title, labels, input value, button labels |
| `textSecondary` | `#5A6270` | `#A3ABB7` | `#D0D0D0` | placeholder, inactive segment |
| `textDisabled` | `#9AA1AB` | `#6B7380` | `#9E9E9E` | the disabled `Cancel` |
| `accent` | `#1A6FD6` | `#5A9CFF` | `#8AB4F8` | `Open`, checked box, caret, links, egg ring |
| `accentHover` | `#155CB3` | `#7AB0FF` | `#A8C7FA` | hovered `Open` |
| `accentPressed` | `#114A91` | `#4788E8` | `#6A9DF0` | pressed `Open` |
| `onAccent` | `#FFFFFF` | `#0D1117` | `#000000` | `Open`'s label, the check mark |
| `selection` | `#D6E6FA` | `#2D4F7F` | `#1F3A5F` | selected text in the input |
| `selectionInactive` | `#E6E9EE` | `#333B46` | `#333333` | selection in an unfocused input |
| `focusRing` | `#1A6FD6` | `#7AB0FF` | `#FFFFFF` | the focus ring |
| `error` | `#C8322B` | `#F0625A` | `#FF8A80` | the `Error:` line under the segments |
| `brandYolk` | `#F5B700` | `#F5B700` | `#F5B700` | the egg mark's yolk only |
| `success`, `warning`, `zebra`, `syntax*` | gui.md | gui.md | `#7CE38B`, `#FFD75F`, `#0A0A0A`; `#8AB4F8`, `#7CE38B`, `#FFD75F`, `#D0D0D0` | not used here |

Contrast ratios (WCAG 2.x relative luminance), every text pair at least 4.5:1, every
high-contrast pair at least 7:1, every non-text boundary at least 3:1. A test computes these from
the `Theme` values and fails under the limits.

| Pair | Light | Dark | High contrast |
|------|-------|------|---------------|
| `textPrimary` on `bg0` (title, labels, input value) | 16.1:1 | 13.4:1 | 21.0:1 |
| `textPrimary` on `bg2` (the active segment) | 13.9:1 | 10.9:1 | 18.4:1 |
| `textSecondary` on `bg0` (placeholder, inactive segment) | 6.2:1 | 6.9:1 | 13.6:1 |
| `onAccent` on `accent` (`Open`, the check mark) | 4.9:1 | 6.9:1 | 14.9:1 |
| `accent` on `bg0` (links, egg ring, caret) | 4.9:1 | 6.3:1 | 10.0:1 |
| `onAccent` on `accentPressed` | 8.7:1 | 5.4:1 | 7.7:1 |
| `error` on `bg0` | 5.3:1 | 5.0:1 | 9.2:1 |
| `textDisabled` on `bg1` (exempt, listed for honesty) | 2.8:1 | 3.1:1 | 8.3:1 |
| `borderControl` on `bg0` (non-text, 3:1) | 4.6:1 | 5.2:1 | 21.0:1 |
| `focusRing` on `bg0` (non-text, 3:1) | 4.9:1 | 7.2:1 | 21.0:1 |

The theme follows the OS: the dark setting picks dark; Windows "Contrast themes" on, macOS
"Increase contrast" on, GTK's `gtk-theme-name` ending in `HighContrast`, or the browser's
`prefers-contrast: more` picks high contrast (one high-contrast theme, on black, whichever the
OS's own is); `--theme light|dark|high-contrast` and `View > Theme` override. High contrast also
means nothing is said by colour alone: here the checked box has a check mark, the active segment
is bold, links are underlined and the disabled button says so to the screen reader.

Spacing, radii and type are gui.md's: `space2` 4, `space3` 8, `space4` 12, `space5` 16,
`space6` 24; `radiusControl` 4, `radiusCard` 8; `headline` ui bold 20 / 26, `body` ui regular
13 / 18, `bodyBold` ui bold 13 / 18 (`TextStyle { size, line_height, color }`). The `ui` font is
the OS's (gui.md's fallback chains); no font is shipped.

## The window
| Property | Value |
|----------|-------|
| title | `EggDB` |
| default size | 1280 × 800 (gui.md's frame), centred on the screen that has the pointer |
| minimum size | 800 × 560: the card stack is 520 tall and is never clipped (gui.md's 800 × 500 main-window minimum rises to 560 in its next revision) |
| background | `bg0` |
| remembered | size and position per user, restored at the next start, clamped to a visible screen |
| scale `s` | from the OS: `backingScaleFactor` (macOS), `GetDpiForWindow / 96` (Windows), the output's buffer scale (Wayland) or `Xft.dpi / 96` (X11), `devicePixelRatio` (browser); re-read when the window moves between screens |
| close | the window's close button, `Cmd+W` and `Cmd+Q` (macOS), `Alt+F4` and `Ctrl+Q` (Windows, Linux) quit: it is the only window |

The card stack (the card, a 16 px gap, a 24 px links row: 520 tall, 560 wide) is centred in the
container, rounded to whole pixels: `cx = round((W − 560) / 2)`, `cy = round((H − 520) / 2)`. In
this slice the container is the whole window; once the toolbar and sidebar exist the container is
the main area (gui.md's connect screen). At 1280 × 800: card `(360, 140, 560, 480)`, links row
`(360, 636, 560, 24)`. Resizing re-centres at once (no motion).

## The card
`(cx, cy, 560, 480)`: fill `bg0`, 1 px `border` inside the rect, `radiusCard` 8, no shadow (it is
not an overlay). The segment `Open a file` is active; everything below the segments belongs to it.

| Item | Rect / position (relative to `cx, cy`) | Drawing |
|------|------|---------|
| egg mark | ring centred at (32, 37): circle d 16, stroke 2 `accent`; filled circle d 6 `brandYolk` at the same centre | the toolbar's mark (gui.md), 16 × 16 at (24, 29) |
| title | text at (48, 24) | `headline` `textPrimary`: `Connect` (gui.md had the title at x 24; the mark takes that place while this screen has no toolbar) |
| segmented control | (24, 64, 512, 32) | 1 px `borderControl` around, `radiusControl`; two segments of 256 × 32, a 1 px `borderControl` divider at x 280 |
| segment `Open a file` | (24, 64, 256, 32), active | fill `bg2`, label `bodyBold` `textPrimary` centred, line box at y 71 |
| segment `Connect to a server` | (280, 64, 256, 32), inactive | no fill, label `body` `textSecondary` centred; hover `bg2` |
| error line (hidden until a failure) | text at (24, 100) | `body` `error`: `Error: <engine message>`, a second line `Try: <fix>`; the first segment-panel row moves down by 18 per line while shown |
| `File` label | text at (24, 120) | `body` `textPrimary` |
| File input | (24, 142, 420, 28) | input: `bg0`, 1 px `borderControl`, `radiusControl`, padding 0 8; value `~/eggdb/quickstart.eggdb` `body` `textPrimary` at (32, 147); when empty, placeholder `/path/to/shop.eggdb` `textSecondary`; caret 2 px `accent`, 20 tall at y 146, after the value, blinking 530 / 530 ms (solid under reduced motion) |
| `Browse…` | (452, 142, 84, 28) | secondary button: `bg0` / 1 px `border` / `textPrimary`, label centred (padding 12), `radiusControl` |
| checkbox `Create the file if it does not exist` | box (24, 186, 16, 16); label text at (48, 185) | box `radiusControl`, 1 px `borderControl`; **checked**: fill `accent`, check icon two 1.5 px `onAccent` lines (4, 8)–(7, 11) and (7, 11)–(12, 5) inside the box; label `body` `textPrimary`; the whole row (24, 182, 512, 24) is the hit area |
| checkbox `Open read-only` | box (24, 210, 16, 16); label text at (48, 209) | unchecked: `bg0` fill, 1 px `borderControl`; row (24, 206, 512, 24) |
| `Cancel` | (364, 436, 84, 28) | secondary, **disabled** at first start (nothing to return to): `bg1` / 1 px `border` / `textDisabled`, label centred |
| `Open` | (456, 436, 80, 28) | primary: `accent` / no border / `onAccent`, label `Open` `body` centred; hover `accentHover`, pressed `accentPressed`; the default button: `Enter` activates it from the input, the checkboxes and the segments |

Below the card, the links row `(cx, cy + 496, 560, 24)`, text line boxes at y `cy + 499`:

| Link | Position | Drawing |
|------|----------|---------|
| `Where are you coming from?` | text at (24, 499) | `body` `accent`, 1 px `accent` underline 2 px under the baseline, always (never a colour-only link); hover `accentHover` text and underline; opens the layout picker of gui-layouts.md (a later slice) |
| `Load a sample dataset` | text 24 px after the first link's right edge | same style; loads the `shop` sample (D42, a later slice) |

Hit sizes: every control is at least 24 px tall (the 16 px boxes through their rows, the links
through the 24 px row). Hover fills are `bg2` on the secondary button and the inactive segment,
`borderStrong` on the input's border, with gui.md's 80 ms linear fade (instant under reduced
motion). Nothing else moves on this screen.

In the browser the container is the viewport; narrower than 608 px, the card is `viewport − 32`
wide with 16 px margins, the segments share the width equally, the input is `card − 48 − 8 − 84`
wide, `Browse…` keeps 84 and stays at the right content edge, and the buttons keep their sizes
right-aligned; the two links wrap to two rows (the row grows to 48).

## The focus ring
One ring, drawn after everything else so nothing covers it: colour `focusRing`; width 2 px
(light, dark) or 3 px (high contrast); a 2 px gap between the control's outer edge and the ring's
inner edge in every theme (so the ring occupies 2–4 px outside the control, 2–5 px in high
contrast); the ring's inner corner radius is the control's radius + 2 (`radiusControl` 4 → 6; the
check boxes 4 → 6). Rings never appear from mouse clicks on buttons and links (the pressed fill
shows the click); they always appear from the keyboard, and on the input in both cases together
with its `accent` border. What the ring surrounds:

| Focused element | Ring's inner edge |
|-----------------|-------------------|
| a segment | that segment's rect, radius 4 (only the active segment is focusable) |
| the File input | the input rect; the input's own border also turns `accent` |
| `Browse…`, `Cancel`, `Open` | the button rect |
| a checkbox | the 16 × 16 box |
| a link | the text's line box widened by 4 on each side, radius 4 |

## Keyboard
Initial focus: the File input, with the whole value selected (`selection` behind the glyphs) so
typing replaces it and `Enter` opens the default at once. `Tab` order, wrapping at both ends
(desktop; in the browser `Tab` leaves the page at the ends, as on any page):

1. the segmented control (one stop: the active segment); `Left` / `Right` switch segments and
   change the panel below at once; `Home` / `End` go to the first / last segment
2. the File input: a plain single-line editor (gui.md's input rules; `Cmd/Ctrl+A` selects all;
   `Cmd/Ctrl+C`, `X`, `V` use the OS clipboard)
3. `Browse…`
4. `Create the file if it does not exist` (`Space` toggles)
5. `Open read-only` (`Space` toggles)
6. `Open` (`Cancel` is skipped while disabled)
7. `Where are you coming from?`
8. `Load a sample dataset`

| Key | Does |
|-----|------|
| `Tab` / `Shift+Tab` | next / previous stop in the order above |
| `Enter` | on a button or link: activates it; anywhere else in the card: `Open` (the default button) |
| `Space` | on a button or link: activates it; on a checkbox: toggles it; in the input: a space |
| `Esc` | `Cancel`; at first start `Cancel` is disabled, so `Esc` only clears a shown `Error:` line and the input's selection |
| `Cmd+O` (macOS), `Ctrl+O` (Windows, Linux) | focuses the File input (gui.md's Connect key) |
| `Cmd+Q`, `Cmd+W` (macOS); `Ctrl+Q`, `Alt+F4` (Windows, Linux) | quits; the browser keeps its own `Ctrl/Cmd+W` and `Q` |
| `Cmd+K` / `Ctrl+K`, `F6`, `Cmd+1`… | nothing yet: the palette and the regions come with the main window |

In this slice `Open`, `Browse…` and the links are drawn and focusable but do nothing when
activated (a no-op, not an error); the checkboxes and the input do work, and the segmented
control switches the panel to gui.md's server fields when they are drawn, otherwise stays on
`Open a file` and keeps its focus.

## What the screen reader hears
The toolkit keeps one accessibility tree per window (nodes with a role, name, value, states, a
rect in logical pixels and children), updated whenever the frame changes, and exposes it through
the platform layer: NSAccessibility (macOS), UI Automation (Windows), AT-SPI2 (Linux) and, in the
browser, a DOM of positioned, visually empty elements with ARIA attributes over the canvas, one
per node, which also carry the keyboard focus so `Tab` and the screen reader agree. Names and
values are the visible strings, never a description of the drawing.

| Element | Role (generic → macOS / UIA / AT-SPI / ARIA) | Name | Value / states |
|---------|---------------------------------------------|------|----------------|
| window | window → AXWindow / Window / frame / (document) | `EggDB` | main, focused |
| card | group → AXGroup / Group / panel / `group` | `Connect` (labelled by the title) | |
| egg mark | none: decorative, hidden from the tree | | |
| title | heading → AXStaticText (`AXHeading`) / Text / heading / `heading` level 1 | `Connect` | |
| segmented control | tab list → AXTabGroup / Tab / page tab list / `tablist` | `Connection kind` | |
| each segment | tab → AXRadioButton (in the tab group) / TabItem / page tab / `tab` | `Open a file`, `Connect to a server` | selected (the active one), `1 of 2` |
| the segment's panel | group → AXGroup / Pane / panel / `tabpanel` | `Open a file` | controlled by its tab |
| error line | text, live → AXStaticText / Text (LiveSetting Polite) / label / `status`, `aria-live=polite` | the whole text | announced when it appears |
| File input | text field → AXTextField / Edit / entry / `textbox` | `File` (labelled by the label) | value `~/eggdb/quickstart.eggdb`, placeholder `/path/to/shop.eggdb`, focused, editable; selection start and end |
| `Browse…` | button → AXButton / Button / push button / `button` | `Browse` (the ellipsis is dropped from the name) | |
| checkbox 1 | checkbox → AXCheckBox / CheckBox / check box / `checkbox` | `Create the file if it does not exist` | checked |
| checkbox 2 | checkbox | `Open read-only` | not checked |
| `Cancel` | button | `Cancel` | disabled (not focusable while disabled, still in the tree) |
| `Open` | button | `Open` | default button |
| links | link → AXLink / Hyperlink / link / `link` (`<a>`) | the link's text | |

Focus changes raise the platform's focus event; toggling a checkbox raises a value (state) change;
the error line's appearance is announced politely without moving focus. Nothing has a tooltip or
description the screen does not show.

## At 2× (and any scale)
The logical layout above never changes with the scale. The framebuffer is `W × s` by `H × s`
device pixels and every rect, radius, stroke and font size is multiplied by `s` before
rasterising; glyphs are rasterised at `13 × s` px (not scaled bitmaps), with their origins rounded
to whole device pixels. One-pixel lines are `round(s)` device pixels wide, so at 1.25 a line stays
1 px, at 1.5 and 2 it is 2 px, at 3 it is 3 px; at a fractional scale the rect's edges are
multiplied and rounded, so no edge is blurred across two pixels. At 2× on a 1280 × 800 window:
the framebuffer is 2560 × 1600, the card is `(720, 280, 1120, 960)` device px with a 2 px border
and 16 px corners, the input `(768, 564, 840, 56)` with a 2 px `borderControl` line, the focus
ring 4 px wide (6 in high contrast) 4 px outside the control, the caret 4 px wide, the check mark
3 px strokes, the title set at 40 px on a 52 px line and body text at 26 px on 36 px lines, the
egg ring 32 px across with a 4 px stroke and a 12 px yolk. The accessibility tree reports logical
pixels; the platform layer converts for the OS where it wants device pixels (UIA).

## Tests this slice ships with
- Golden frames at 1280 × 800 with the box typeface (D191): light, dark and high contrast, each
  with the File input focused (ring, `accent` border, selection, solid caret), plus light with
  `Open` focused and light with checkbox 1 focused; a 2× frame (2560 × 1600, light) checks the
  scale rules. With the system's fonts the same frames are written to `target/gui/connect_*.png`
  for people to look at.
- A contrast test over the three `Theme` values, with the limits above.
- A focus-order test: `Tab` × 8 and `Shift+Tab` × 8 from the initial focus visit the stops in
  order and wrap; `Space` toggles both checkboxes and nothing else; `Enter` on a checkbox reaches
  `Open`.
- An accessibility-tree test: the tree built from the frame has exactly the nodes in the table,
  with those names, roles and states, in that order.
- On each desktop OS in CI: the window opens, reports its logical size and scale, gets focus and
  closes on the quit key, headless where the OS allows (Xvfb on Linux); the browser build renders
  the canvas and its ARIA DOM in a headless browser and the same tree test runs over the DOM.

## Left for the next slices
Opening a file, the server segment and the `Error:` line from a real failure are built (D274,
`docs/ux/workspace.md` 1). Left: the
layout picker behind `Where are you coming from?`, the sample dataset, recent connections, the
toolbar and sidebar around the card, right-to-left mirroring, and the `warning` token's light value
(gui.md's `#B26A00` is 4.2:1 on `bg0`; `#995A00`, 5.5:1, is proposed for its next revision).
