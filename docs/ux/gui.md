# The EggDB GUI client: desktop design (M6e, first screens)

Designed by Claude Fable (D15, roadmap M6e). The SQL client people love, drawn by our own toolkit
(D1): a software rasterizer into an RGBA framebuffer (filled and rounded rectangles, 1 px lines,
circles, alpha blending), text from the OS's installed TrueType fonts, left-to-right only. No image
or icon files: every icon is geometry. It is the same product as `eggdb ui` ([ui.md](ui.md)): the
same words (`Keys`, `Result`, `Committed`, `Error:`), nothing said by colour alone, windowed grids
that stay fast on a billion rows. All values are logical pixels at scale 1 (a window at scale `s`
multiplies them and draws 1 px lines `round(s)` px wide). The first milestone renders the main
window offscreen at 1280 × 800 for golden-image tests, then opens real windows on macOS, Windows
and Linux.

## Principles
1. **The query and its result are the page**; tree, toolbar and status are a margin: one quiet
   grey, one accent, no decoration.
2. **Text first.** Every state is a string in a fixed place (`3 rows · 0.4 ms`, `Ln 1, Col 44`,
   `Connected`), never only a colour or a shape.
3. **Regions, not pixels.** Six named regions (`toolbar`, `sidebar`, `tabs`, `editor`, `results`,
   `statusbar`) in a tree of splits; EggDB's layout is one arrangement, the familiar layouts (D15)
   are others, so nothing assumes where a region sits.
4. **Keyboard complete, mouse welcome.** The mouse never has an action the keyboard lacks.
5. **Calm motion.** Nothing moves unless it tells the user something, and then under 150 ms.

## Design tokens

### Colours
Code uses roles (`Theme::bg0`), never literals. The OS setting picks the theme; `View > Theme` overrides.

| Role | Light | Dark | Used for |
|------|-------|------|----------|
| `bg0` | `#FFFFFF` | `#1E2227` | editor, grid body, inputs, active tab, cards |
| `bg1` | `#F4F5F7` | `#23282E` | sidebar, results header, grid header, current editor line |
| `bg2` | `#E9EBEE` | `#2A3038` | toolbar, tab strip, status bar, hover fill |
| `zebra` | `#F8F9FA` | `#21262C` | every second grid row |
| `border` | `#D5D8DD` | `#363D47` | all 1 px lines, splitter lines |
| `borderStrong` | `#B8BCC4` | `#4A525E` | pressed button border, column separators |
| `textPrimary` | `#1B1F24` | `#E6E9EE` | labels, cell text, SQL identifiers |
| `textSecondary` | `#5A6270` | `#A3ABB7` | line numbers, counts, status items, placeholders |
| `textDisabled` | `#9AA1AB` | `#6B7380` | disabled labels, `NULL` |
| `accent` | `#1A6FD6` | `#5A9CFF` | primary button, active tab stripe, caret, links |
| `accentHover` | `#155CB3` | `#7AB0FF` | hovered primary button |
| `accentPressed` | `#114A91` | `#4788E8` | pressed primary button |
| `onAccent` | `#FFFFFF` | `#0D1117` | text on `accent` |
| `selection` | `#D6E6FA` | `#2D4F7F` | selected row/cell/text in the focused region |
| `selectionInactive` | `#E6E9EE` | `#333B46` | selection in an unfocused region |
| `focusRing` | `#1A6FD6` | `#7AB0FF` | 2 px ring around the focused control |
| `success` | `#1E8E3E` | `#3FB950` | `Committed`, connected dot |
| `warning` | `#B26A00` | `#E0A030` | `Warning:` lines |
| `error` | `#C8322B` | `#F0625A` | `Error:` lines, error underline |
| `brandYolk` | `#F5B700` | `#F5B700` | the egg mark's yolk only |
| `syntaxKeyword` | `#0550AE` | `#79B8FF` | SQL keywords |
| `syntaxString` | `#0A7B3E` | `#7EE787` | string literals |
| `syntaxNumber` | `#B35C00` | `#FFA657` | numeric literals |
| `syntaxComment` | `#6E7781` | `#8B949E` | `--` and `/* */` comments |

