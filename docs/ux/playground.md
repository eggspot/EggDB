# The EggDB browser playground

Designed by Claude Fable (D15). The first page someone sees when they try EggDB in a browser: the
GUI's main window ([gui.md](gui.md)) with the sidebar and tabs taken away, so only the query and
its result remain. One static `index.html` with inline CSS and one module `eggdb.js` (D16): no
framework, no font, image, icon or CDN; works offline once loaded. Icons are text or pure CSS.
Same words as `eggdb ui` and the GUI ([ui.md](ui.md)): `Result`, `Error:`, `NULL`, `3 rows · 0.4 ms`.

## Principles (from gui.md, applied)
1. The query and its result are the page; everything else is one bar above and one below.
2. Text first: every state is a string in a fixed place, never only a colour or a motion.
3. Keyboard complete: `Ctrl+Enter` (`Cmd+Enter` on macOS) runs from anywhere on the page.
4. Calm: the only motion is the running bar, under reduced motion a static bar plus `Running…`.
5. Nothing leaves the browser; the page says so in one sentence and never asks for a network.

## Tokens (CSS custom properties, `@media (prefers-color-scheme: dark)` swaps them)
| Role | Light | Dark | Playground use |
|------|-------|------|----------------|
| `bg0` | `#FFFFFF` | `#1E2227` | card, editor, grid body, dialog, select |
| `bg1` | `#F4F5F7` | `#23282E` | page background, `Result` bar, grid header |
| `bg2` | `#E9EBEE` | `#2A3038` | toolbar, status bar, hover fill |
| `zebra` | `#F8F9FA` | `#21262C` | every second grid row |
| `border` | `#D5D8DD` | `#363D47` | every 1 px line |
| `borderStrong` | `#B8BCC4` | `#4A525E` | header column separators, egg shell, pressed border |
| `textPrimary` | `#1B1F24` | `#E6E9EE` | SQL, cells, labels, grid header |
| `textSecondary` | `#5A6270` | `#A3ABB7` | summary, tags, status bar, placeholder, `Dialect` label |
| `textDisabled` | `#9AA1AB` | `#6B7380` | `NULL`, disabled buttons |
| `accent` / `accentHover` / `accentPressed` | `#1A6FD6` / `#155CB3` / `#114A91` | `#5A9CFF` / `#7AB0FF` / `#4788E8` | Run button, caret, running bar |
| `onAccent` | `#FFFFFF` | `#0D1117` | Run label |
| `selection` | `#D6E6FA` | `#2D4F7F` | `::selection` in the editor |
| `focusRing` | `#1A6FD6` | `#7AB0FF` | `outline: 2px solid`, `outline-offset: 2px`, on `:focus-visible` |
| `success` / `warning` / `error` | `#1E8E3E` / `#B26A00` / `#C8322B` | `#3FB950` / `#E0A030` / `#F0625A` | storage dot, `Error:` line |
| `brandYolk` | `#F5B700` | `#F5B700` | the egg mark's yolk |

