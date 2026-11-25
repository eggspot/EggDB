# Import, export, backup and restore

Designed by Claude Fable (D15) for roadmap M6e: the import wizards (CSV, JSON lines, Parquet, SQL
dumps, SQLite files, mongodump, Redis AOF; D13, D42), export from a result or a table, and the
backup and restore screens, all driven by the engine's own commands (`eggdb import`, `eggdb
export`, `eggdb backup`, `eggdb restore`, cli.md) so the GUI shows the command it runs
(client-brief.md principle 2) and the Jobs tray of client-brief.md 6 holds the long ones. Every
token, rect and role not restated is [gui.md](gui.md)'s and [workspace.md](workspace.md)'s; the
page-tab shape is [admin-screens.md](admin-screens.md) section 1's. Rects are at 1280 × 800.

## 1. Where they live
| Entry | Opens |
|-------|-------|
| `File > Import…` (the `more` menu, layouts-contents.md 6), `Ctrl/Cmd+Shift+O`, the tree's connection or `Tables` row menu `Import…`, a dropped file that is not `.eggdb` (`.csv`, `.jsonl`, `.ndjson`, `.parquet`, `.sql`, `.sqlite`, `.db`, `.aof`, a `mongodump` directory or `.bson`) | the Import page tab, section 2 |
| the results header's `Export…` (gui.md), the grid menu's `Export…`, a table row's menu `Export…`, `Ctrl/Cmd+Shift+E` in the grid | the Export dialog, section 3 |
| `File > Back up…`, the connection row's menu `Back up…` | the Backup dialog, section 4 |
| `File > Restore…`, the connection row's menu `Restore…`, a dropped backup file (`.eggdb.bak`, a `pg_dump` plain file is an import) | the Restore page tab, section 5 |
| the status bar's `Jobs` item | the Jobs popover, section 6 |

## 2. The Import wizard
A page tab titled `Import` (one per connection; a second `Import…` activates it). Its page is a
wizard: a steps header, a body per step, a footer with the buttons.

| Item | Rect | Drawing |
|------|------|---------|
| steps header | (260, 80, 1020, 44), `bg1`, 1 px `border` below | four steps as text at y 93, 32 px apart from x 284: `1 Source`, `2 Shape`, `3 Target`, `4 Run`; the current step `bodyBold` `textPrimary` with a 2 px `accent` stripe under it `(x, 122, w, 2)`; done steps `body` `textPrimary` with a check icon before them; later steps `textSecondary` |
| body | (260, 124, 1020, 608) | the step's controls from (284, 148), 12 px gaps, labels `body` above inputs |
| footer | (260, 732, 1020, 44), `bg1`, 1 px `border` above | left: the command line it will run in `code` `textSecondary` at (284, 745), cut with `…` at x 900 (`eggdb import shop.eggdb --csv orders.csv --table orders --header`), a ghost `Copy` after it; right: `Back` secondary (1092, 740, 80, 28) and `Next` primary (1180, 740, 88, 28), `Import` on step 3, `Done` on step 4 |

**Step 1, Source.** A `File` input (284, 170, 760, 28) with `Browse…` (1052, 170, 84, 28) where
the platform has a dialog, and a drop target `(284, 212, 852, 120)`: a 1 px dashed `border`
rounded rect (`radiusCard`; dashes 4 on 4) with `Drop a file here` centred in `body`
`textSecondary` (the ring turns 2 px `accent` while a drag is over it, as 1.3). Under it the
detected format as a row of radio buttons (16 × 16 circles, `accent` dot when chosen) with
labels: `CSV`, `JSON lines`, `Parquet`, `SQL dump`, `SQLite file`, `mongodump`, `Redis AOF`; the
detection reads the first 64 KB (and a directory's `.bson` files) and pre-selects, the user may
override; `SQL dump` adds a `Dialect` dropdown (`Detect`, `PostgreSQL`, `MySQL`, `SQL Server`,
`Oracle`, `SQLite`) with the detected one named in its label (`Detect (MySQL)`). `Encoding`
dropdown (`UTF-8`, `UTF-16`, `Latin-1`, `Windows-1252`) for text formats, detected from a BOM
or the bytes. `Next` is disabled until a readable file is chosen; an unreadable one shows 1.6's
error lines under the input (`Error: cannot read orders.csv: permission denied`).

**Step 2, Shape** (text formats and Parquet; a dump, a SQLite file, mongodump and an AOF skip
to step 3 with what they contain listed instead):
- CSV options in a row of controls at y 148: `Delimiter` (dropdown `,`, `;`, `Tab`, `|`,
  `Other…` with a 1-character input), `Quote` (`"`, `'`, `none`), `Header row` checkbox, `NULL
  word` input (default empty: an empty field is `NULL`; `\N` for a `COPY` file), `Skip lines`
  number input, `Trim spaces` checkbox; JSON lines has `Nested objects` (`as JSON columns` /
  `flatten with .`); Parquet has nothing.
- A preview grid `(284, 196, 972, 300)` of the first 100 rows read with the options, 3's grid
  with the detected column names as headers and, under each header, a 24 px row of type
  dropdowns (`text`, `integer`, `bigint`, `numeric`, `double`, `boolean`, `date`, `timestamp`,
  `timestamptz`, `json`, `uuid`, `bytea`) pre-filled by sampling the 100 rows (the narrowest type
  every sampled value fits), plus a `Skip` checkbox per column; the header's name is an editable
  input (the target column's name). Cells that do not fit the chosen type draw `errorFill`
  (grid-editing.md 9) with the count `3 of 100 rows do not fit integer` under the grid in `body`
  `warning`. `Reset` (ghost) restores the detection.

**Step 3, Target.** `Table` as a radio pair: `New table` with a name input (pre-filled from the
file's stem, lower-cased, non-identifier characters to `_`) and `Existing table` with a dropdown
of the catalog's tables; for an existing table a mapping grid `(284, 240, 972, 240)` with
columns `Source column`, `→`, `Target column` (dropdown of the table's columns or `skip`), `Type`
(the target's, read-only), matched by name first. Below: `On conflict` radio (`Fail`, `Skip
rows`, `Replace rows`; the last only when the table has a key) and `Batch` (`1,000 rows per
transaction`, `10,000`, `One transaction`). A dump shows instead the objects it creates as a
tree with checkboxes (`Tables (12)`, `Views (2)`, `Data`, `Roles` when `--roles` applies) and
`Into` (`this database` / `a new file…`); a SQLite file lists its tables with checkboxes;
mongodump its collections; an AOF the key count. `Import` runs.

**Step 4, Run.** A progress block at (284, 148): `body` `textPrimary` `Importing orders.csv into
orders`, under it a 6 px bar `(284, 176, 852, 6)` `bg2` with `accent` filled by bytes read / file
size (`radiusPill`; polish.md 7.3's sweeping form when the size is unknown), then `small`
`textSecondary` `12,400 rows · 3.2 MB of 18.1 MB · 4.1 MB/s · 2 s left`, updated every 250 ms;
`Stop` (secondary) ends it, keeping what was committed and saying so. Errors: a rejected row
under `Fail` stops with 2.5's strip form at (284, 200): `Error 22P02: invalid input syntax for
type integer: "abc" · line 1,204` and `Show line` (opens the value view on the raw line); under
`Skip rows` the count `Skipped 3 rows` and a ghost `Show skipped` listing them in a small grid
with the reason. Done: `Imported 12,400 rows into orders · 4.2 s`, the catalog refreshes, the
tree selects the table, `Open` (primary) runs its `LIMIT 100` tab, `Done` closes the page.
Leaving the page (`Ctrl/Cmd+W`, another tab) during a run moves it to the Jobs tray (section 6)
and does not stop it.

Keys: `Tab` through the step's controls then `Back`, `Next`; `Enter` on any control is `Next`
(or `Import`); `Esc` asks `Cancel the import?` only while running, else closes the page;
`Alt+1` … `Alt+4` jump to a done step. Roles: the page a `region` named `Import`; the steps
header a `list` of `listitem`s named `Step 1 of 4, Source`, `current` on the active one; the drop
target a `button` named `Drop a file or browse`; the preview a `grid` named `Preview` whose
header cells hold the name `textbox` and type `combobox`; the progress bar a `progressbar`
named `Import progress` with value 0–100 (or indeterminate); the footer's command a read-only
`textbox` named `Command`; progress text a polite `status` read at most every 5 s.

## 3. The Export dialog
9.1's dialog shape at 480 × 360, `dx = round((W − 480) / 2)`, `dy = round((H − 360) / 2)`:

| Item | Rect (relative) | Drawing |
|------|------|---------|
| title | text (24, 20) | `title`: `Export results` (`Export orders` from a table row; `Export selection` when cells were selected) |
| `Format` | label (24, 56); radio row at y 78 | `CSV`, `TSV`, `JSON`, `JSON lines`, `Parquet`, `SQL INSERT`, `XLSX` (16 × 16 radios, labels at +22, 24 px apart from the previous label's end) |
| options | from y 114 | CSV/TSV: `Header row` checkbox, `Delimiter`, `NULL as` input; JSON: `Pretty` checkbox; SQL: `Dialect` dropdown and `Rows per INSERT` (`1`, `100`); Parquet: `Compression` (`zstd`, `snappy`, `none`); XLSX: `Sheet name` input |
| `Rows` | label (24, 190); radio row at y 212 | `Fetched rows (12,400)` / `The whole query (re-run without the grid's 100,000 limit)` / `Selection (4 × 3 cells)` (the last only with a selection; a table row's export has `Whole table (2.1M rows)` instead) |
| `File` | label (24, 256); input (24, 278, 348, 28); `Browse…` (380, 278, 76, 28) | pre-filled `~/Downloads/orders.csv` (the table's or `result`'s name, the format's extension); in the browser the file downloads and the input is the name only |
| buttons | y 308 | `Cancel` (300, 308, 76, 28), `Export` (384, 308, 72, 28) primary and default |
| command | `code` `textSecondary` at (24, 316), cut at x 290 | `eggdb export shop.eggdb --table orders --csv > orders.csv` |

`Export` streams through the engine (`eggdb export`'s writers; a result set from the tab's
session re-run with the chosen rows), closes the dialog and adds a job to the tray (section 6);
a small export (under 1 s) just toasts `Exported 12,400 rows to orders.csv` with an `Open
folder` button (`Show in Finder`, `Show in Explorer`, the file manager through `xdg-open` on
Linux). Focus opens on the `Format` radio; `Tab` is trapped; `Esc` cancels. Role `dialog`
(modal), the radio rows `radiogroup`s named `Format` and `Rows`.

## 4. The Backup dialog
9.1's shape at 480 × 288: title `Back up shop`; `To` label and input `(24, 78, 348, 28)` with
`Browse…`, pre-filled `~/Backups/shop-2026-11-25-1412.eggdb.bak`; `Compress` checkbox (default
on, `--compress`); `Verify after writing` checkbox (default on: the engine re-reads the copy and
checks every checksum, `eggdb check` on the copy); a `small` `textSecondary` line `Live: the
file keeps working during the backup; the copy is the state at its start.`; `Cancel` and `Back
up` (primary, default) at y 236; the command line at (24, 244): `eggdb backup shop.eggdb
~/Backups/shop-….eggdb.bak --compress`. On a server the dialog reads `Back up db.example.com`
and the `To` path is on the client, streamed over the connection (`eggdb backup` through the
server's backup endpoint). Running backups are tray jobs with `184 MB of 1.2 GB · 48 MB/s`; a
finished one toasts `Backed up shop · 1.2 GB · 26 s` with `Open folder`. A failure (disk full,
permission) is the engine's line in the tray's job row and a toast `Error: cannot write …`.
Scheduled backups and point-in-time recovery (D24) are admin-console screens (console.md),
not this dialog.

## 5. The Restore wizard
A page tab `Restore`, the Import wizard's shape with three steps: `1 Source`, `2 Into`, `3 Run`.

- **Source:** the file input, drop target and detected kind: `EggDB backup` (`.eggdb.bak`),
  `SQL dump` (any dialect, detected), `SQLite file`, `mongodump`, `Redis AOF` or `RDB`; a
  backup shows its header (`shop · 1.2 GB · taken 2026-11-25 14:12 · version 0.41`) and a
  `Verify` ghost button (checksums, with a progress bar). Formats D13 lists as "through a
  documented route" (`.bak` from SQL Server, `pg_dump` custom, Data Pump) are recognised by
  content and refused with the CLI's words: `Error: SQL Server .bak files have no public
  specification` and `Try: script the database from SSMS (Generate Scripts) and import the .sql
  file`, with a `Learn more` link to the docs.
- **Into:** radio `A new file` (path input, pre-filled beside the source) / `This database
  (shop)`; the second shows a `warning` line `Restoring into shop replaces its objects that the
  backup also holds.` and, for an EggDB backup, `Replace everything` (the file is overwritten
  by the copy, after closing every tab: the body says so). For a dump, step 2 also lists the
  objects with checkboxes as the Import wizard's step 3 does, plus `Roles` (`--roles`) and
  `Stop at the first error` / `Continue and report` radios.
- **Run:** the progress block (`CREATE TABLE orders · 12 of 48 statements`, or bytes for a
  backup), `Stop`, the error strip with `Show statement`, and at the end `Restored shop from
  shop-….eggdb.bak · 26 s` (or `Restored 48 statements, 3 skipped · 1.2 s` with `Show skipped`);
  when a new file was made, `Open` connects to it (replacing the connection after asking to
  close tabs with 9.1's dialogs); the catalog refreshes otherwise.

The footer's command: `eggdb restore shop.eggdb shop-….eggdb.bak` / `eggdb restore new.eggdb
dump.sql --dialect mysql`. Roles as the Import wizard's with `Restore` as the names.

## 6. The Jobs tray
A status bar item `Jobs · 2` (right group, before the encoding; hidden while no job ran in this
session) with a 12 px spinner (polish.md 7.3) while any job runs, else the check icon; click,
`Space` or `Enter` open a popover above it (9.4's style, 420 wide, rows 44, at most 8, newest
first): each row `bodyBold` the job (`Import orders.csv → orders`, `Export orders.csv`, `Back up
shop`, `Restore shop`), under it `small` `textSecondary` the progress (`12,400 rows · 3.2 MB of
18.1 MB · 2 s left`, `Done · 4.2 s`, `Stopped · 8,000 rows kept`, `Error: …`), right: a 120 × 4
progress bar (`radiusPill`) or the result's ghost button (`Open`, `Open folder`, `Show error`),
and a `Stop` ghost while running. `Up` / `Down`, `Enter` on a row activates its button, `Delete`
removes a finished row, `Esc` closes. Finishing a job toasts (`Imported 12,400 rows into
orders` with `Open`) for 8 s whether or not the tray is open; the toast is a polite `status`
and an error one an `alert`. Closing the window with jobs running asks 9.1's `Stop 2 jobs and
quit?` (`Stop and quit` / `Keep working`, default `Keep working`). Role: the item a `button`
named `Jobs, 2 running`, the popover a non-modal `dialog` named `Jobs` holding a `list` of
`listitem`s with a `progressbar` each.

## 7. Acceptance checklist
- [ ] png: dropping `orders.csv` on the main window opens `Import` on step 1 with `CSV` chosen
      and `Next` enabled; step 2 shows the preview grid with detected types (`id` integer,
      `total` numeric, `created_at` timestamp), a cell `abc` in `id` on `errorFill` and the
      count line; step 3 pre-fills `orders` as a new table; the footer reads `eggdb import
      shop.eggdb --csv orders.csv --table orders --header`.
- [ ] png: step 4 at 1 s of a slow stub shows the bar at the right fraction and the rate line;
      `Stop` leaves `Stopped · 8,000 rows kept`; a stub `22P02` on line 1,204 under `Fail` shows
      the strip with `Show line`; under `Skip rows` the `Skipped 3 rows` line and the list.
- [ ] png: a `mysqldump` file is detected as `SQL dump` with `Detect (MySQL)`; step 3 lists its
      tables with checkboxes; a SQL Server `.bak` is refused with the `Try:` line.
- [ ] png: the Export dialog from a 12,400-row result at `(400, 220, 480, 360)` with `CSV`,
      `Fetched rows (12,400)` and the pre-filled path; `Parquet` shows `Compression`; a selection
      adds the third radio; tree: two `radiogroup`s, the dialog modal with `Export` default.
- [ ] png: the Backup dialog with the dated path and the two checkboxes; a 1.2 GB stub shows the
      tray item `Jobs · 1` with the spinner and the popover row with its bar; the finish toast
      with `Open folder`.
- [ ] png: the Restore wizard on an `.eggdb.bak` shows its header line and `Verify`; `Into: This
      database` shows the `warning` line; step 3's end offers `Open` for a new file.
- [ ] tree: the wizard's steps are `listitem`s with `current` on step 2; the progress bar is a
      `progressbar` with its value; the Jobs popover a `dialog` with a `list`.

## Decided by this design
- Import and Restore are wizards in page tabs with a steps header and a footer that always
  shows the `eggdb` command they run; Export and Backup are dialogs; long runs go to a Jobs
  tray in the status bar.
- Formats are detected by content and may be overridden; types are sampled from the first 100
  rows and shown as dropdowns under the preview's headers; misfits are painted `errorFill` and
  counted.
- D13's "documented route" formats are recognised and refused with the CLI's `Try:` text.
- Export offers fetched rows, the whole query without the grid's limit, or the selection; in
  the browser the file downloads.
- Backups verify by default and say the copy is the state at the start; restoring into the
  open database warns and, for a full backup, closes every tab first.
- Leaving a wizard during a run moves the job to the tray; quitting with jobs running asks.