Contrast (light): `textPrimary` on `bg0` 16.1:1, `textSecondary` on `bg1` 5.5:1, `onAccent` on
`accent` 4.9:1, `accent` on `bg0` 4.9:1, `textDisabled` on `bg0` 2.8:1 (disabled text is exempt).
Dark: `textPrimary` on `bg0` 13.4:1, `textSecondary` on `bg1` 6.6:1, `accent` on `bg0` 6.3:1.

### Spacing, radii, lines
| Token | Value | Token | Value |
|-------|-------|-------|-------|
| `space1` | 2 | `radiusNone` | 0 (grid cells, splitters, status bar) |
| `space2` | 4 | `radiusControl` | 4 (buttons, inputs, tabs, tree selection) |
| `space3` | 8 | `radiusPopover` | 6 (menus, tooltips, toasts) |
| `space4` | 12 | `radiusCard` | 8 (connect card, dialogs) |
| `space5` | 16 | `radiusPill` | 999 (badges, scrollbar thumb) |
| `space6` | 24 | `line` | 1 px, `border` |
| `space7` | 32 | `focusRingWidth` | 2 px, drawn 2 px outside the control, radius + 2 |

### Type
Two OS families, `ui` (sans) and `mono`, regular and bold only (no italic, no synthetic styles).
Baseline = `top + round((lineHeight − (ascent + descent)) / 2 + ascent)` from the hhea metrics;
advances accumulate in `f32` and each glyph origin is rounded to a whole pixel.

| Style | Family | Size / line | Use |
|-------|--------|-------------|-----|
| `headline` | ui bold | 20 / 26 | connect screen title |
| `title` | ui bold | 15 / 20 | dialog titles, sidebar connection name |
| `body` | ui regular | 13 / 18 | buttons, tabs, tree, grid header, labels |
| `bodyBold` | ui bold | 13 / 18 | connection name in the toolbar, `Results` when active |
| `small` | ui regular | 11 / 14 | status bar, counts, badges |
| `code` | mono regular | 13 / 20 | editor text, line numbers, grid cells |

Font fallback chains, first file found wins; `.ttc` collections are read by face name. Searched
(bounded to these roots): macOS `/System/Library/Fonts[/Supplemental]`, `/Library/Fonts`,
`~/Library/Fonts`; Windows `C:\Windows\Fonts`, `%LOCALAPPDATA%\Microsoft\Windows\Fonts`; Linux
`/usr/share/fonts`, `/usr/local/share/fonts`, `~/.local/share/fonts`, `~/.fonts`.

| OS | `ui` | `mono` |
|----|------|--------|
| macOS | Helvetica Neue, Helvetica, Arial | Menlo, Monaco, Courier New |
| Windows | Segoe UI (`segoeui.ttf`, `segoeuib.ttf`), Arial, Tahoma | Cascadia Mono, Consolas (`consola.ttf`), Courier New (`cour.ttf`) |
| Linux | Noto Sans, DejaVu Sans, Liberation Sans, Cantarell, Ubuntu | Noto Sans Mono, DejaVu Sans Mono, Liberation Mono, Ubuntu Mono |

When no chain resolves, the window draws `No TrueType font found. Install one of: <chain>` with a
built-in 5 × 7 ASCII glyph set made of 2 px rectangles (not a font file). The golden test uses
pinned copies of DejaVu Sans and DejaVu Sans Mono in `crates/eggdb-gui/tests/fonts/` (Bitstream
Vera licence, tests only, confirmed by a research subagent under D1), so the image is identical on
every OS; DejaVu Sans Mono at 13 px advances 7.83 px per glyph.