Spacing `space2` 4, `space3` 8, `space4` 12, `space5` 16, `space6` 24; `radiusControl` 4,
`radiusCard` 8; lines 1 px `border`. Type: `title` ui bold 15/20, `body` ui 13/18, `bodyBold`,
`small` ui 11/14, `code` mono 13/20. Font stacks (installed fonts only, matching gui.md's chains):
`ui` = `system-ui, -apple-system, "Segoe UI", "Helvetica Neue", Arial, "Noto Sans", sans-serif`;
`mono` = `ui-monospace, Menlo, "Cascadia Mono", Consolas, "DejaVu Sans Mono", monospace`.
`color-scheme: light dark` so the native select, scrollbars and dialog backdrop follow the theme.

## Layout
**Desktop (> 640 px).** Body `bg1`, margin 0. One card: width `min(960px, 100% − 48px)`, margin
`24px auto`, `bg0`, 1 px `border`, `radiusCard`, `overflow: hidden`. Inside, top to bottom:

| Region | Height | Fill | Contents (padding 0 12, items 8 px apart unless said) |
|--------|--------|------|----------|
| toolbar | 44 | `bg2`, border below | left: egg mark 16 × 20, `h1` `EggDB playground` (`title`), then 16 px gap and `Runs in your browser. Nothing is sent to a server.` (`small` `textSecondary`). Right: `Dialect` (`body` `textSecondary`), `<select>` 140 × 28 (secondary dropdown look: `bg0`, 1 px `border`, `radiusControl`, padding 0 28 0 8, a CSS chevron 8 × 4 from two 1.5 px borders rotated 45°, 12 px from the right edge), `Run` primary 72 × 28, `Reset database` ghost 28 tall |
| editor | 180 (9 lines), user-resizable vertically, min 80 | `bg0`, border below | `<textarea>` 100 % wide, `code`, padding 8 12, no border or radius of its own, `caret-color: accent`, `::selection` `selection`, `white-space: pre`, `spellcheck="false"`, `wrap="off"` |
| `Result` bar | 32 | `bg1`, border below | left `Result` (`bodyBold`), then 12 px gap and the summary (`body` `textSecondary`); the running bar lives on its bottom edge |
| results body | min 240, grows with content (the page scrolls) | `bg0` | the blocks below |
| status bar | 28 | `bg2`, border above | `small` `textSecondary`; left: filled circle d 8 then the storage text at 8 px; right, 16 px apart: dialect name (`PostgreSQL`), `eggdb 0.2.0` |

The egg mark is pure CSS: a 16 × 20 box, `bg0` fill, 1 px `borderStrong`, `border-radius:
50% 50% 50% 50% / 60% 60% 40% 40%`, with a d 6 `brandYolk` circle centred 1 px below its middle.

**Narrow (≤ 640 px).** Card edge to edge: margin 0, no radius, no side borders. Toolbar wraps to
three rows: row 1 (44) egg mark + title left, `Reset database` right; row 2 (22, padding 0 12 8)
the explanation sentence; row 3 (44) `Dialect` label, select `flex: 1`, `Run` 72. Editor 120
(6 lines). Results min 160; each table scrolls horizontally inside its block (`overflow-x: auto`),
nothing wraps. Status bar wraps to two 28 px rows (storage left row, dialect and version right
row). Dialog width `calc(100% − 24px)`. Minimum supported width 320.

## Editor
Prefilled, dialect-neutral so the dialect picker never has to rewrite it:
```
CREATE TABLE customers (id INTEGER PRIMARY KEY, name VARCHAR(100) NOT NULL);
INSERT INTO customers VALUES (1, 'Ada Lovelace');
INSERT INTO customers VALUES (2, 'Grace Hopper');
SELECT id, name FROM customers ORDER BY id;
```
Placeholder when emptied: `Type SQL, then press Run.` No gutter, highlighting or auto-close (a
plain textarea); `Tab` moves focus, it never inserts spaces (a web page must not trap `Tab`).
The dialect select lists `PostgreSQL`, `SQLite`, `MySQL`, `SQL Server`, `Oracle` (values
`postgresql`, `sqlite`, `mysql`, `sqlserver`, `oracle`); its `title` says `Compatible with <name>`.
Changing it reopens the same file with the new dialect (`EggDB.open` again, same `path`), so data
survives; the status bar's dialect item updates; the editor text is untouched.

## The result area, every state
A run is the editor's whole text, or the selection when one exists, as one `db.execute(sql)`.
Blocks are stacked in statement order, one per entry of `results`, each 1 px `border` apart.

| State | Summary (in the bar) | Body |
|-------|---------|------|
| loading | `Loading EggDB…` | nothing; `Run` and `Reset database` disabled until `open` resolves |
| empty (before the first run, and after a reset) | nothing | one line, padding 8 12, `body` `textSecondary`: `Nothing has run yet. Press Run or Ctrl+Enter.` (`Cmd+Enter` when `navigator.platform` starts with `Mac`) |
| nothing to run (editor blank) | `Nothing to run` | unchanged |
| running | `Running…`; `aria-busy="true"` | previous blocks stay; a 2 px `accent` bar 120 px wide sweeps the bar's bottom edge, 1,000 ms per sweep, linear; `prefers-reduced-motion`: a static full-width 2 px bar. `Run` disabled meanwhile; no cancel in this release |
| command block (`columns === null`) | | one 28 px line, padding 4 12, `code` `textSecondary`: the tag (`CREATE TABLE`, `INSERT 0 1`) |
| rows block | `<n> rows · <time>` for the last block with columns, the time being the run's `elapsedMs` | the tag line as above (`SELECT 2`), then a `<table>`: header row 28 `bg1`, `body` `textPrimary`, 1 px `borderStrong` separators between header cells only, 1 px `border` below; body rows 24, `code`, padding 2 12, every second row `zebra`; columns min 48, max 480, longer cells cut with `…` (`text-overflow: ellipsis`, no wrap); a column is right-aligned when every non-null value matches `^-?\d+(\.\d+)?$`, otherwise left; `null` → the word `NULL` in `textDisabled`; `""` → nothing. Hover `bg2`. |
| no rows block in the run | `no rows to show · <time>` | the command blocks only |
| many rows (> 1,000) | `12,345 rows · 2.1 s` | the first 1,000 rows, then a 28 px line `small` `textSecondary`: `Showing the first 1,000 of 12,345 rows. Add a LIMIT to see others.` |
| error (`out.error` set) | `Error` | the blocks of the statements that ran, then an error block: `code`, `white-space: pre`, padding 8 12; the first line in `error`, the rest `textPrimary` (so `LINE 1:`, `^` and `SQLSTATE:` keep their columns). The text is the engine's; a first line not starting with `ERROR:` is prefixed `Error: `. |
| EggDB fails to load | `Error` | `Error: EggDB could not load in this browser.` then `Try: a current Chrome, Edge, Firefox or Safari.` as an error block; the editor stays usable |

Numbers: counts use `en-US` grouping (`2,031 rows`, `1 row`, `0 rows`); time is `0.4 ms` under
10 ms (one decimal), `87 ms` under 1,000 ms, `1.23 s` from 1,000 ms.

## Status bar strings
`Saved in this browser (OPFS)` with a `success` dot when `open` ran with `persist: true` and the
browser gave OPFS; `In memory: nothing is saved` with a `warning` dot when OPFS is unavailable
(the page retries `open` with `persist: false`) or in a private window that refuses it. The
version is `eggdb ` + `db.version` (one string the loader exports from the build; the item is
omitted when absent). For 8 s after a reset, the left group reads `Database reset` in `success`.

## Reset database
A native `<dialog>` styled as the GUI's dialog card: 360 wide, `bg0`, 1 px `border`, `radiusCard`,
padding 24; title `Reset database` (`title`); text `body`: `Deletes every table and row in this
browser's playground database. This cannot be undone.`; buttons right-aligned, 8 px apart:
`Cancel` secondary (84 × 28, focused first) and `Reset database` primary. `Esc` and `Cancel`
close; `Enter` on the primary resets: `await db.reset()`, the results body returns to the empty
state, the summary clears, the editor keeps its text, focus returns to the `Reset database` button.

## Keyboard and focus
- `Ctrl+Enter` / `Cmd+Enter` runs from any focused element (`keydown` on `document`);
  `Ctrl+Shift+Enter` runs the whole text even when there is a selection. `F5` is left to the
  browser (reload); the empty state names `Ctrl+Enter`, never `F5`.
- Tab order: dialect select → `Run` → `Reset database` → editor → results body (`tabindex="0"`,
  so a scrolling table is reachable) → nothing in the status bar. Buttons show states as gui.md:
  primary `accent`/`accentHover`/`accentPressed`, disabled `bg2` + `textDisabled`; ghost hover `bg2`.
- Every focusable control gets the 2 px `focusRing` outline on `:focus-visible` only; the editor
  shows focus by its caret and a 1 px `accent` inset box-shadow on its bottom border.

## Accessibility
- `<h1>` is the page title; `<label for>` on the select (`Dialect`) and a visually hidden
  `<label>` `SQL` on the textarea; `Run` and `Reset database` are `<button>`s with their text.
- Results: `<section aria-label="Result">`; the summary is `role="status"` (`aria-live="polite"`),
  so a screen reader hears `2 rows · 0.4 ms`, `Error`, `Running…`; the body is `aria-live="polite"`
  with `aria-busy` while running, so the blocks are read once when they land. Each table has a
  visually hidden `<caption>` `Statement 4: SELECT 2` and `<th scope="col">` headers; `NULL` cells
  carry `aria-label="NULL"` because the word is also the text, nothing is conveyed by colour alone.
- The storage text and the dialog use `role="status"` and `aria-labelledby` respectively; the
  dialog traps `Tab` natively (`showModal`) and closes on `Esc`.
- Contrast is the GUI's token table (all text ≥ 4.5:1 except `textDisabled`, exempt as disabled).
  The page follows the browser's font size (`rem`-free px sizes are fine at this scale; zoom works).

## Implementation notes (one HTML file, ~150 lines of JS)
`index.html` holds the CSS variables, the layout and the empty state. `main()` opens the database
(persist first, memory fallback), fills the status bar, enables the buttons. `run()` reads the
selection or the text, sets the running state, calls `db.execute`, then `render(out)` builds the
blocks with `document.createElement` (never `innerHTML` with result text), capping rows at 1,000.
`formatCount`, `formatTime`, `isNumeric(column)` are the only helpers. The dialog uses
`<dialog>`'s `showModal()`/`close()`. No state beyond `db`, the current dialect and the toast timer.

## Left out, on purpose
Syntax highlighting, line numbers and the current-line highlight; `Tab` inserting spaces; running
the statement under the caret; cancelling a run; query history and saving the editor text; export
and copy of cells; several result tabs or a `Messages` segment; column resizing, sorting and cell
selection; schema browsing; `EXPLAIN`; any network call, analytics or external asset; a theme
toggle (the OS setting decides); the familiar layouts (D15). These belong to the GUI's browser build.
