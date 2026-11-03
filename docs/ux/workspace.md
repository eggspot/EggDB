# The workspace: connect actions, the query tab, the grid, the object browser and the plan view

Designed by Claude Fable (D15) as the slice after [first-screen.md](first-screen.md): the connect
card acts for real, and the main window of [gui.md](gui.md) becomes a place to work. Every token,
component and rect not restated here is gui.md's; the card's rects, focus ring, themes and
accessibility tree are first-screen.md's; the words are the same as `eggdb ui` and the CLI
([ui.md](ui.md), [cli.md](cli.md)). All values are logical pixels at scale 1, rects `(x, y, w, h)`,
text positions the top-left of a line box; `cx, cy` is the card's origin. Keys are written
`Ctrl/Cmd+X`: `Cmd` on macOS, `Ctrl` elsewhere. Everything is drawn by `eggdb-gui` through the
platform layer (D8, D256), from scratch (D1), and the same on every OS and in the browser.

## 1. The connect card acts

### 1.1 Open (a typed path)
`Open` (the default button, `Enter` from the input, the checkboxes and the segments) opens the
file in the input. The path is used as typed, after `~` expands to the home directory and
surrounding whitespace is cut. With `Create the file if it does not exist` checked, a missing file
is created (its parent directory must exist); with `Open read-only` checked the file opens
read-only and the main window says `read-only`. Relative paths resolve against the current
directory of the process, and the resolved absolute path is what the main window shows.

### 1.2 Browse…
`Browse…` opens the OS file dialog on macOS (`NSOpenPanel`) and Windows (`IFileOpenDialog`),
filtered to `*.eggdb` with `All files` as the second filter, started in the input's directory
when it exists, otherwise `~`. Choosing a file puts its path in the input, selects nothing, places
the caret at the end and moves focus back to the input; cancelling the dialog changes nothing and
focus returns to `Browse…`. On Linux, the BSDs and in the browser there is no OS dialog in this
slice: `Browse…` is not drawn, the input is `(cx + 24, cy + 142, 512, 28)` and the focus order
skips stop 3. The platform layer reports `has_file_dialog()`; the card lays itself out from it.

### 1.3 Drag-and-drop
Dropping one file onto the window (anywhere, not only the card) puts its path in the input and
opens it at once with the checkboxes as set. While a drag is over the window the card's border
turns 2 px `accent` (`radiusCard`, no fill change) and the text `Drop to open` in `body`
`accent` replaces the title's right half: text at `(cx + 400, cy + 28)`. Several files: the
first is used and the status line of 1.5 says `Opened the first of 3 files`. A directory, or a
drag that is not a file, shows no ring and does nothing. In the browser the dropped file is read
into memory and opened as an in-memory database named after the file; `Create` is ignored there.

### 1.4 Connect to a server
The `Connect to a server` segment has gui.md's fields plus, above them, one input `URL`
(`(cx + 24, cy + 142, 512, 28)`, label `URL` at `(cx + 24, cy + 120)`, placeholder
`postgresql://user@host:5432/shop`) which fills the fields below as the user types, and is
re-written from them when a field changes. Accepted schemes, all meaning "the PostgreSQL protocol
that `eggdb serve` speaks": `postgresql://`, `postgres://` and `eggdb://`; `mysql://`,
`sqlserver://` and `redis://` set the protocol dropdown to their protocol. The port defaults per
protocol (5432, 3306, 1433, 6379). `Use TLS` is on unless the URL says `?sslmode=disable`
(`?tls=false` for the others). The password is never written into the URL field. The primary
button reads `Connect`; `Enter` from any field connects.