## The main window at 1280 × 800 (the golden image)
Database `shop` in `shop.eggdb` (tables `customers`, `orders`, `products`) after running the
editor's statement, light theme. Rectangles are `(x, y, w, h)`; a text position is the top-left of
its line box unless `right-aligned at x` (the text's right edge) is stated.

| Region | Rect | Fill | Border |
|--------|------|------|--------|
| `toolbar` | (0, 0, 1280, 44) | `bg2` | 1 px `border` at y 43 |
| `sidebar` | (0, 44, 260, 732) | `bg1` | 1 px `border` at x 259 |
| sidebar splitter (hit) | (256, 44, 6, 732) | none | line is the sidebar's border |
| `tabs` | (260, 44, 1020, 36) | `bg2` | 1 px `border` at y 79 |
| `editor` | (260, 80, 1020, 300) | `bg0` | none |
| editor/results splitter | (260, 380, 1020, 6) | `bg2` | 1 px `border` at y 382 |
| `results` | (260, 386, 1020, 390) | `bg0` | header below |
| `statusbar` | (0, 776, 1280, 24) | `bg2` | 1 px `border` at y 776 |

### Toolbar contents
| Item | Rect | Drawing |
|------|------|---------|
| egg mark | (12, 14, 16, 16) | circle d 16 stroke 2 `accent`; filled circle d 6 `brandYolk` centred at (20, 23) |
| connection name | text at (36, 13) | `bodyBold` `textPrimary`: `shop` |
| connection file | text at (72, 13) | `body` `textSecondary`: `shop.eggdb` |
| divider | (220, 12, 1, 20) | `border` |
| Run | (232, 8, 72, 28) | primary button; icon: triangle of three 1.5 px lines `onAccent` with corners (244, 17) (244, 27) (253, 22); label `Run` at (260, 13) |
| Run all | (312, 8, 88, 28) | secondary button; same triangle in `textPrimary` at x 324 plus a rect (335, 17, 2, 10); label `Run all` at (342, 13) |
| Explain | (408, 8, 84, 28) | secondary; icon three rects (420, 17, 10, 2) (424, 21, 6, 2) (428, 25, 2, 2) joined by a 1 px line at x 421 from y 19 to 26; label `Explain` at (436, 13) |
| Stop | (500, 8, 68, 28) | secondary, **disabled**; icon rounded rect (512, 17, 10, 10) r 2 `textDisabled`; label `Stop` at (528, 13) `textDisabled` |
| dialect | (1056, 8, 120, 28) | secondary (dropdown); label `PostgreSQL` at (1068, 13); chevron-down 8 × 4 at (1156, 20) |
| more | (1240, 8, 28, 28) | ghost; three filled circles d 3 `textPrimary` centred at (1249, 22) (1254, 22) (1259, 22) |

### Sidebar contents
| Item | Rect | Drawing |
|------|------|---------|
| filter field | (8, 52, 244, 28) | input; search icon: circle d 8 stroke 1.5 at (16, 60), line (23, 67)–(26, 70); placeholder `Filter objects` `textSecondary` at (32, 57) |
| tree | (0, 88, 260, 688) | rows of 24 px, indent 16 per level, chevron slot 16 wide at `8 + 16 × level`, icon 12 × 12 at chevron slot + 16, label at chevron slot + 36 |

Tree rows in the first frame (y is the row top; label text is at `y + 3`):

| y | Level | Chevron | Icon | Label | Right text (right-aligned at 244, `small` `textSecondary`) |
|---|-------|---------|------|-------|------|
| 88 | 0 | down at (8, 96) | database at (24, 94) | `shop` (`bodyBold`) | `read-write` |
| 112 | 1 | down at (24, 120) | folder at (40, 118) | `Tables` | `3` |
| 136 | 2 | none | table at (56, 142) | `customers` **selected** | |
| 160 | 2 | none | table at (56, 166) | `orders` | |
| 184 | 2 | none | table at (56, 190) | `products` | |
| 208 | 1 | right at (24, 216) | folder at (40, 214) | `Views` | `0` |
| 232 | 1 | right at (24, 240) | folder at (40, 238) | `Indexes` | `3` |
| 256 | 1 | right at (24, 264) | folder at (40, 262) | `Keys` | |

The selected row is a rounded rect (4, 136, 252, 24) r 4 `selectionInactive` (the tree is not
focused). Icons, 1 px `textSecondary` strokes: **database** rounded rects (0, 0, 12, 5) and
(0, 7, 12, 5) r 2; **folder** rounded rect (0, 2, 12, 9) r 2 plus line (0, 5)–(12, 5); **table**
rect (0, 0, 12, 10), lines (0, 3)–(12, 3) and (4, 3)–(4, 10). Chevrons, 1.5 px: **right** lines
(0, 0)–(4, 4) and (4, 4)–(0, 8); **down** lines (0, 0)–(4, 4) and (4, 4)–(8, 0).

### Tabs
Active tab (260, 44, 140, 36): fill `bg0`, stripe (260, 44, 140, 2) `accent`, no bottom border
(it joins the editor); label `Query 1` `body` `textPrimary` at (272, 53); close icon two 1.5 px
lines `textSecondary` across (380, 58, 8, 8) (hit 24 × 24 centred). New-tab button (408, 48, 28, 28)
ghost, plus icon: lines (417, 62)–(427, 62) and (422, 57)–(422, 67), 1.5 px.

### Editor
Gutter (260, 80, 48, 300) `bg0`; line number `1` `code` `textSecondary` right-aligned at 296, y 88.
Current-line highlight (308, 88, 972, 20) `bg1`. Text origin (320, 88), one line, `code`:
`SELECT id, name FROM customers ORDER BY id;` with `SELECT`, `FROM`, `ORDER`, `BY` in
`syntaxKeyword` and everything else `textPrimary`. Caret: rect (657, 88, 2, 20) `accent`, after
column 43 (`round(320 + 43 × 7.83)`), drawn solid in the golden frame. No scrollbars. The editor
has keyboard focus, shown by the caret and the `bg1` line; regions never get a focus ring.

### Results
| Item | Rect | Drawing |
|------|------|---------|
| results header | (260, 386, 1020, 32) | `bg1`, 1 px `border` at y 417 |
| `Results` segment | text at (272, 393) | `bodyBold` `textPrimary`; stripe (272, 414, 48, 2) `accent` |
| `Messages` segment | text at (344, 393) | `body` `textSecondary` |
| summary | text at (428, 393) | `body` `textSecondary`: `3 rows · 0.4 ms` |
| Export… | (1192, 388, 76, 28) | ghost button, label `Export…` at (1204, 393) |
| grid header row | (260, 418, 1020, 28) | `bg1`, 1 px `border` at y 445 |
| rows 1–3, then empty | (260, 446 + 24 × (n − 1), 1020, 24); (260, 518, 1020, 258) | `bg0`, `zebra`, `bg0`; `bg0` |

Columns: row gutter (260–308, w 48), `id` (308–388, w 80, integer → right-aligned at 376),
`name` (388–628, w 240, text → left at 400). Header texts `body` `textPrimary`: `id` right-aligned
at 376, `name` at 400, both at y 423. Column separators 1 px `borderStrong` at x 307, 387, 627, in
the header only; the gutter's right edge x 307 continues through the body in `border`. Cell text is
`code`, line box at `y + 2`:

| Row | Gutter (right at 296, `textSecondary`) | `id` (right at 376) | `name` (left at 400) |
|-----|------|------|------|
| 1 (y 448) | `1` | `1` | `Ada Lovelace` |
| 2 (y 472) | `2` | `2` | `Grace Hopper` |
| 3 (y 496) | `3` | `3` | `Alan Turing` |

### Status bar
`small` text, line box at y 781. Left: a filled circle d 8 `success` at (12, 784);
`Connected · shop.eggdb · read-write` at (26, 781); `SELECT 3 · 0.4 ms` at (260, 781). Right,
right-aligned at 1268, 16 px apart, right to left: `PostgreSQL`, `Ln 1, Col 44`, `UTF-8`.

### Golden test contract
`crates/eggdb-gui/tests/golden/main_light.png` and `main_dark.png` are this frame with the pinned
fonts; a test renders the model offscreen and compares bytes, failing with the first differing
`(x, y)` and both colours and writing `*.actual.png` beside the expected file. Window minimum
800 × 500; below it the sidebar collapses to a 36 px icon rail.

## Components and states (fill / 1 px border / text colour)

### Buttons (height 28, padding 0 12, `radiusControl`, `body`, icon 10–12 px then 8 px gap)
| Kind | Normal | Hover | Pressed | Focused | Disabled |
|------|--------|-------|---------|---------|----------|
| primary | `accent` / none / `onAccent` | `accentHover` | `accentPressed` | + `focusRing` | `bg2` / none / `textDisabled` |
| secondary | `bg0` / `border` / `textPrimary` | `bg2` | `bg2` / `borderStrong` | + `focusRing` | `bg1` / `border` / `textDisabled` |
| ghost | none / none / `textPrimary` | `bg2` | `bg2` / `borderStrong` | + `focusRing` | `textDisabled` |
A dropdown is a secondary button with a chevron-down (8 × 4) 12 px from its right edge; minimum width 28.

### Tabs (height 36, min width 100, max 240, label padding 12, close hit 24 × 24)
Active: `bg0`, 2 px `accent` stripe on top, `textPrimary`. Inactive: `bg2`, `textSecondary`, hover
`bg1`. Unsaved: a filled circle d 6 `textSecondary` in the close icon's place until hovered.
Focused (keyboard): `focusRing` inset 2 px. A strip wider than its region scrolls with the wheel.

### Tree rows (height 24, indent 16 per level)
Normal: no fill. Hover: `bg2` rounded rect inset 4 px horizontally. Selected: `selection` when the
tree is focused, `selectionInactive` otherwise. Focused row (keyboard): 2 px `focusRing` inset 1 px.
Disabled (a failed connection): `textDisabled` label, error dot d 8 and the word `error` in the right
text slot. The chevron's hit area is its 24 × 24 slot; typing letters jumps to the next matching label.

### Grid
Header row 28, body rows 24, cell padding 0 12, `code` text, every second row `zebra`, columns never
wrap, overflow cut with `…`. Alignment by type: integers, decimals, floats and money right; text,
dates, booleans (`true`/`false`), JSON and blobs left. `NULL` is the word `NULL` in `textDisabled`;
an empty string is nothing; a blob is `0x` + its first 16 bytes in hex + `…` in `textSecondary`.
Selected cell: `selection` plus a 2 px `accent` rect; a selected row (gutter click, `Shift+Space`)
fills with `selection`, `selectionInactive` when unfocused; hover `bg2`. Column width: the wider of
the header and the first 200 rows, clamped 48–480; a header separator drags (hit 6 px), double-click
resets. Rows are a window over the result, as in `eggdb ui`: only visible rows are formatted. The
summary is `<n> rows · <time>` (`1 row`, `0 rows`, `s` from 1,000 ms) or `Error`.

### Text input and the editor
Input: height 28, `bg0`, 1 px `border`, `radiusControl`, padding 0 8, `body`; placeholder
`textSecondary`; hover `borderStrong`; focused `accent` border plus `focusRing`; disabled `bg1` and
`textDisabled`; invalid `error` border plus an `Error:` line below in `small` `error`. Caret: 2 px
`accent`, 530 ms on / off, solid for 500 ms after a key. Selection: `selection` behind the glyphs,
`selectionInactive` when unfocused. Editor: `code` at 20 px lines, gutter 48 px with numbers
right-aligned 12 px from its right edge (`textSecondary`, the caret's line `textPrimary`), text 12 px
after the gutter, current line `bg1`, no wrapping, `Tab` inserts four spaces, brackets auto-close,
unlimited undo per tab. Syntax: the dialect's keywords `syntaxKeyword`, strings `syntaxString`,
numbers `syntaxNumber`, comments `syntaxComment`, everything else `textPrimary`. A run's error
underlines its range with a 2 px `error` rect and moves the caret there; `Messages` shows the
engine's text (`ERROR:`, `LINE n:`, `^`, `SQLSTATE:` as in `eggdb sql`) in `code`.

### Scrollbars and splitters
Scrollbars are overlays: 10 px track at the region's edge, thumb 6 px inset 2 px, `radiusPill`,
`textDisabled` at 60 % alpha, at least 24 px long; hover or drag `textSecondary`, 8 px wide; shown
on scroll or hover, fading 800 ms after the last movement. Wheel: 3 rows per notch in grid and
tree, 60 px in the editor; `Shift` + wheel scrolls horizontally. Splitters: a 6 px hit band with a
1 px `border` line in the middle; hover and drag 2 px `accent`, resize cursor; focusable
(`focusRing` around the band, arrows 8 px, `Shift` + arrows 32 px, `Enter` and double-click
reset). Limits: sidebar 180–480 (default 260); editor at least 80, results at least 120.

### Status bar items
`small` text, 16 px apart, each an invisible button (hover: `bg1` pill, 6 px horizontal padding).
Left: connection dot and text, then the last run's command tags (`CREATE TABLE · INSERT 0 2 ·
SELECT 2`). Right: encoding, `Ln l, Col c`, dialect (click opens the dialect menu). A toast
replaces the left group for 8 s or until a key: `Committed`, `Warning:` or `Error:` first, in
`success` / `warning` / `error`, cut with `…`, clickable to open the full text in a dialog.

### Icons
All icons are 1.5 px strokes in their control's text colour inside a 12 × 12 box, from lines, rects
and circles only; the ones not drawn above: key (circle d 6 + a 6 px line + a 2 px tooth), check
(two lines), warning (a triangle of three lines + a 2 px dot), error (circle + ×).

## The connect screen
Shown in the `editor` + `results` area when a tab has no connection (the whole main area at first
start); the sidebar lists recent connections. One card, `bg0`, `radiusCard`, 1 px `border`,
centred in the main area: (490, 140, 560, 480).

| Item | Rect / position | Content |
|------|------|---------|
| title | text at (514, 164) | `headline`: `Connect` |
| segments | (514, 204, 512, 32) | two equal segments, `Open a file` (active: `bg2` fill, `textPrimary`) and `Connect to a server` (`textSecondary`), 1 px `border` around, `radiusControl` |
| **Open a file** | | |
| File | label (514, 260); input (514, 282, 420, 28) | placeholder `/path/to/shop.eggdb`; `Browse…` secondary button (942, 282, 84, 28) |
| checkboxes | (514, 326) and (514, 350) | 16 × 16 box, `radiusControl`, checked = `accent` fill + check icon `onAccent`; labels at x 538: `Create the file if it does not exist`, `Open read-only` |
| **Connect to a server** | | |
| Protocol | label (514, 260); dropdown (514, 282, 512, 28) | `PostgreSQL protocol`, `MySQL protocol`, `SQL Server (TDS)`, `Redis (RESP)`; names are trademarks, shown as "compatible with" in the tooltip |
| Host, Port | labels (514, 326) (894, 326); inputs (514, 348, 372, 28) (894, 348, 132, 28) | Port prefilled per protocol (5432, 3306, 1433, 6379) |
| Database | label (514, 392); input (514, 414, 512, 28) | |
| User, Password | labels (514, 458) (776, 458); inputs (514, 480, 250, 28) (776, 480, 250, 28) | password masked with filled circles d 6, `Show` ghost button inside the right edge |
| TLS checkbox | (514, 546) | `Use TLS` (checked by default; unchecked shows `Warning: the password travels unencrypted` under it) |
| buttons | `Cancel` (854, 576, 84, 28) secondary; `Connect` (946, 576, 80, 28) primary (label `Open` on the file segment) | `Enter` connects, `Esc` cancels |

A failed connection keeps the card and shows the engine's message under the segments as an
`Error:` line in `body` `error`, with its `Try:` line. Recent connections in the sidebar are tree
rows (`shop.eggdb`, right text `~/data`); `Enter` or double-click connects, `Delete` forgets.

## Keyboard shortcuts (EggDB layout)
Familiar layouts (D15) replace this table with their own.

| Action | macOS | Windows / Linux |
|--------|-------|-----------------|
| Run the statement under the caret (or the selection) | `Cmd+Enter`, `F5` | `Ctrl+Enter`, `F5` |
| Run the whole tab | `Cmd+Shift+Enter` | `Ctrl+Shift+Enter` |
| Explain the statement | `Cmd+E` | `Ctrl+E` |
| Stop the running statement | `Cmd+.`, `Esc` in the editor | `Ctrl+Break`, `Esc` in the editor |
| New tab / close tab | `Cmd+T` / `Cmd+W` | `Ctrl+T` / `Ctrl+W`, `Ctrl+F4` |
| Next / previous tab | `Ctrl+Tab` / `Ctrl+Shift+Tab` | same |
| Focus the tree / editor / grid | `Cmd+1` / `Cmd+2` / `Cmd+3` | `Ctrl+1` / `Ctrl+2` / `Ctrl+3` |
| Cycle regions | `Ctrl+F6` / `Ctrl+Shift+F6` | `F6` / `Shift+F6` |
| Find / find next / previous / replace | `Cmd+F` / `Cmd+G` / `Cmd+Shift+G` / `Cmd+Option+F` | `Ctrl+F` / `F3` / `Shift+F3` / `Ctrl+H` |
| Connect (file or server) | `Cmd+O` | `Ctrl+O` |
| Toggle the sidebar / the results panel | `Cmd+B` / `Cmd+J` | `Ctrl+B` / `Ctrl+J` |
| Comment lines / format the statement | `Cmd+/` / `Cmd+Shift+F` | `Ctrl+/` / `Ctrl+Shift+F` |
| Copy selected cells (TSV) | `Cmd+C` | `Ctrl+C` |
| Command palette (every action by name) / help | `Cmd+K` / `Cmd+?` | `Ctrl+K` / `F1` |
Grid: arrows move the cell, `PgUp`/`PgDn` a page, `Home`/`End` the row's ends, `Enter` opens the
cell's full value, `Shift+Space` selects the row. Tree: `→`/`←` expand and collapse, `Enter` opens
a `SELECT * FROM <table> LIMIT 100;` tab, `Shift+Enter` inserts the name at the caret.

## Motion
| What | Duration | Easing | Reduced motion |
|------|----------|--------|----------------|
| hover and pressed fills | 80 ms | linear | instant |
| toast enters (slides 8 px up, fades in) and leaves | 120 ms / 100 ms | ease-out `cubic-bezier(0.2, 0, 0, 1)` / ease-in | instant |
| scrollbar thumb fade | 200 ms after 800 ms idle | linear | always visible |
| running indicator: a 2 px `accent` bar, 120 px wide, sweeping the toolbar's bottom edge | 1,000 ms per sweep, repeat | linear | a static full-width 2 px `accent` bar plus `Running…` in the status bar |
| tree expand / collapse, tab switch, splitter drag, focus ring | none | | |
| caret blink | 530 / 530 ms | | solid |
Reduced motion follows the OS setting (macOS Reduce Motion, Windows "Show animations", GTK's `gtk-enable-animations`), `--reduced-motion`, or `View > Reduce motion`.

## Accessibility
- Contrast: text at least 4.5:1, icons, control borders and the focus ring at least 3:1, in both
  themes; a test checks the token table's figures. Text follows the OS font size and window scale.
- Focus is always visible (`focusRing`, the caret and `bg1` line, the `accent` cell rect,
  `selection` in the tree) and the window scrolls it into view.
- Hit sizes at least 24 × 24: toolbar buttons 28 tall, tree and grid rows 24, the 8 px close glyph in
  a 24 × 24 area, splitters 6 px wide with keyboard handles.
- Keyboard only: every action is in the shortcut table or the palette; `Tab` order toolbar →
  sidebar → tabs → editor → results → status bar; dialogs trap `Tab` and close on `Esc`.
- Nothing by colour alone: `Error:`/`Warning:`/`Committed` words, the `NULL` word, text beside the
  connection dot, stripe plus bold label on the active tab.
- Screen readers: the accessibility tree (names, roles, values, the status strings) is the platform
  layer's next step after real windows; until then every state is text on screen.

## Later (one line each)
- Familiar layouts (D15), each a `docs/ux/layouts/<name>.md` arranging the same regions with its shortcuts.
- Explain view: the plan as a tree of rounded-rect nodes with row and time bars, from `EXPLAIN ANALYZE`.
- Schema diagrams: tables as cards, foreign keys as 1 px orthogonal lines, auto-laid out.
- Data editing in the grid with a pending-changes bar and one transaction per commit.
- Key-value, document, search, time-series and graph browsers under the same tree (`Keys` first).
- Completion and error help in the editor; several result sets as segments; import, export, backup,
  users, monitoring and the MCP agent log as panels; the WebAssembly build; right-to-left text.