The `URL` row does not fit above gui.md's fields in the 480 px card, so the panel packs them
(card-relative, labels' text positions, then rects): `URL` (24, 120), input (24, 142, 512, 28);
`Protocol` (24, 186), dropdown (24, 208, 168, 28); `Host` (200, 186), input (200, 208, 224, 28);
`Port` (432, 186), input (432, 208, 104, 28); `Database` (24, 252), input (24, 274, 512, 28);
`User` (24, 318), input (24, 340, 250, 28); `Password` (286, 318), input (286, 340, 250, 28);
`Use TLS` checkbox (24, 384, 16, 16), label at x 48; the TLS warning at (24, 406) in `body`
`warning`, at most 332 px wide and elided, shown only while TLS is off and the card is enabled.
The status line of 1.5 is also at most 332 px wide. The last control ends 36 px above the
buttons, so two error lines never reach them; the card stays 480 px and the focus order is
unchanged (D274).

The protocol menu is one focus stop: `Up`/`Down` and `Space` step its value in place (the open
list is a later slice), and a Port still holding the old protocol's default takes the new one's.
A password typed into the URL (`user:secret@`) fills the Password input and is taken out of the
URL when the focus leaves it or `Connect` starts. Empty fields mean libpq's defaults: host
`localhost`, user `eggdb` (the user `eggdb serve` creates) and the user's name as the database.
Until the client speaks the other protocols, `Connect` on them shows `Error: the client cannot
connect over MySQL protocol yet` / `Try: choose "PostgreSQL protocol", which eggdb serve always
speaks`, with the focus on the menu (D274).

### 1.5 Progress
Pressing `Open` or `Connect` at once:
- disables the whole card (every control `textDisabled`, inputs `bg1`), keeps focus where it was;
- replaces the primary button's label with `Opening…` / `Connecting…` and draws gui.md's running
  indicator (a 2 px `accent` bar, 120 px, sweeping the card's bottom edge inside the border, 1 s
  per sweep; static full-width bar under reduced motion);
- shows a status line at `(cx + 24, cy + 440)`, `body` `textSecondary`: `Opening shop.eggdb…`
  (the file's name) or `Connecting to db.example.com:5432…` (host and port);
- `Esc` and `Cancel` (now enabled) cancel: the card re-enables, the status line reads `Cancelled`
  for 2 s, focus returns to the input.
An open that takes under 120 ms shows no progress state (the window changes directly), so the
common case never flickers.

### 1.6 Failure
The card re-enables and first-screen.md's error lines appear under the segments, moving the panel
down 18 px per line. The texts are the engine's, the same as `eggdb` prints them (cli.md 7):

| Line | `body` colour | Text |
|------|------|------|
| 1 | `error` | `Error: <message>` with the code first when the engine has one: `Error 42P01: relation "users" does not exist`; a file error has no code: `Error: cannot open /data/shop.eggdb: no such file` |
| 2 | `textPrimary` | `Try: <fix>` from the engine's `try:` line (`Try: check "Create the file if it does not exist", or eggdb create /data/shop.eggdb`); omitted when there is none |

The lines wrap inside the card's 512 px content width (at most 3 lines each, then `…`). Focus
moves to the field the error is about (the File input for a file error, Host for a connection
refused, Password for an authentication failure, the URL field when the URL did not parse) with
its value selected; the input gets its `error` border. The screen reader hears the two lines
(1.6 of section 7). A second attempt clears the lines before it starts.

Failure messages this slice must produce, by cause (`<path>`, `<host>:<port>` filled in):

| Cause | Line 1 | Line 2 |
|-------|--------|--------|
| no such file, Create unchecked | `Error: cannot open <path>: no such file` | `Try: check "Create the file if it does not exist", or eggdb create <path>` |
| directory missing, Create checked | `Error: cannot create <path>: the directory does not exist` | `Try: create the directory first, or choose another path with Browse…` |
| not an EggDB file | `Error: <path> is not an EggDB file` | `Try: open an .eggdb file, or restore this file with eggdb restore` |
| locked by another process | `Error: <path> is open in another process` | `Try: close the other program, or check "Open read-only"` |
| permission denied | `Error: cannot open <path>: permission denied` | `Try: check the file's permissions, or check "Open read-only"` |
| URL does not parse | `Error: the URL is not a database URL` | `Try: postgresql://user@host:5432/database` |
| connection refused | `Error: cannot connect to <host>:<port>: connection refused` | `Try: check that the server is running and the port is open` |
| no route, timeout (10 s) | `Error: cannot connect to <host>:<port>: timed out after 10 s` | `Try: check the host name and your network` |
| TLS failed | `Error: cannot connect to <host>:<port>: the server does not offer TLS` | `Try: uncheck "Use TLS" only on a network you trust` |
| authentication failed | `Error 28P01: password authentication failed for user "<user>"` | `Try: check the user name and password` |
| database missing | `Error 3D000: database "<db>" does not exist` | `Try: check the database name, or leave it empty for the default` |

### 1.7 Success
The main window replaces the card (no motion). The toolbar's connection name is the database's
name (the file's stem, or the URL's database), the file text the path or `host:port`; the sidebar
tree has the connection's node expanded with `Tables` expanded (section 4); one tab `Query 1`
with an empty editor has keyboard focus; the status bar reads `Connected · shop.eggdb ·
read-write` (`read-only` when so; `Connected · db.example.com:5432 · TLS` for a server). The
status bar's left group shows the toast `Opened shop.eggdb` / `Connected to db.example.com` for
8 s. Focus is in the editor: `Enter` types a newline there (the card's `Enter` meant `Open`; the
main window's `Enter` never runs anything, only `Ctrl/Cmd+Enter` does).

## 2. The query tab

### 2.1 Regions
gui.md's `tabs` (36 tall), `editor` and `results` with the splitter between them. A tab is one
dialect plus one connection; `Ctrl/Cmd+T` adds `Query 2`, `Query 3`… (the lowest unused number);
`Ctrl/Cmd+W` closes the current one (a tab with text and no save asks `Close Query 2 without
saving?` with `Close` and `Keep`, default `Keep`). The editor is gui.md's (gutter 48, 20 px
lines, `code`, current line `bg1`, syntax tokens by the dialect), now multi-line with a vertical
and a horizontal overlay scrollbar and the caret always scrolled into view (4 lines of margin).

### 2.2 The dialect pill
The toolbar's dropdown (`(1056, 8, 120, 28)` in the golden frame, right-aligned to the toolbar at
other widths) shows the tab's dialect: `PostgreSQL`, `MySQL`, `SQL Server`, `Oracle`, `SQLite`.
Click, `Space`, `Enter` or `Alt+Down` open a menu (popover: `bg0`, 1 px `border`, `radiusPopover`,
shadow 0 4 12 at 16 % black, rows 28 tall, 160 wide, below the button, 4 px gap) listing the
dialects with a check icon on the current one; `Up`/`Down` move, `Enter` picks, `Esc` closes,
typing a letter jumps. Switching re-lexes the text with the new dialect's keywords and keeps the
text, caret, selection and undo history; the status bar's dialect item changes with it, and the
next run uses it. The pill's name is `Dialect`, value the dialect.

### 2.3 Running
| Action | Keys | What runs |
|--------|------|-----------|
| Run | `Ctrl/Cmd+Enter`, `F5`, the `Run` button | the selection when there is one, else the statement under the caret: from the previous `;` (or the start) to the next `;` (or the end), with the dialect's rules for strings, comments and `$$` bodies, so a `;` inside them does not split |
| Run all | `Ctrl/Cmd+Shift+Enter`, `Run all` | the whole tab, statement by statement, stopping at the first error |
| Explain | `Ctrl/Cmd+E`, `Explain` | section 5 |
| Stop | `Esc` in the editor, `Cmd+.` / `Ctrl+Break`, `Stop` | cancels the running statement |

While a statement runs: `Run`, `Run all` and `Explain` are disabled and `Stop` enabled; the tab's
close icon is replaced by a spinner (a 12 px arc of 270°, 1.5 px `accent`, one turn per second;
under reduced motion a static 12 px ring with a 3 px `accent` dot at its top); the running
indicator sweeps the toolbar's bottom edge; the results header's summary reads `Running · 1.2 s`,
updated every 100 ms, and after 2 s adds `· 12,400 rows so far` when rows stream. Rows appear in
the grid as they arrive (every 100 ms or 1,000 rows, whichever comes first), the grid staying
scrolled to the top unless the user scrolled. A second `Ctrl/Cmd+Enter` while running does
nothing; the status bar shows `Running… press Esc to stop` for 2 s. Stopping shows `Stopping…` in
the summary until the engine confirms, then `Stopped · 12,400 rows (partial)` and the rows stay.

### 2.4 The status line
The results header's summary (`body` `textSecondary` at `(428, 393)` in the golden frame, after
`Results` and `Messages`) is one string:

| Case | Text |
|------|------|
| a result set | `3 rows · 0.4 ms · 1.2 KB` (`1 row`, `0 rows`; time with one decimal under 10 ms, none from 10 ms, `s` with two decimals from 1,000 ms; bytes `B`, `KB`, `MB`, `GB` with one decimal from `KB`, 1,024-based, counting the result's wire bytes) |
| a statement without rows | `INSERT 0 2 · 0.3 ms` (the dialect's tag as `eggdb sql` prints it: `CREATE TABLE`, `UPDATE 3`, `Committed`) |
| run all | `5 statements · 3 rows · 12 ms` and the last result set in the grid; the `Messages` segment lists each statement's tag line |
| an error | `Error` (the segment `Messages` becomes active and shows the engine's text) |
| cancelled | `Stopped · 12,400 rows (partial)` |
| more than the fetch limit | `100,000 rows shown of 2,104,311 · 1.4 s · 8.2 MB` (section 3.7) |

The status bar's left group repeats the tags (`SELECT 3 · 0.4 ms`), as in gui.md. Numbers use a
thousands separator (`,`); the CLI's `Time:` figure and the GUI's are the same measurement.

### 2.5 Inline errors
An error from the engine (SQLSTATE, message, position) is shown three ways at once:
1. **Squiggle**: a 2 px `error` zigzag (period 4 px, amplitude 2 px, drawn at the line box's
   bottom 2 px) under the error's range: from the position to the end of the token there, or the
   whole statement when the engine gives no position. The caret moves to the position.
2. **Message**: a one-line strip at the editor's bottom edge, `(260, 352, 1020, 28)` in the golden
   frame (the editor's last 28 px, pushing the text area up), fill `bg1`, 1 px `border` on top,
   the error icon (circle + ×, 12 px, `error`) at x 272, then `code` text at x 292:
   `Error 42P01: relation "users" does not exist · Ln 1, Col 18`, then, when the engine gave a
   fix, a ghost button `Fix: create table users` (label `Fix: ` + the fix's first 40 characters)
   right-aligned 12 px from the strip's right edge, which puts the fix's statement in a new tab
   (`Fix 1`) without running it. `Esc` in the editor hides the strip; the next run or edit of the
   error's line hides it too; the squiggle stays until the line is edited.
3. **Messages**: the engine's full text in the `Messages` segment, `code`, as `eggdb sql` prints
   it (`ERROR:`, `LINE n:`, the `^` line, `SQLSTATE:`).

Errors are the engine's words, never stored data (D45), and identical in the CLI and over MCP.

## 3. The results grid

### 3.1 Layout
gui.md's grid: header row 28 (`bg1`, 1 px `border` below), body rows 24 (`bg0` / `zebra`), cell
padding 0 12, `code`, row gutter 48 wide with the 1-based row number right-aligned 12 px from its
right edge in `textSecondary`, column separators 1 px `borderStrong` in the header only. Both axes
are **virtualized**: the body draws only the rows in `visible_rows(total, first, height)` and the
columns whose x range meets the viewport, and the engine is asked for rows in pages of 1,000 as
the scroll approaches them (two pages ahead); a page not yet fetched draws its cells as 60 × 12
`bg2` rects (`radiusControl`) and reads `Loading…` to the screen reader. Row heights are fixed, so
the scrollbar's thumb is exact for any count. Column widths are `column_widths` (the wider of the
header and the first 200 rows, clamped 48–480), draggable, double-click resets.

### 3.2 Cells
| Value | Drawn | Aligned |
|-------|-------|---------|
| integer, decimal, float, money | the engine's text | right |
| text, date, time, timestamp, interval, uuid | the text, one line, cut with `…` | left |
| boolean | `true` / `false` | left |
| `NULL` | the word `NULL` in `textDisabled` | as its column |
| empty string | nothing; `""` in `textDisabled` only in the full-value view | left |
| JSON, array | the compact text (`{"a":1}`), cut with `…` | left |
| blob | `0x` + first 16 bytes in hex + `…`, `textSecondary` | left |
| vector | `[0.12, -0.33, … 768]` (first 3 values, then the dimension), `textSecondary` | left |
| a key's TTL (key-value rows) | `12 s`, `3 m 20 s`, `none` | right |
Text is never coloured by its value; the type alignment and the `NULL` word carry the meaning.
Control characters draw as `␀`-style glyphs when the font has them, else as `·`.

### 3.3 Keyboard navigation
The grid is one `Tab` stop (roving focus on the current cell). From the editor `Ctrl/Cmd+3` or
`Tab` through the results header reaches it; the current cell starts at row 1, column 1.

| Key | Moves |
|-----|-------|
| arrows | one cell; at an edge, nothing (no wrap) |
| `PgUp` / `PgDn` | one viewport of rows, keeping the column |
| `Home` / `End` | the row's first / last column; with `Ctrl/Cmd`: the first / last row of the result (`End` fetches the last page) |
| `Tab` / `Shift+Tab` | leaves the grid to the next / previous region (never cell to cell) |
| `Enter`, `F2` | opens the full-value view: a popover 480 × 240 below the cell (above it when no room), `code`, wrapped, read-only, with `Copy` (ghost) and `Esc` to close |
| `Shift+Space` | selects the current row |
| `Ctrl/Cmd+Space` | selects the current column |
| `Ctrl/Cmd+A` | selects every fetched row and column |
| `Esc` | collapses the selection to the current cell |
The current cell is always scrolled into view with the header excluded from the reckoning.

### 3.4 Selection
A selection is one rectangle: anchor to current cell. `Shift` + arrows, `Shift+PgUp/PgDn`,
`Shift+Home/End` and `Shift` + click extend it; click sets anchor and current; drag extends;
gutter click selects a row, `Shift` + gutter click a row range; header click selects a column
(`Shift`: a column range); the top-left corner cell `(260, 418, 48, 28)` selects all. Drawn as
`selection` under the cells (`selectionInactive` when the grid is not focused) with the current
cell's 2 px `accent` rect on top; the selected rows' gutters and the selected columns' headers
fill `bg2`. The status bar's left group adds `4 × 3 cells` while more than one cell is selected.

### 3.5 Copy
`Ctrl/Cmd+C` copies the selection as TSV: cells joined by `\t`, rows by `\n`, `NULL` as empty,
text with `\t`, `\n`, `\r` or `\\` escaped as `\t`, `\n`, `\r`, `\\`, no header; with
`Ctrl/Cmd+Shift+C` the header row comes first. `Ctrl/Cmd+Shift+I` copies the selected rows as
`INSERT INTO <table> (<cols>) VALUES (...);` in the tab's dialect, one statement per row, every
selected column, `NULL` as `NULL`, strings quoted and escaped the dialect's way, numbers bare,
dates quoted; the table name is the result's source table when the query read one table, else
`result`. The copied text's first 40 characters show as a toast `Copied 12 cells` / `Copied 3
INSERT statements` for 2 s. The grid's `Ctrl/Cmd+C` is gui.md's listed shortcut; the `INSERT`
form moves from `Shift` to `Shift+I` because `Shift+C` is the header copy, as in the CLI's
`\copy` family (decided here, recorded for client-brief.md's next revision).

### 3.6 Pointer
Hover fills the row `bg2` (80 ms); the wheel scrolls 3 rows per notch, `Shift` + wheel 3 columns
horizontally; a header separator's 6 px hit band shows a column-resize cursor; double-click on a
cell opens the full-value view. Right-click opens a menu with `Copy`, `Copy with header`, `Copy
as INSERT`, `Select row`, `Select column`, `Open value` (keyboard: `Shift+F10`, the menu key).

### 3.7 The truncated-results banner
Results fetch at most 100,000 rows into the grid. Beyond that a banner sits between the results
header and the grid header: `(260, 418, 1020, 28)`, fill `bg1`, 1 px `border` below, the warning
icon (`warning`, 12 px) at x 272, then `body` `textPrimary` at x 292: `Showing the first 100,000
rows of 2,104,311. Add LIMIT, export, or sort and filter in the query.`, with a ghost button
`Export…` right-aligned 12 px from the right edge and a close icon after it (24 × 24 hit). The
grid header moves down 28 while it shows; the summary says `100,000 rows shown of 2,104,311`. The
banner is `textPrimary` on `bg1` (13.9:1), with the icon and the words carrying the meaning. When
the engine cannot count the rest, the text is `Showing the first 100,000 rows. More exist.`

## 4. The object browser and the inspector

### 4.1 The tree
gui.md's sidebar (260 wide, 180–480, `Ctrl/Cmd+B` toggles, under 800 px window width a 36 px icon
rail), filter field on top, rows 24 tall, indent 16 per level, chevron slot 16, icon 12 × 12.
Level 0 is the connection (`bodyBold`, right text `read-write`, `read-only` or `TLS`); level 1
the model groups, each with its count as right text, in this order, a group drawn only when the
connection has that model (`Tables` always):

| Group | Icon (1.5 px strokes in a 12 × 12 box) | Children (level 2) | Right text of a child |
|-------|------|------|------|
| `Tables` | folder | tables by name | row count when analyzed (`2.1M`), else none |
| `Views` | folder | views | |
| `Indexes` | folder | `<index> on <table>` | `unique` when so |
| `Sequences` | folder | sequences | the current value |
| `Keys` | key | `default key space`, then each collection (ui.md's words) | key count |
| `Documents` | rect (0, 0, 10, 12) with a folded corner line (6, 0)–(10, 4) | document collections | document count |
| `Search`, `Series`, `Graphs`, `Vectors`, `Queues` | folder | later slices; drawn only when the connection reports the model | |

A table (level 2) expands to `Columns` (level 3, each `name type` with `pk` or `not null` as right
text), `Indexes` and `Triggers` groups; a key collection expands to nothing (its keys are a grid);
a document collection expands to nothing. Counts and names come from the engine's catalog and
refresh after every run that ran DDL, with `F5` in the tree or `Refresh` in the row's menu
refreshing on demand. The filter field (`Filter objects`) hides rows whose label does not contain
the text, case-insensitive, keeping their ancestors; `Esc` in it clears and returns focus to the
tree; the count right text becomes `2 of 12` while filtering.

### 4.2 Keyboard
The tree is one `Tab` stop (`Ctrl/Cmd+1` focuses it); the current row has the ring.

| Key | Does |
|-----|------|
| `Up` / `Down` | previous / next visible row |
| `Right` | expands, or moves to the first child when expanded; `Left` collapses, or moves to the parent |
| `Home` / `End` | first / last visible row; `*` expands every sibling |
| `Enter` | a table or view: opens a new tab `SELECT * FROM <name> LIMIT 100;` (the dialect's form) and runs it; a key collection: a tab with the keys grid (`SCAN` form); a document collection: a tab with `db.<name>.find().limit(100)`; a group or the connection: toggles expansion |
| `Shift+Enter` | inserts the row's name at the editor's caret (quoted when the dialect needs it) |
| `Space` | selects the row and shows it in the inspector without opening a tab |
| letters | jump to the next row whose label starts with the typed prefix (1 s to type more) |
| `Delete` | on a recent connection in the connect screen's list: forgets it; in the object tree: nothing |
| `Shift+F10`, menu key, right-click | the row's menu: `Open`, `Insert name`, `Copy name`, `Show DDL`, `Refresh`, then `Drop…`; `Drop…` shows the statement it will run in a dialog (`Drop table orders?` with the DDL in `code`, `Drop` secondary in `error` text, `Cancel` default) |
Double-click is `Enter`; single click selects and shows the inspector. A row's menu never runs
a statement without showing it first (client-brief.md principle 2).

### 4.3 The inspector
The seventh region, 280 wide (200–480), on the right of the main area, `bg1`, 1 px `border` on
its left. At a window 1440 px or wider it is open by default; narrower, it is closed and
`Ctrl/Cmd+I` opens it as an overlay over the right edge of the main area (shadow 0 4 12 at 16 %
black, `Esc` closes). Content for the selected tree row, top to bottom, each a block with a
`title` heading in `bodyBold` and 12 px padding:

| Block | Content |
|-------|---------|
| header | the name in `title` (`orders`), under it `Table · 2.1M rows · 184 MB` in `small` `textSecondary` (`View`, `Index on orders`, `Collection · 312 keys`, `Documents · 1,204`) |
| `Columns (9)` | a two-column list: name `code` `textPrimary`, type `code` `textSecondary` right-aligned, `pk` / `not null` as `small` badges (`bg2`, `radiusPill`, padding 0 6) |
| `Indexes (3)` | name and `(columns)`, `unique` badge |
| `DDL` | the `CREATE` statement from the engine in `code`, read-only, 1 px `border` box, a ghost `Copy` button in its top-right; `Open in tab` puts it in a new tab |
| `Sample rows` | the first 5 rows as a small grid (rows 20 tall, `small`, 3 columns at most then `…`), fetched when the block scrolls into view; `Open` runs the `LIMIT 100` tab |
A connection row shows the file's `stats` (`eggdb stats`'s words) instead; a group row shows its
count and the `Refresh` button. The inspector scrolls as one; its blocks collapse with their
headings (`Right` / `Left`, click), state kept per block. `Ctrl/Cmd+P` is the cross-model search
(a later slice); until then the filter field is the search.

## 5. The plan view

### 5.1 Getting a plan
`Ctrl/Cmd+E` or `Explain` runs the statement under the caret (or the selection) as `EXPLAIN` in
the tab's dialect, read-only, never the statement itself (cli.md 24); `Ctrl/Cmd+Shift+E` runs
`EXPLAIN ANALYZE` (it does run the statement, inside a transaction that is rolled back for a
statement that writes, and the toolbar's `Explain` menu says `Explain (analyze) runs the
statement`). The results header gains a `Plan` segment after `Messages`, active when a plan came
back; `Results` keeps the previous rows. A dialect without a plan (`Oracle` for now) shows the
CLI's `not_supported` message in `Messages`.

### 5.2 Layout
The plan is a tree drawn top-down in the `results` body: the root node at the top centre, children
below their parent, siblings 24 px apart, levels 56 px apart (node 40 tall + 16 gap), the whole
tree centred horizontally and scrollable in both axes when larger than the panel (same overlay
scrollbars). Parent-to-child links are 1 px `borderStrong` orthogonal lines: down 8 px from the
parent's bottom centre, across to the child's x, down to its top. Nodes are rounded rects
(`radiusControl`, width = text width + 24, min 120, max 320, height 40), fill `bg0`, 1 px
`borderControl`:

| Row inside the node | Text |
|------|------|
| 1, `body` `textPrimary` at (12, 4) | the operation as the engine names it: `Index Scan using users_pkey on users`, `Seq Scan on orders`, `Hash Join`, `Sort`, cut with `…` at the max width |
| 2, `small` `textSecondary` at (12, 22) | `cost 0.29..8.31 · rows 1` under `EXPLAIN`; `12.4 ms · rows 1,204 · buffers 38` under `ANALYZE` (`time` is the node's own time excluding its children; `buffers` the pages read); fields the engine does not give are omitted |

Below each node's row 2 a 4 px bar at `(12, 34, w − 24, 4)` `bg2` with a filled part `accent`
whose width is the node's share of the root's total cost (`EXPLAIN`) or of the root's total time
(`ANALYZE`), so the eye finds the heavy node before reading. The plan's text form (the engine's
rows, as the CLI prints them) is one `Text` toggle (ghost button in the results header, `Alt+T`)
away, for copying.

### 5.3 Hot nodes
A node's **share** is its own cost (total cost minus its children's) or its own time divided by
the root's total. Tinting is by thresholds, and the share is always printed so colour is never
alone:

| Share | Border | Row 2 adds | Screen reader state |
|-------|--------|-----------|------|
| under 20 % | `borderControl` 1 px | nothing | |
| 20 % to under 50 % | `warning` 2 px | `· 34 %` in `warning` | `warm` |
| 50 % and over | `error` 2 px | `· 62 %` in `error` | `hot` |
A node whose estimated rows and actual rows differ by 10× or more (under `ANALYZE`) adds
`· est. 10 → 1,204` in `warning` and the state `misestimated`.

### 5.4 Selecting a node
The plan is one `Tab` stop with a current node (the root at first), ring `focusRing` around the
node. `Up` goes to the parent, `Down` to the first child, `Left` / `Right` to the previous / next
sibling, `Home` to the root, `Enter` or click selects: the node fills `selection`, and the
editor highlights the statement's range the node is about (`selection` behind the glyphs; the
whole statement when the engine gives no range) and scrolls the caret there without moving it.
`Esc` clears the highlight. Hover fills a node `bg2`. The node's full text (row 1 and 2 untruncated
plus the engine's extra lines such as `Filter: (total > 100)`) shows in a tooltip after 600 ms of
hover or at once with `Ctrl/Cmd+Shift+Space`, popover style, `code`, at most 480 wide.

## 6. Pointer, clipboard and window

### 6.1 Hover, pressed, click
gui.md's component states: hover and pressed fills fade 80 ms linear (instant under reduced
motion); a pressed state needs the pointer still inside the control on release to activate, and
a release outside cancels. A click anywhere in a region focuses that region (the editor puts the
caret at the click, the grid sets the current cell, the tree selects the row, the plan selects
the node); a click on empty space in a region focuses the region without changing its state.
Mouse focus never draws the ring on buttons, links and tabs; it does on inputs (with the `accent`
border), on grid cells (the `accent` cell rect), tree rows (`selection`) and plan nodes.
Double-click in the editor selects the word, triple-click the line; in a grid cell it opens the
value, on a tab's strip it adds a tab, on a splitter it resets it.

### 6.2 The caret
2 px `accent`, the line's full 20 px, blinking 530 ms on / 530 ms off, solid for 500 ms after
any key or click in the editor, solid always under reduced motion and when the window is not
active (then `textDisabled`). Only the focused editor or input draws a caret; others draw none.

### 6.3 Clipboard
The OS clipboard on desktop (`NSPasteboard`, `OpenClipboard`, X11 `CLIPBOARD` with `UTF8_STRING`
and `text/plain;charset=utf-8`), the browser's async clipboard API in the browser (which may ask
the user once). Copy from the editor and inputs is the selected text; from the grid section 3.5;
from the tree `Copy name`; from the plan the current node's full text; from the inspector's DDL
the statement. Paste into the editor inserts at the caret, replacing a selection, normalising
`\r\n` to `\n`; a paste of more than 1 MB is refused with the toast `Paste too large (limit 1 MB);
open the file instead`.

### 6.4 Resizing and the minimum size
Minimum window size 800 × 560 (first-screen.md). Regions resize from a tree of splits: the
sidebar and inspector keep their widths, the centre takes the rest; the editor/results split keeps
its ratio (40 % results by default), within the minimums (editor 80, results 120). Narrower than
1024 the inspector closes; narrower than 800 the sidebar becomes the 36 px rail. Sizes persist per
layout profile. Resizing redraws at once, never animates, and the grid keeps its first visible
row and current cell. In the browser the window is the viewport and the same rules apply, with
the explorer as a slide-over drawer under 1024 (client-brief.md 2).

## 7. Accessibility

The tree of first-screen.md grows these nodes (generic role → macOS / UIA control type and
patterns / AT-SPI role and interfaces / ARIA). Names and values are the visible strings; every
state has a text form on screen.

| Element | Role and patterns | Name | Value / states / properties |
|---------|------|------|------|
| toolbar | toolbar → AXToolbar / ToolBar / tool bar / `toolbar` | `Toolbar` | |
| `Run`, `Run all`, `Explain`, `Stop` | button → AXButton / Button, Invoke / push button, Action / `button` | the label | disabled while so; the shortcut as the accelerator (`AXKeyboardShortcut`, `AcceleratorKey`, `aria-keyshortcuts`) |
| dialect pill | combo box → AXPopUpButton / ComboBox, Value + ExpandCollapse / combo box, Selection / `combobox` | `Dialect` | value `PostgreSQL`; expanded while the menu is open; the menu is a `menu` of `menuitemradio` items, one checked |
| sidebar filter | text field → AXTextField / Edit, Value + Text / entry, EditableText / `searchbox` | `Filter objects` | the text |
| object tree | tree → AXOutline / Tree, Selection / tree, Selection / `tree` | `Objects` | one selected item; `aria-multiselectable=false` |
| tree row | tree item → AXRow (outline row) / TreeItem, ExpandCollapse + SelectionItem + Invoke / tree item, Action + Selection / `treeitem` | the label (`orders`), with the right text as the description (`2.1M rows`) | level, position in set, set size; expanded / collapsed (a leaf has neither); selected; `aria-level`, `aria-posinset`, `aria-setsize` |
| tab strip | tab list / `tablist` | `Queries` | |
| query tab | tab → AXRadioButton / TabItem, SelectionItem / page tab / `tab` | `Query 1` | selected; `running` as a state text (`Query 1, running`) while running; its close is a child `button` `Close Query 1` |
| editor | text area → AXTextArea / Edit (multi-line), Text + Value / text, EditableText + Text / `textbox` `aria-multiline=true` | `Query` | the text; caret as the selection range; `Ln 1, Col 44` through the text pattern; an error range is an annotation (UIA `AnnotationTypes` Error, AT-SPI `invalid` attribute, ARIA `aria-invalid=true` + `aria-errormessage` to the strip) |
| error strip | alert → AXGroup (live, assertive) / Text (LiveSetting Assertive) / alert / `alert` | the whole text | announced at once when it appears |
| `Fix:` button | button | `Fix: create table users` | |
| results header segments | tab list / tabs, `Results`, `Messages`, `Plan` | `Result views` | selected one |
| status line | status → AXStaticText (live, polite) / Text (LiveSetting Polite) / status bar / `status` | the summary text | announced politely after every run: `3 rows, 0.4 milliseconds` (the `·` read as a pause, `ms` as `milliseconds`) |
| grid | table → AXTable / DataGrid, Grid + Table + Selection + Scroll / table, Table + Selection / `grid` | `Results` | row count, column count (`RowCount`, `ColumnCount`); rows not yet fetched count in the total |
| column header | header cell → AXCell in AXRow (header) / HeaderItem / column header, TableCell / `columnheader` | the column's name | `aria-sort` when sorted |
| grid cell | cell → AXCell / DataItem, GridItem + TableItem + SelectionItem + Value / table cell, TableCell + Text / `gridcell` | `<column> <value>` in the name only in the browser; elsewhere the value is the value and the column comes from the header | value the drawn text (`NULL` reads `null`, blobs `blob, 184 bytes`, a loading cell `loading`); selected; row and column index; the current cell is the focused child |
| banner | status (live, polite) / `status` | the banner text | |
| inspector | group → AXGroup / Pane / panel / `region` | `Inspector` | `aria-labelledby` the name heading |
| inspector blocks | disclosure → AXDisclosureTriangle / Button, ExpandCollapse / toggle button / `button` `aria-expanded` | `Columns (9)` | expanded / collapsed |
| DDL box | text area, read-only | `DDL` | the statement; read-only |
| plan | tree / `tree` | `Plan` | one selected node |
| plan node | tree item / `treeitem` | row 1 (`Index Scan using users_pkey on users`) | description row 2 (`cost 0.29 to 8.31, rows 1, 62 percent`); states `hot`, `warm`, `misestimated` as text in the description; selected; level, position, set size |
| splitters | splitter → AXSplitter / Thumb, Transform / separator, Value / `separator` `aria-orientation` | `Sidebar width`, `Results height`, `Inspector width` | value the size in px, min and max |
| status bar | status bar → AXGroup / StatusBar / status bar / `status` | `Status` | each item a `button` with its text |
| toast | alert / `status` (`Committed`) or `alert` (`Error:`) | the text | |
| menus | menu → AXMenu / Menu / menu / `menu`; items `menuitem`, radio items `menuitemradio` | the item label | checked; disabled |
| dialogs (`Close Query 2 without saving?`, `Drop table orders?`) | dialog → AXSheet / Window (modal) / dialog / `alertdialog` | the title | modal, traps `Tab`, `Esc` cancels, the default button named |

Announcements (live regions, never moving focus): the connect card's error lines (polite), the
progress status line's `Opening…` (polite) and its `Cancelled`; the results status line after a
run (polite); the editor's error strip (assertive); the truncated banner (polite); `Copied 12
cells` (polite); `Stopping…` then `Stopped…` (polite). Selection changes in the grid raise the
platform's selection event with the new cell's value; a `Shift` range raises one event with
`4 rows by 3 columns selected`. The plan's current node raises a focus event with its name and
description. Every shortcut in section 2 to 5 is exposed as the control's accelerator, and every
action is in the command palette by the same name (a later slice wires the palette).

## 8. Acceptance checklist

Each item is checked by a test that renders a PNG offscreen (the box typeface, 1280 × 800, light
unless stated) and compares pixels, or dumps the accessibility tree and compares nodes. `png:` and
`tree:` name which.

**1. Connect**
- [ ] png: a typed path to a fixture file, `Enter` → the main window frame with `shop` and
      `shop.eggdb` in the toolbar and `Connected · shop.eggdb · read-write` in the status bar
      (byte-equal to gui.md's golden frame with an empty editor).
- [ ] png: a missing path with Create checked creates the file; with Create unchecked, the frame
      shows the two error lines at `(cx + 24, cy + 100)` and `(cx + 24, cy + 118)` with the texts
      of 1.6 and the input's `error` border; tree: the File input is focused with its value
      selected and the error nodes are live, polite.
- [ ] png: on Linux `Browse…` is absent and the input is 512 wide; on macOS and Windows the
      platform layer's `has_file_dialog()` is true and the button is at `(cx + 452, cy + 142)`.
- [ ] png: a simulated drag-over draws the 2 px `accent` card border and `Drop to open`; a drop
      opens the file (frame equals the first item's).
- [ ] png: the server segment shows the `URL` input; typing `postgresql://ada@db:5433/shop` fills
      Host `db`, Port `5433`, Database `shop`, User `ada`; `?sslmode=disable` unchecks `Use TLS`
      and shows the warning line. A refused port shows `Error: cannot connect to db:5433:
      connection refused` and focuses Host.
- [ ] png: with a stub engine that takes 1 s, the frame at 200 ms shows `Opening…`, the disabled
      card and the status line; `Esc` at 500 ms shows `Cancelled`; tree: every control disabled
      while opening.

**2. Query tab**
- [ ] png: two statements on two lines, caret in the second, `Ctrl+Enter` runs only the second
      (the grid shows its rows and the summary its count); `Ctrl+Shift+Enter` shows
      `2 statements · …`.
- [ ] png: the dialect menu open: 5 rows of 28 at `(1056, 40, 160, 140)` with the check icon on
      `PostgreSQL`; after `Down`, `Enter` the pill reads `MySQL` and the same text re-highlights
      (`LIMIT` stays `syntaxKeyword` in both, a backtick identifier is `textPrimary` in MySQL).
- [ ] png: a 1 s stub statement at 500 ms: `Run` disabled, `Stop` enabled, the tab's spinner, the
      indicator bar at the toolbar's bottom, `Running · 0.5 s`; under reduced motion the static
      bar and ring.
- [ ] png: `SELECT * FROM users` on `shop` shows the squiggle under `users` (x range from the
      position to the token's end at row y 98–99), the strip's text `Error 42P01: relation "users"
      does not exist · Ln 1, Col 15`, and `Messages` with the four CLI lines; tree: the editor's
      `aria-invalid`, the strip `alert`.
- [ ] png: the summary strings of 2.4 for 1 row, 0 rows, 1,234 rows at 1,234 ms (`1.23 s`), an
      `INSERT 0 2` and an error, each compared as the rendered text.

**3. Grid**
- [ ] png: a 1,000,000-row stub result renders in under 16 ms per frame (benchmarked, baseline
      recorded) and draws exactly `visible_rows` rows; the frame after `Ctrl+End` shows row
      1,000,000 in the gutter and the summary unchanged.
- [ ] png: a row with an integer, a text, `NULL`, `true`, a JSON value, a 20-byte blob, a vector
      and a TTL draws each cell as 3.2 says, right- and left-aligned at the column's padding.
- [ ] tree: after `Right`, `Down`, `Shift+Right`, `Shift+Down` the grid reports a selection of
      2 × 2 with the current cell at row 3, column 3; `Esc` collapses it; `Tab` leaves the grid.
- [ ] clipboard stub: `Ctrl+C` on that selection yields `1\tAda\n2\tGrace\n` (NULL as empty, a
      tab inside text escaped); `Ctrl+Shift+C` prefixes the header; `Ctrl+Shift+I` yields two
      `INSERT INTO customers (id, name) VALUES (1, 'Ada');` lines with a `'` doubled.
- [ ] png: a 100,001-row stub shows the banner at `(260, 418, 1020, 28)` with its text, the grid
      header at y 446, and the summary `100,000 rows shown of 100,001 · …`; tree: the banner is a
      polite `status`.

**4. Object browser and inspector**
- [ ] png: `shop` with its three tables, one view, three indexes, two sequences, the default key
      space and one collection draws the groups in 4.1's order with their counts; a filter `ord`
      leaves `Tables 1 of 3` and `orders` visible and hides `customers`.
- [ ] tree: `Down` × 2 then `Right` expands `Tables`; `Enter` on `orders` opens tab `Query 2`
      with `SELECT * FROM orders LIMIT 100;` run; `Shift+Enter` inserts `orders` at the editor's
      caret; typing `pr` moves to `products`; each row reports level, posinset, setsize and
      expanded or collapsed.
- [ ] png at 1440 × 800: the inspector open at `(1160, 44, 280, 732)` with `orders`, `Table · 3
      rows`, `Columns (3)`, `Indexes (1)`, the DDL box and 3 sample rows; png at 1280 × 800: it is
      closed and `Ctrl+I` draws it as an overlay with the shadow; tree: the blocks are
      expandable buttons, the DDL read-only text.
- [ ] png: `Drop…` from the menu shows the dialog with the `DROP TABLE orders;` statement in
      `code`, `Cancel` as the default; tree: the dialog is modal and `Tab` stays inside it.

**5. Plan**
- [ ] png: `Ctrl+E` on `SELECT * FROM customers WHERE id = 2` shows the `Plan` segment active and
      one node `Index Scan using customers_pkey on customers` centred at the top of the results
      body with a `cost` row when the engine gives costs and none when it does not.
- [ ] png: a stub three-node plan (root 100 % total, children with own cost 62 % and 20 %) draws
      the links, the bars, the `error` 2 px border and `· 62 %` on one child, `warning` and
      `· 20 %` on the other, and the root plain; under `ANALYZE` the rows show `ms`, `rows` and
      `buffers`, and a 10× misestimate adds `est. 10 → 1,204`.
- [ ] tree: `Down`, `Right`, `Enter` selects the second child, its node reports `hot` in its
      description, and the editor's highlight covers the statement's range; `Esc` clears it.

**6. Pointer and window**
- [ ] png: a hover frame over `Run all` fills `bg2`; a pressed frame `bg2` with `borderStrong`; a
      click-release outside the button changes nothing; a click at `(600, 90)` in the editor puts
      the caret at the clicked column and draws no ring.
- [ ] png: frames at 0, 400, 1,100 and 1,700 ms after a key show the caret on, on, off and on
      (solid for 500 ms, then on until 1,030 ms, off until 1,560 ms); under reduced motion all
      four are on; with the window inactive the caret is `textDisabled` and solid.
- [ ] png: at 800 × 560 the sidebar is the 36 px rail, the editor at least 80 tall and the results
      at least 120; at 1024 × 700 the inspector is closed; at 1440 × 800 it is open; after a
      resize from 1280 to 1440 the grid's first visible row and current cell are unchanged.

**7. Accessibility**
- [ ] tree: the main window's dump has exactly the nodes of section 7's table for the golden state
      (toolbar with four buttons and the combo box, filter, tree with its rows, tab list with one
      tab, editor, result tabs, status line, grid with 3 rows and 2 columns plus the gutter, status
      bar) with those names, roles, patterns and states, in that order.
- [ ] tree: every button's accelerator matches gui.md's shortcut table for the platform.
- [ ] tree: after a run the status line's live region reads `3 rows, 0.4 milliseconds`; after an
      error the strip is an `alert` with the strip's text; selecting 2 × 2 cells raises
      `2 rows by 2 columns selected`.
- [ ] browser: the same dump over the ARIA DOM in a headless browser, and `Tab` through the page
      visits toolbar, filter, tree, tab, editor, result tabs, grid, status bar in that order.
- [ ] contrast: `warning` on `bg1`, `error` on `bg1`, `borderControl` on `bg0` and the plan's
      `accent` bar on `bg2` pass 4.5:1 (text) or 3:1 (non-text) in all three themes, computed from
      the `Theme` values; gui.md's `warning` light value becomes `#995A00` with this slice.

## Decided by this design
- Copy as `INSERT` is `Ctrl/Cmd+Shift+I`, not the brief's `Shift` variant, so `Ctrl/Cmd+Shift+C`
  can be copy-with-header as people expect from spreadsheets and from `\copy`.
- The error code shown is the engine's SQLSTATE (`42P01`), the only code the engine has today;
  the brief's `E2104` form is not introduced. The CLI, MCP and the GUI show the same code.
- The server URL is libpq's form because `eggdb serve` speaks the PostgreSQL protocol (cli.md 14,
  29); `eggdb://` is an alias of it so the brief's example keeps working.
- `Browse…` exists only where the OS has a file dialog the platform layer can call without
  third-party code (macOS, Windows); Linux and the browser type or drop the path (D1, D8).
- The inspector opens by default only from 1440 px, so gui.md's 1280 × 800 golden frame is
  unchanged by this slice.
- Hot-node thresholds are 20 % and 50 % of the root's total, chosen so a plan with four equal
  nodes (25 % each) reads `warm` everywhere and nothing `hot`, and one dominant node is `hot`.

## 9. More tabs, the kept session, the value view, the grid menu, column widths and scrollbars

Designed by Claude Fable for D310. Pins down what sections 2 and 3 leave open. Rects are at the
1280 × 800 golden frame; tokens, components and roles not restated are gui.md's. The golden frame
(one tab `Query 1`, 140 wide, `+` at `(408, 48, 28, 28)`, no scrollbars) is unchanged by all of it.

### 9.1 The tab strip with several tabs
Tabs are `w` wide, side by side from x 260: `w = 140` while `n × 140 + 36 ≤ 1020` (up to 7 tabs),
then `w = max(100, floor(984 / n))` (8 tabs 123, 9 tabs 109, 10 or more 100); from 10 tabs the
strip scrolls. While it fits, `+` sits 8 px after the last tab, `(260 + n × w + 8, 48, 28, 28)`.
While it scrolls, `+` is pinned at `(1244, 48, 28, 28)` behind a 1 px `border` line at x 1236, and
the tabs draw clipped in the viewport `(260, 44, 976, 36)` with scroll offset `s`: tab `i` (0-based)
is at `x = 260 + i × w − s`. On adding, closing or activating a tab, `s` moves only as far as needed
to show the whole active tab (`s = i × w` when it is cut on the left, `s = (i + 1) × w − 976` on
the right); the wheel moves `s` 100 px per notch (either axis), clamped to `0 … n × w − 976`; `s`
returns to 0 when the tabs fit again. No scrollbar: the cut tab at each edge is the cue.

| Tab | Fill | Label at `(x + 12, 53)`, cut with `…` at `x + w − 32` | Close icon `(x + w − 20, 58, 8, 8)`, hit `(x + w − 28, 50, 24, 24)` | Between tabs |
|-----|------|------|------|------|
| active | `bg0`, stripe `(x, 44, w, 2)` `accent`, no bottom border | `body` `textPrimary` | always | none |
| inactive | `bg2` (the strip), hover `bg1` | `body` `textSecondary` | only while hovered (the unsaved dot d 6 always) | 1 px `border` at `(x + w − 1, 52, 1, 20)` between two inactive tabs |

Pointer: click activates; a click in the close hit closes; middle-click closes; a double-click on the
strip's empty part (after `+`, or in the viewport past the last tab) adds a tab. Keys (EggDB layout):
`Ctrl/Cmd+T` adds `Query n` (the lowest unused number) right after the active tab, with the active
tab's dialect and connection, activates it and focuses its empty editor; `Ctrl/Cmd+W` and `Ctrl+F4`
close the active tab; `Ctrl+Tab` / `Ctrl+Shift+Tab` go to the next / previous tab and wrap. After a
close the tab to its right becomes active, else the one to its left. Closing the last tab leaves one
empty `Query 1` (no text, no result, blank summary), so the strip is never empty and the window
returns to the golden frame's state. A running tab (2.3) is never closed silently: every close path
first asks the `Stop` dialog below; `Keep running` leaves it running.

**Confirm dialogs** (`Close Query 2 without saving?` from 2.1, and the two below) share one shape,
with the layout picker's conventions: the `scrim` covers the window and takes no input; the dialog
opens and closes instantly, centred in the whole window.

| Item | Rect (relative to `dx = round((W − 420) / 2)`, `dy = round((H − 160) / 2)`: `(430, 320)` here) | Drawing |
|------|------|------|
| dialog | (0, 0, 420, 160) | `bg0`, 1 px `border` inside, `radiusCard`, no shadow |
| title | text at (24, 20) | `title` `textPrimary` |
| body | text at (24, 52), wrapped at words to 372 wide, at most two lines of 18 | `body` `textPrimary` |
| buttons | y 108, h 28, right edge at x 396, 8 px apart, width `max(72, text + 24)`; the default button is rightmost and primary, the other secondary | |

One dialog per close, by priority running > transaction > unsaved, its body naming the rest:

| Case | Title | Body | Buttons (default last) |
|------|-------|------|------|
| running | `Stop Query 2 and close it?` | `The running statement is cancelled and the rows fetched so far are discarded.` + ` Its open transaction rolls back.` + ` Its text is unsaved.` as they apply | `Stop and close` / `Keep running` |
| transaction open or failed | `Close Query 2 and roll back?` | `Query 2 has an open transaction. Closing rolls it back; nothing from it is kept.` + ` Its text is unsaved too.` | `Roll back and close` / `Keep` |
| unsaved | `Close Query 2 without saving?` | `Its text is not saved anywhere.` | `Close` / `Keep` |

Focus opens on the default button; `Tab` is trapped and cycles the two buttons; `Enter` activates the
focused button, `Esc`, a click on the scrim and the window's close key are `Keep`. Role `alertdialog`
(AXSheet / Window modal / dialog), named by the title, described by the body, the default button named.

### 9.2 The kept session
A tab owns one engine session, opened at its first run and closed when the tab closes (with a
`ROLLBACK` if a transaction is open), so `BEGIN` in one run holds for the next run in that tab, and
no other tab sees its uncommitted work. The engine lets one session hold the write transaction at a
time; a second tab's writer gets the engine's error, shown as any error (2.5), nothing more.

| State | Tab | Status bar (left group, 16 px after the tags) |
|-------|-----|------|
| transaction open | dot d 6 `warning` at `(x + 12, 59)`, label moves to `x + 24` | `Transaction open` `small` `warning` |
| transaction failed (the engine reports the block aborted after an error) | dot `error` | `Transaction failed: ROLLBACK to continue` `small` `error` |
| another tab holds a transaction | | `Transaction open in Query 2` `small` `textSecondary`; click activates that tab |

The status item is a button: click, `Space` or `Enter` open a two-row popover in the dialect menu's
style (160 wide, rows 28, `bg0`, 1 px `border`, `radiusPopover`, shadow 0 4 12 at 16 %), above the
item with a 4 px gap: `COMMIT`, `ROLLBACK` (`COMMIT` disabled while failed). Picking one runs that
statement in the tab as a normal run, so the summary, `Messages` and the `Committed` toast follow
2.3 and 2.4. The tab's accessible name adds the state text: `Query 2, transaction open`,
`Query 2, transaction failed`.

The dialect pill refuses a switch while the tab's transaction is open or failed: its menu opens with
every other dialect disabled (`textDisabled`), and opening it shows the toast
`Warning: COMMIT or ROLLBACK first; Query 2 has a transaction open` for 8 s. The session's dialect
never changes mid-transaction, so the status bar's dialect item and the engine always agree.

### 9.3 The full-value view
`Enter`, `F2` or a double-click on a cell opens a popover 480 × 240: `bg0`, 1 px `border`,
`radiusPopover`, shadow 0 4 12 at 16 % black, at `x = clamp(cell.x, 260, 800)`, `y = cell.bottom + 4`,
or `y = cell.y − 244` when `cell.bottom + 244 > 776`. Rects relative to its top-left:

| Item | Rect | Drawing |
|------|------|------|
| header | (0, 0, 480, 32) | `bg1`, 1 px `border` at y 31 |
| column and type | text at (12, 7), cut with `…` at x 404 | `bodyBold` `textPrimary` for the name, then ` · ` and the type in `body` `textSecondary` (`name · text`, `data · bytea · 184 bytes`) |
| `Copy` | (412, 2, 56, 28), label at (424, 7) | ghost button; disabled for `NULL` and the empty string |
| body | (0, 32, 480, 208); text at (12, 40), lines 20, wrapped per character at 456 px (58 glyphs with the pinned font) | `code` `textPrimary`, read-only, no partial selection |
| scrollbar | track (470, 32, 10, 208) | 9.6 |

Shown: text verbatim with its line breaks; JSON and arrays pretty-printed with 2-space indent and keys
in stored order; a vector as a JSON array, one value per line; a blob as `0x` + the full hex, no
spaces; `NULL` as `NULL` and the empty string as `""`, both `textDisabled`. Past 1 MB the body stops
and its last line is `… and 2.1 MB more` in `textSecondary`. The body scrolls with the wheel (60 px),
`Up` / `Down` one line, `PgUp` / `PgDn` nine lines, `Home` / `End` to the ends. `Ctrl/Cmd+C` and
`Copy` copy the body's full text as shown (the whole value, past the 1 MB cut) and toast
`Copied 1,204 characters` or `Copied 184 bytes as hex` for 2 s. `Esc`, `Enter`, `F2` and a click
outside close it; focus returns to the cell, which stays current. Role: a non-modal dialog (AXPopover /
Window IsDialog / dialog / `dialog` `aria-modal=false`) named `<column> value`, its body a read-only
`textbox` named `Value` (focused on open), the button `Copy value`; `Tab` cycles body → `Copy`.

### 9.4 The grid's right-click menu
Opens at the pointer (top-left at the pointer, moved left to end at x 1280 and flipped above the
pointer when it would cross y 776) or, from `Shift+F10` and the menu key, below the current cell with
a 4 px gap, left-aligned to it, with the same clamping. A right-click outside the selection first makes
that cell current and collapses the selection; inside it keeps the selection; on the gutter or a header
it selects that row or column first. Popover 240 wide, `bg0`, 1 px `border`, `radiusPopover`, shadow
0 4 12 at 16 %, 4 px padding top and bottom, rows 28 (label `body` `textPrimary` at x 12, shortcut
`body` `textSecondary` right-aligned at x 228), separators 1 px `border` across the width with 4 px
above and below; height 278. Highlighted row: `selection` rounded r 4 inset 4 px horizontally;
disabled `textDisabled`.

| Group | Items (shortcut) |
|-------|------|
| copy | `Copy` (`Ctrl/Cmd+C`), `Copy with header` (`Ctrl/Cmd+Shift+C`), `Copy as INSERT` (`Ctrl/Cmd+Shift+I`), `Copy as CSV`, `Copy as JSON` |
| value | `Open value` (`Enter`) |
| select | `Select row` (`Shift+Space`), `Select column` (`Ctrl/Cmd+Space`), `Select all` (`Ctrl/Cmd+A`) |

From the keyboard the first item is highlighted, from the pointer none. `Up` / `Down` move without
wrapping, a letter jumps to the next item starting with it (`C` cycles the copy items), `Enter` and a
click activate, `Esc` and a click outside close; focus returns to the grid. Role `menu` of
`menuitem`s, each with its shortcut as the accelerator.

`Copy as CSV` is the selection as RFC 4180: a header row of the selected columns' names, fields joined
by `,`, records by `\r\n` with no trailing one; a field holding `,`, `"`, `\r` or `\n` is quoted with
`"` doubled; `NULL` is empty; numbers, booleans, dates and uuids are the cell's text; JSON is its
compact text (quoted as needed); a blob is `0x` + its full hex. `Copy as JSON` is a pretty-printed
(2-space) array of one object per selected row, keys the column names in column order (a repeated
name gets `_2`, `_3`); integers, decimals and floats bare; booleans `true` / `false`; `NULL` `null`;
JSON and arrays embedded as JSON; text, dates, times, uuids, money and intervals as strings; a blob
as the string `0x…`; a vector as an array of numbers. Toasts: `Copied 12 cells as CSV`,
`Copied 3 rows as JSON`, 2 s.

### 9.5 Column widths
A header separator's hit band is 6 px (`x − 3 … x + 2`); hover draws a 2 px `accent` line over the
separator for the header's height and the column-resize cursor; a drag extends the line through the
body and resizes live, min 48, max 960. The gutter's edge at x 307 is not a handle. Double-click on
the band auto-fits: the wider of the header and the first 200 fetched rows, clamped 48–480 (gui.md's
reset). While dragging, the status bar's left group shows `name · 240 px`. Widths belong to the tab:
a later run whose columns have the same names in the same order keeps them, any other result resets
them. Keyboard: `Alt+Left` / `Alt+Right` change the current cell's column by 8 px (`Shift` 32 px) and
`Alt+Enter` auto-fits it; each announces `name, 240 pixels` politely, and the header exposes
`width 240 px` as its description.

### 9.6 Scrollbars
A region whose content fits its viewport draws no scrollbar, so the golden frame has none. Tracks are
10 px overlays at the region's edge; gutters never scroll horizontally; the grid header scrolls with
its columns.

| Region | Vertical track | Horizontal track |
|--------|------|------|
| grid body | (1270, 446, 10, 330); h 320 while the horizontal one shows | (308, 766, 962, 10) |
| editor | (1270, 80, 10, 300); h 290 with the horizontal one; both 28 shorter / higher while the error strip shows | (308, 370, 962, 10) |
| full-value view | (470, 32, 10, 208) relative to the popover | none (the text wraps) |
| tab strip | none (wheel only, 9.1) | none |

For a track of length `L`, viewport `V`, content `T` and offset `O`: `thumb = max(24, round(L × V / T))`
and `pos = round((L − thumb) × O / (T − V))`; the thumb is 6 px wide inset 2 px in the track,
`radiusPill`, `textDisabled` at 60 % alpha; hover and drag `textSecondary`, 8 px wide inset 1 px.
Shown while the pointer is over the region and for 800 ms after the last scroll or drag, then fading
200 ms; under reduced motion the thumb is always visible while the content exceeds the viewport,
never faded. A press on the track pages one viewport toward the pointer at once; dragging the thumb
sets `O = (pointer − grab − trackStart) / (L − thumb) × (T − V)`, clamped, and the drag continues
outside the track. Roles: `scrollbar` → AXScrollBar / ScrollBar, RangeValue / scroll bar, Value /
`scrollbar` with `aria-controls` the region and `aria-orientation`; named `Vertical scrollbar` /
`Horizontal scrollbar` as children of the region; value `0` to `100` percent; not in the `Tab`
order, since the region's own keys scroll it.

### Decided by this design (9)
- Tabs shrink from 140 to 100 before the strip scrolls (from 10 tabs); while scrolling `+` is pinned
  at the strip's right behind a 1 px line, and the only scroll cue is the cut tab at each edge.
- Closing the last tab leaves an empty `Query 1`; the strip is never empty.
- Every close path on a running tab goes through the `Stop Query 2 and close it?` dialog; closes
  show one dialog, by priority running > transaction > unsaved, whose body names the other reasons.
- Confirm dialogs are 420 × 160, centred in the window, default button primary and rightmost.
- An open transaction shows as a `warning` dot in the tab plus a `Transaction open` status item whose
  popover offers `COMMIT` and `ROLLBACK`; a failed block reads `Transaction failed: ROLLBACK to
  continue` in `error`; other tabs see `Transaction open in Query 2`.
- The dialect pill refuses a switch while a transaction is open (other dialects disabled, a
  `Warning:` toast), rather than letting the next run fail.
- The value view pretty-prints JSON (2-space), shows the full hex of blobs, cuts the body at 1 MB,
  and copies what it shows; it has no partial text selection in this slice.
- `Copy as CSV` (RFC 4180, header row, `\r\n`, no trailing break) and `Copy as JSON` (array of
  objects, numbers bare, `NULL` as `null`) join the menu; `Select all` joins the select group.
- Menu items do not wrap at the ends, as in the picker, tree and grid.
- Column widths belong to the tab and survive runs with the same column names in the same order;
  the drag's max is 960; `Alt+Left` / `Alt+Right` / `Alt+Enter` are the keyboard path.
- Under reduced motion scrollbar thumbs stay visible rather than hiding after 800 ms, so people who
  turn animation off never lose the position cue.
