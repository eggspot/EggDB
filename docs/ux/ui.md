# `eggdb ui`: the terminal UI (design, v0.2.0)

Designed by Claude Fable (D15, roadmap items 14 and 15). `eggdb ui <file>` opens one database
file full screen in the terminal to browse and edit its keys. In v0.2.0 a database is one ordered
key space of byte strings (D87); the layout reserves a place for the collections sidebar that
comes with named collections. Everything here is implementable with `std` alone (D1): our own
terminal layer, our own JSON formatter, our own character-width table.

The UI is a thin client of the embedded API ([embedded.md](../embedded.md)): every screen is
drawn from `Database::stats`, short read transactions (`scan`, `scan_prefix`, `get`) and
`Database::check`; every change is one write transaction (`try_begin_write`, `put` or `delete`,
`commit`). The UI never holds a transaction between two key presses, except while a commit runs.

## Starting
```
eggdb ui app.db [--read-only] [--cache 64M] [--ascii] [--mono]
```
- Both stdin and stdout must be terminals, or the command prints `eggdb ui needs a terminal;
  use eggdb get/scan instead` to stderr and exits 2.
- The file is opened with `create_if_missing(false)`. A missing file, `Locked`, `NotADatabase`,
  `Incompatible` and `Corrupt` print the `eggdb::Error` message and exit 1 before the screen is
  touched. For `Locked` the message adds: `another program has it open; eggdb ui --read-only
  can read it beside another reader`.
- `--read-only` opens read-only: `e`, `a`, `d` are dimmed in the hints bar and pressing one
  shows the toast `Opened read-only; start without --read-only to edit`.
- `--cache` sets `Options::cache_bytes`, which is also the largest value one edit can store.
- Everything else uses the API's defaults, so a commit is durable (`Durability::Full`) before
  the UI draws the next frame.

## Screen layout

### 80 × 24, the reference size
```
 EggDB  app.db                 1.2 MiB · 312 pages · 1,204 keys · 09:14:03Z  RW 
 Keys /user:       1–20 of 1,204│ Value  text · 58 B                      1–3/3 
> user:1                      58│ user:1                                        
  user:2                      61│───────────────────────────────────────────────
  user:3                      55│ Ada Lovelace                                  
  user:4                      70│ ada@example.com                               
  user:5                   1.2 K│ 1815-12-10                                    
  user:6                      58│                                               
  user:7                      44│                                               
  user:8                      59│                                               
  user:9                      62│                                               
  user:10                     61│                                               
  user:11                     57│                                               
  user:12                 14.6 K│                                               
  user:13                     60│                                               
  user:14                     58│                                               
  user:15                     63│                                               
  user:16                     55│                                               
  user:17                     59│                                               
  user:18                     61│                                               
  user:19                     58│                                               
  user:20                     60│                                               
 Opened app.db read-write, durability Full                                      
 ↑↓ move  Enter open  / filter  e edit  a add  d delete  c check  ? help  q quit
```
Regions, top to bottom, with their heights at `rows` rows and widths at `cols` columns:

| Region | Rows | Content |
|--------|------|---------|
| Header | 1 | ` EggDB  <file name>`, then right-aligned the stats: file size, page count, key count, time of the last commit (UTC, `HH:MM:SS`; the full date and stamp are on the check screen), then `RW` or `RO`. Fields drop from the left of the stats group (size first, then pages) until the line fits; the file name is never dropped and is cut with `…` at the front (`…/data/app.db`) when it alone is too long. |
| Pane titles | 1 | Left: ` Keys` and the active prefix filter as `/prefix`, right-aligned `first–last of total`. Right: ` Value`, the view mode, the value's size, right-aligned the viewer's position (`line a–b/n` in text and JSON, `bytes a–b of n` in hex). The focused pane's title is drawn in the focus style. |
| Panes | rows − 4 | Key list on the left, value viewer on the right, one separator column between them. |
| Status | 1 | The latest message (toast). Empty when there is none. |
| Hints | 1 | The keys that work right now, in the current pane and mode. Dimmed entries are unavailable (read-only, nothing selected). |

Column split: `list_cols = clamp(cols × 2 / 5, 28, 60)`, the separator takes one column, the
viewer gets the rest. At 80 columns that is 32 + 1 + 47.

Key list rows: a two-column marker (`> ` on the selected row, blank otherwise, the marker is
what makes the selection visible without any colour), the key cut with `…` to fit, one space,
and the value size right-aligned in five cells (`58`, `1.2 K`, `14.6 M`, `1.0 G`; `K`, `M`, `G`
are powers of 1024, matching `db.stats()`'s units in the header). Viewer rows: the full key on
the first row (wrapped onto a second row when longer than the pane, then cut with `…`), a rule,
then the value from the third row.

Key and value text is shown as UTF-8 where it is valid and printable; every other byte is
`\xNN`, and `\t`, `\n`, `\r` and `\\` are escaped in the list and the viewer's key line (in the
viewer's value area, text mode shows real line breaks and tabs expand to 4 cells). A key that is
mostly bytes therefore reads as `\x00\x00\x12user`, and the same spelling is what the editor
accepts (below).

### Wide terminals
The list grows with the terminal up to 60 columns and the viewer takes everything else; nothing
else changes at 100 or 200 columns. When collections exist (later release) and the database has
more than one, a sidebar of `clamp(cols / 6, 16, 24)` columns appears on the left at
`cols ≥ 110`, and `Tab` cycles sidebar → keys → value. v0.2.0 draws no sidebar, but the layout
function already takes `sidebar_cols` (0 today), so adding it moves no other region.

```
 Collections        │ Keys /user:              1–18 of 1,204│ Value  json · 1.2 K                          line 1–18/41 
 > default          │> user:1                             58│ user:5                                                    
   sessions         │  user:2                             61│───────────────────────────────────────────────────────────
   …                │  user:3                             55│ {                                                         
```
(120 columns, a later release; shown so the v0.2.0 regions leave it room.)

### Small terminals
- `cols < 80` and `cols ≥ 40` (a phone over SSH, a split pane): one pane fills the width; the
  key list is shown, `Enter` or `Tab` shows the value, `Esc` or `Tab` returns. Header and hints
  stay; the header keeps only the file name and the key count; the hints bar shows the first
  entries that fit and always ends with `? help`.
- `cols < 40` or `rows < 10`: the screen is cleared and shows, centred:
  ```
  EggDB needs a terminal of at least 40 × 10.
  This one is 32 × 8. Make it larger, or press q.
  ```
  Nothing is lost: an editor in progress resumes when the terminal is large enough again.

## Panes and navigation
Focus is in the key list at start. `Tab` and `Shift-Tab` move focus between the panes; `Enter`
in the list focuses the viewer; `Esc` in the viewer goes back to the list. The focused pane's
title uses the focus style and the hints bar shows that pane's keys.

### Key list
The list is a window over an ordered scan, so it works the same for 20 keys and for a billion:
- The window is `rows − 4` entries starting at `top_key`. A redraw runs one read transaction
  that scans `scan_prefix(filter)` from `top_key` for `rows − 4 + 1` entries with `next_entry`
  (lending, so only the key, the value's length and its first 64 bytes are copied, never a large
  value), then `get`s the selected key for the viewer (below) and ends.
- `↓`/`j`, `↑`/`k` move the selection; past the window's edge the window moves by one.
  `PgDn`/`PgUp` move by a window: `PgDn` scans forward from the last visible key, `PgUp` scans
  `rev()` from the first visible key. `Home`/`g` scans from the filter's start; `End`/`G` scans
  `rev()` from its end, so the last page is found without counting.
- The title's `first–last of total` needs a count. On open a worker thread counts the keys of
  the current filter in its own read transaction, lending entries; until it is done the title
  shows `1–20 of 1,204+` (keys seen so far) and the header `1,204+ keys`, both updating about
  every 200 ms. Every commit the UI makes adjusts the count by ±1; a `c` check replaces it with
  `CheckReport::entries`. The position `first` is the number of keys before `top_key`, which is
  only known after `Home`, `PgDn` steps from it, or a finished count; until then it shows
  `?–?/1,204` with `20 shown`. The count restarts when the filter changes.
- An empty filter result shows one dimmed row `no keys start with "user:"`; an empty database
  shows `empty database — press a to add a key`.

### Prefix filter
`/` opens the filter in the list's title row: ` Keys /█`. Typing narrows the list as you type
(each key press redraws from a fresh scan). `Enter` keeps the filter, `Esc` restores the filter
that was active before `/`. The filter is bytes: `\xNN` escapes are accepted, so `/\x00` finds
keys that start with a zero byte. A filter that matches nothing shows the empty-result row, not
an error. `Backspace` on an empty filter closes it. The filter is shown in the title as long as
it is active; `/` then `Esc` with an empty line clears it.

### Value viewer
The selected key's value is read with `get` in the redraw's transaction and kept in memory
until the selection moves. A value is read whole (the API has no streaming read yet), so above
`64 MiB` the viewer does not load it on its own: it shows `128.0 M value — press Enter to load
(takes 128 MiB of memory)` and loads on `Enter`.

Three modes, `v` cycles `auto → text → json → hex → auto`; the title shows the mode and `(auto)`
when it was chosen by the rule:

| Mode | Rendering |
|------|-----------|
| text | The bytes as UTF-8. Lines longer than the pane are cut with `…`; `←`/`→` scroll horizontally by 8 cells, `w` toggles wrapping. Invalid bytes show as `\xNN` in the error style. Tabs expand to 4 cells; other controls show as `^X`; bidi overrides and isolates and the other invisible format characters show as `\u{202e}`, in keys too, so no byte of the data can reorder or hide text on the screen. |
| json | Our own strict RFC 8259 parser and pretty-printer: 2-space indent, one member per line, keys in the key style, strings, numbers and `true`/`false`/`null` in their styles, long strings cut per line like text mode. `v` to json on a value that is not JSON shows text mode plus the toast `Not JSON: expected ',' or '}' at byte 212`. |
| hex | `offset  hex bytes  │ ascii gutter`: 16 bytes per row when the viewer has ≥ 76 columns, 8 otherwise. Offsets are 8 hex digits, bytes grouped in 8, the gutter shows `.` for anything outside 0x20–0x7E. |

Auto-detect rule, in order: an empty value is text (`(empty)` dimmed); if not valid UTF-8, or it
contains a control byte other than `\t`, `\n`, `\r` → hex; if the first non-whitespace byte is
`{` or `[` and the whole value parses as JSON → json; otherwise text.

Scrolling in the viewer: `↑`/`↓`/`j`/`k` one line (one row of bytes in hex), `PgUp`/`PgDn` a
pane, `Home`/`g` and `End`/`G` to the ends. The title's position always says where you are
(`line 120–139/8,002`, `bytes 7,936–8,191 of 1,048,576`, decimal like the other counts). Rendering is windowed: a
1 GiB value is held once in memory and only the visible rows are formatted per frame.

## Keyboard map
Unmodified letters are preferred so the UI works on every keyboard; `Ctrl-C` and `Ctrl-S` are
the only chords a user needs.

| Key | In the key list | In the viewer | In an editor |
|-----|-----------------|---------------|--------------|
| `↑` `↓` `j` `k` | move selection | scroll one line | move cursor (letters type) |
| `PgUp` `PgDn` | move one window | scroll one pane | move one pane |
| `Home` `End` (`g` `G`) | first / last key | top / bottom of value | start / end of line (`Ctrl-Home`/`Ctrl-End`: of text) |
| `←` `→` | — | scroll horizontally | move cursor |
| `Enter` | open: focus the viewer | load a value above the size gate | single-line: confirm; multi-line: new line |
| `Tab` `Shift-Tab` | switch pane | switch pane | insert tab / — |
| `/` | prefix filter | — | types `/` |
| `v` | cycle view mode | cycle view mode | — |
| `w` | — | toggle wrapping (text) | — |
| `e` | edit the selected value | edit this value | — |
| `a` | add a key | add a key | — |
| `d` | delete the selected key (confirm) | delete this key (confirm) | — |
| `c` | check screen | check screen | — |
| `r` | re-read the list and the stats | re-read the value | — |
| `m` | show the last message in full | same | same |
| `?` | help screen | help screen | help screen (editor section) |
| `Esc` | close filter / clear toast | back to the list | cancel (confirm if changed) |
| `Ctrl-S` | — | — | confirm (both editors) |
| `q` `Ctrl-C` | quit | quit | quit (confirm if changed) |

Editor keys, in both editors: `Backspace`, `Delete`, `Ctrl-←`/`Ctrl-→` word moves, `Ctrl-A`/`Ctrl-E`
line start/end, `Ctrl-U` delete to line start, `Ctrl-K` delete to line end, `Ctrl-Z` undo and
`Ctrl-Y` redo (unlimited while the editor is open), `Alt-Enter` in the single-line editor inserts
a line break and turns it into the multi-line editor. Pasting (bracketed paste) inserts the text
as it is, line breaks included, and never triggers key bindings.

`Esc` alone is `Esc`: the input layer waits 25 ms for a following byte to tell an escape key from
an escape sequence, and treats `Alt-x` as `Esc` then `x` when the terminal sends it that way.
`Ctrl-C` arrives as a key (raw mode turns the terminal's signal keys off), so it is handled like
`q`; a second `Ctrl-C` while the unsaved-edit question is open quits without saving.

## Editing
Every edit is one write transaction that begins when the user confirms, not when the editor
opens: an editor left open overnight blocks nothing, and the only transaction the UI ever holds
is the one committing right now.

### Edit a value (`e`)
1. The viewer pane becomes the editor, its title ` Edit  user:1  text · 58 B → 61 B  ln 1 col 4`
   (the arrow shows the size the stored value would have). The editor always works on the raw
   bytes in the escaped text spelling (the pretty JSON view is display only): a text value opens
   as it is, a binary value opens as `\xNN` escapes which decode on save, and `\\` is a backslash.
   The value opens in the **single-line editor** (one row, horizontal scrolling) when it has no
   line break and is under 1 KiB, otherwise in the **multi-line editor** (the whole pane, line
   numbers in the gutter when the pane is ≥ 60 columns). Both are ours, in the UI; nothing
   external is launched.
2. `Enter` (single line) or `Ctrl-S` confirms. An escape that does not decode (`\xZ`) is an
   error at this step: the cursor moves to it and the toast says `Bad escape at column 12:
   expected two hex digits after \x`. Nothing happens to the database.
3. The confirm step is a modal in the middle of the screen:
   ```
   ┌ Replace value ──────────────────────────────────┐
   │ Key    user:1                                   │
   │ Value  58 B → 61 B, text                        │
   │                                                 │
   │ One commit, written to disk before you continue.│
   │                                                 │
   │            Enter commit     Esc back to editor  │
   └─────────────────────────────────────────────────┘
   ```
   `Esc` returns to the editor with everything as it was.
4. `Enter` runs `try_begin_write`, `put`, `commit`, and the screen redraws from a fresh read:
   the list keeps the same key selected, the viewer shows the new value, the toast says
   `Committed user:1 (61 B) at 09:14:03Z`. The header's commit time and size update.

### Add a key (`a`)
The list's title row becomes ` Add key: █` (single-line editor for the key, same escapes;
`Enter` continues, `Esc` cancels). A key over `db.max_key_bytes()` is refused at this step with
the toast `Key is 530 B; this database's keys are at most 512 B`. Then the value opens in an
empty single-line editor, and the flow continues as for edit; the confirm modal is titled
`Add key`, or `Replace value` when the key exists (the modal then says `already exists: 58 B →
12 B`). After the commit the new key is selected, the filter is left as it is, and if the key
does not match the filter the toast says so: `Committed user:9; not shown by filter /sess`.

### Delete a key (`d`)
```
┌ Delete key ──────────────────────────────────────┐
│ Key    user:1                                    │
│ Value  58 B, text: Ada Lovelace                  │
│                                                  │
│ Deletes this one key in one commit, written to   │
│ disk before you continue. There is no undo.      │
│                                                  │
│                 d delete     Esc keep            │
└──────────────────────────────────────────────────┘
```
The confirmation key is `d` again (not `Enter`, so a held `Enter` can't delete). After the
commit the selection moves to the next key (or the previous at the end) and the toast says
`Deleted user:1 (58 B) at 09:14:04Z`.

### When a commit fails
| Error kind | What the UI does |
|------------|------------------|
| `WriterActive` (`try_begin_write`) | Modal `Another write transaction is open on this database` with the API's message; `r` retries with `begin_write` (waits `write_timeout`, 5 s, with a spinner), `Esc` returns to the editor or the delete question with the input kept. |
| `TooLarge` | `rollback`, back to the editor, toast with the message and the limit: `Value is 17.2 M; the most this database stores in one edit is 15.8 M (page cache 16 MiB). Start eggdb ui with --cache 64M, or store a smaller value`. |
| `CacheFull` | Same, the API already rolled back; `Start eggdb ui with --cache 64M`. |
| `ReadOnly` | Cannot happen (the keys are dimmed); if it does, a toast. |
| `DatabaseFull`, `Io`, `Other` | Modal with the message; `Esc` back to the editor, nothing committed. |
| `NeedsReopen` | Modal `A write failed and the database must be reopened to recover its last durable state`, `r` reopens (`Database::open` again, then redraw) and `q` quits; the editor's text is kept and offered again after a reopen. |
| `Corrupt` | Modal with the message and `c` to open the check screen. |

An edit the user discards is gone; the UI has no drafts on disk.

## Messages: toast or modal
Rule: a **toast** (status line, the message style, cleared by the next key press or after 8 s)
for anything that needs no decision and fits one line: confirmations, filter results, `Not
JSON…`, read-only reminders. A **modal** (centred box, at most `cols − 4` wide and `rows − 4`
high, scrolls when the text is longer, `Esc` closes) for anything that asks a decision, loses
work, or doesn't fit the status line. A toast that was cut shows `… (m)` at its end and `m`
opens the full text as a modal.

Every error message is the `eggdb::Error`'s own text (written for people and models, D87),
followed where the UI knows one by a line starting `Try:` with what to do. Errors use the error
style, warnings (leaked pages, a count still running) the warning style, success the success
style; each also starts with a word (`Error:`, `Warning:`, `Committed`) so colour is never the
only signal.

## The check screen (`c`)
```
 EggDB  app.db                                          check running 00:04  RW 

  Stats (from the header, read without touching a page)
    page size 4 KiB · 312 pages · 18 free · file 1.2 MiB
    last commit 2026-10-26 09:14:03.512 UTC (stamp 1793005243512000.0001)
    cache 16 MiB, 211 pages held, 98.2% hits · log since checkpoint 24 KiB

  Check
    reading every page… 240 of 312     ▏████████████████▎        ▏  4 s

  Other operations wait while the check runs; Ctrl-C quits.
```
`Database::check` runs on a worker thread so the screen keeps drawing; it cannot be cancelled,
so `Esc` is disabled until it ends (the hints bar says so). Progress is `pages read / page_count`
if the API exposes a progress callback, otherwise the elapsed time and a spinner. The stats
section is shown at once. When the check ends:
```
  Check  OK — every page accounted for, 2.1 s
    1,204 keys · keys 38.1 KiB · values 1.1 MiB
    tree height 2, 203 pages · 3 large values on 61 overflow pages
    18 free pages · 30 log pages · 0 leaked pages

  Enter back
```
A leaked-page count above 0 is a warning line (`Warning: 4 leaked pages do no harm but are never
reused`); a `Corrupt` error replaces the summary with `Check  FAILED` in the error style, the
full message, and `Try: eggdb check app.db for the same report on the command line; restore the
file from a backup`. The key count in the header is replaced by `entries`. `Enter` or `Esc`
returns to the browser, which redraws from a fresh read.

## The help screen (`?`)
Full screen, scrollable, `Esc`, `?` or `q` closes it. Text (ASCII shown; the Unicode build uses
arrows):
```
 EggDB ui — help                                                            1/2

 Move        Up/Down or j/k   PgUp/PgDn   Home/End or g/G   Left/Right scroll
 Panes       Tab or Shift-Tab switch   Enter open the value   Esc back to keys
 Find        /  filter keys by prefix (bytes; \xNN for a byte)   Esc restores
 View        v  text > json > hex > auto      w  wrap long lines (text)
 Change      e  edit value   a  add key   d  delete key (asks d again)
 Check       c  verify every page and show stats     r  re-read
 Messages    m  show the last message in full
 Quit        q or Ctrl-C (asks before discarding an edit)

 Editing     Enter confirms a one-line value, Ctrl-S confirms any value
             Alt-Enter makes it multi-line   Esc cancels
             Ctrl-Z/Ctrl-Y undo/redo   Ctrl-U/Ctrl-K delete to line start/end
             Values are edited as bytes: \xNN is one byte, \\ a backslash,
             \n a line break in a one-line editor
 Every change is one transaction, written to disk before the screen redraws.
 Nothing is changed until you confirm. There is no undo after a commit.

 Terminal    at least 40x10; two panes from 80 columns
             NO_COLOR, --mono and --ascii change colours and box characters
 Database    app.db   page size 4 KiB   keys up to 512 B   values up to 1 GiB
             (one edit up to 15.8 M with the current --cache)
 Version     EggDB 0.2.0   docs github.com/eggspot/EggDB/blob/main/docs/ux/ui.md
```
The last section is filled in from the open database so the limits shown are the real ones.

## Resize
The terminal layer delivers a `Resize` event; the UI recomputes the layout and redraws. The
selected key stays selected and visible (the window is re-read with the selection in the same
relative row where possible), the viewer keeps its scroll line, editors keep their text and
cursor, and modals re-centre. Below the minimum the small-terminal message is drawn, and the
state behind it is untouched.

## Colours, styles and fallbacks
Styles are roles, resolved once at start against what the terminal can do:

| Role | 16-colour terminal | NO_COLOR / `--mono` | `TERM=dumb` |
|------|--------------------|---------------------|-------------|
| header, pane titles | inverse | inverse | inverse |
| focused title | bold cyan on default (or inverse + bold) | bold inverse | inverse (the `>` marker and `[Keys]` brackets mark focus) |
| selected row | inverse | inverse | `>` marker only |
| key names in JSON | cyan | bold | plain |
| strings / numbers / literals | green / yellow / magenta | plain | plain |
| dimmed (sizes, hints, unavailable keys) | bright black | plain (unavailable keys are omitted) | plain |
| error / warning / success | red / yellow / green, bold | bold | plain with `Error:` / `Warning:` words |
| hex offsets and gutter | bright black / default | plain | plain |

Only the 16 ANSI colours are used, so the UI follows the user's terminal theme on light and dark
backgrounds; no 256-colour or truecolour palette exists to drift from it. `NO_COLOR` set (any
value) or `--mono` drops colours and keeps attributes. `TERM=dumb`, or `--mono --ascii`, is the
plain build: no attributes, ASCII only, which is also the screen-reader build.

Unicode is used when `LC_ALL`, `LC_CTYPE` or `LANG` says UTF-8 (on Windows, when the console
output code page is UTF-8, which the terminal layer sets for the session), and never when
`--ascii` is given. Fallbacks: `│ ─ ┌ ┐ └ ┘` → `| - + + + +`, `…` → `~`, `–` → `-`, `·` → `|`,
`↑↓` → `Up/Down`, `█▏▎` progress → `#=.`, `▾` → `v`. The fallback table is one function, tested.

Character widths: our own table of East Asian Wide and Fullwidth ranges (2 cells) and combining
marks (0 cells, shown appended to the previous cell); anything unknown is 1. A character wider
than the cells left on a row is replaced by `…`.

## Mouse
**Out for v0.2.0.** Every action is a key, and enabling mouse reporting takes over the
terminal's own selection, which is how people copy a key or value out of the UI today (there is
no clipboard access without a terminal-specific escape). A `--mouse` option with wheel scrolling
and click-to-select can come later without changing the layout; the hints bar already has room.

## Accessibility
- Nothing is conveyed by colour alone: selection has the `>` marker, focus has the title style
  and a `[Keys]`/`[Value]` bracket in the plain build, errors start with `Error:`.
- Every state has a text position (`1–20 of 1,204`, `line 3/41`, `ln 2 col 7`) in a fixed place
  so a screen reader's cursor can find it.
- The plain build (`TERM=dumb`, or `--mono --ascii`) draws no box characters and no attributes,
  and redraws only the rows that changed, which keeps screen readers from re-reading the frame.
- No animation except the spinner and the progress bar on the check screen; `--no-animation` is
  not needed since both are text that only changes once a second.
- The default sizes (80 × 24) and the minimum (40 × 10) are tested, as is large-font use (a 100 ×
  12 terminal: two panes, 8 list rows).

## The terminal layer and the testing contract
The UI never talks to the OS. It uses one trait, implemented once for real terminals in the
platform layer and once by a fake in the tests:

```rust
pub trait Terminal {
    fn size(&self) -> Size;
    fn capabilities(&self) -> Capabilities;
    fn next_event(&mut self, timeout: Duration) -> io::Result<Option<Event>>;
    fn present(&mut self, frame: &Frame) -> io::Result<()>;
}

pub struct Size { pub cols: u16, pub rows: u16 }
pub struct Capabilities { pub colors: Colors, pub attributes: bool, pub unicode: bool }
pub enum Colors { None, Ansi16 }

pub enum Event { Key(KeyEvent), Paste(String), Resize(Size), Interrupt }
pub struct KeyEvent { pub code: KeyCode, pub modifiers: Modifiers }
pub enum KeyCode {
    Char(char), Enter, Esc, Tab, BackTab, Backspace, Delete, Insert,
    Up, Down, Left, Right, Home, End, PageUp, PageDown, F(u8),
}
pub struct Modifiers { pub ctrl: bool, pub alt: bool, pub shift: bool }

pub struct Frame { pub size: Size, pub cells: Vec<Cell>, pub cursor: Option<(u16, u16)> }
pub struct Cell { pub text: CellText, pub style: Style }
pub enum CellText { Char(char), Wide(char), WideContinuation, Combined(char, char) }
pub struct Style {
    pub fg: Color, pub bg: Color,
    pub bold: bool, pub inverse: bool, pub dim: bool, pub underline: bool,
}
pub enum Color { Default, Ansi(u8) }
```
Input events in, a cell grid out: the UI owns a `Frame`, draws every region into it each cycle
(`Ui::draw(&mut Frame)`), and `present` writes it. The real implementation diffs the frame
against the last one presented and emits only the changed runs (cursor moves, style changes,
text), ending with the cursor placement; after a `Resize` it redraws everything. The `Interrupt`
event is for signals that must end the program (SIGTERM, SIGHUP, a closed console): the UI
treats it as `q` without questions.

The UI is `Ui::new(database, options, capabilities)` with `fn handle(&mut self, event: Event)`
and `fn draw(&self, frame: &mut Frame)`; `run(terminal)` is the loop `next_event → handle →
draw → present` plus a `tick` every 200 ms while a worker (count, check, write wait) is running.
Workers report through a channel the loop drains in `handle`; `Ui::run_until_idle()` blocks
until no worker is pending, which makes the tests deterministic.

**The fake terminal** (`FakeTerminal` in the UI crate's tests, modelled on the platform layer's
`sim_*` modules): `new(cols, rows, capabilities)`, `push(Event)`, `type_str("user:9")` (one
`Char` event per character), `press("Ctrl-S")` (a parsed key name), `resize(cols, rows)`,
`screen() -> String` (the last presented frame as rows of plain text, each row trimmed on the
right, wide cells as their character once), `styled(row, col) -> Style`, and `frames_presented()`.
Screens are compared with golden files under `crates/eggdb-ui/tests/screens/<scenario>.txt`;
`UPDATE_SCREENS=1 cargo test` rewrites them, and the diff a failing test prints is the two
screens side by side. Test databases are built in a temporary directory from a small fixture
(keys `user:1`…`user:20`, a JSON value, a binary value, one 14 KiB overflow value, one empty
value), so every scenario starts from the same bytes.

**Scenarios** the tests cover, each a script of events and one or more screen comparisons:
1. Open at 80 × 24: header, 20 rows, first key selected, first value shown in auto text mode.
2. Open read-only: `RO` in the header, dimmed `e a d`, pressing `e` shows the read-only toast.
3. Navigation: `↓` × 25 moves the window; `PgDn`, `PgUp`, `End`, `Home` land where the model says.
4. Position title: `1–20 of 20` after the count finishes; `?–?` form while it runs (with a fixture
   that makes the worker wait).
5. Prefix filter: `/user:1` shows 11 keys, `Esc` restores, `/zzz` shows the empty row, `/\x00`
   is accepted as a byte.
6. Auto-detect: the JSON fixture opens pretty-printed, the binary one in hex with 8 bytes per
   row at 80 columns and 16 at 120, the text one as text, the empty one as `(empty)`.
7. `v` cycles modes; forcing json on text shows the toast with the byte offset.
8. Hex and text scrolling over the 14 KiB value: position titles, `End`, horizontal scroll, `w`.
9. Large-value gate: a 70 MiB value is not loaded until `Enter` (fixture built lazily).
10. Edit single-line: `e`, type, `Enter`, confirm modal text, `Enter`, the toast and the new
    value; the model map agrees with a scan of the database afterwards.
11. Edit multi-line: `Alt-Enter`, lines, `Ctrl-S`, undo and redo, `Esc` with changes asks, `Esc`
    without changes doesn't.
12. Escapes: editing the binary value shows `\xNN`; saving it unchanged leaves the bytes
    identical; `\xZ` is refused at the cursor.
13. Add: a new key is selected after the commit; adding an existing key shows `Replace value`;
    a key over the limit is refused with the limit in the toast; a key outside the filter says so.
14. Delete: the modal shows the key, size and preview; `Enter` does nothing, `d` deletes, the
    selection moves to the next key, the last key moves it to the previous one.
15. Too large: with `--cache 1M` a 2 MiB value fails with the `TooLarge` toast naming the limit,
    the editor keeps the text, the database is unchanged.
16. Writer busy: a test hook holds the write transaction; `WriterActive` modal, `r` retries after
    the hook releases, the commit succeeds.
17. Quit handling: `q` quits clean; `q` in a changed editor asks; `Ctrl-C` twice quits anyway;
    `Interrupt` quits at once.
18. Check: the running screen, then the summary from a fresh `CheckReport`; a fixture with a
    corrupted page shows `FAILED` and the message.
19. Help: both pages, limits filled from the open database.
20. Resize: 80 × 24 → 120 × 40 keeps the selection; → 60 × 20 stacks the panes; → 30 × 8 shows the
    too-small message and 80 × 24 again restores an editor in progress.
21. Fallbacks: the same scenario 1 screen with `unicode: false` (ASCII box table), with
    `colors: None` (styles absent, `>` marker present) and with `attributes: false`.
22. Diffing: the second `present` after `↓` touches two list rows, the viewer and the status
    line, and nothing else (`frames_presented` plus a cell-change count from the fake).
23. Messages: a long error shows `… (m)`, `m` opens it as a modal that scrolls.
24. Model agreement: 500 random edits, adds and deletes through the UI against a `BTreeMap`,
    then a scan of the file equals the map (the UI's own model test, D89).
Every scenario is also run at 40 × 10 where the layout allows, and a smoke test runs the real
terminal layer's escape-sequence parser over a recorded input log (arrow keys, `Esc` alone,
`Alt-x`, bracketed paste, a resize) and asserts the events.

## What the terminal layer needs from the platform layer
The platform layer (`eggdb-platform`, behind its one boundary, D8) gets a `terminal` module with
an OS implementation per family and a simulated one for tests, like `fs` and `net` today. The
UI crate needs these operations and nothing else:

| Need | macOS and Linux (and the BSDs) | Windows |
|------|--------------------------------|---------|
| Is this a terminal? | `isatty` on stdin and stdout | `GetConsoleMode` succeeds on both handles |
| Raw mode | save `termios`, then clear `ICANON`, `ECHO`, `ISIG`, `IEXTEN`, `IXON`, `ICRNL`, `OPOST`; `VMIN=0`, `VTIME=0` | save both console modes; input: clear line, echo and processed input, set virtual-terminal input; output: set virtual-terminal processing, disable newline auto-return; set the output code page to UTF-8 and remember the old one |
| Alternate screen, cursor, paste | write `?1049h`, `?25l`, `?2004h` at start and the reverse at exit | the same sequences (Windows 10 1809 and later) |
| Window size | `ioctl(TIOCGWINSZ)` | `GetConsoleScreenBufferInfo` (the window rectangle, not the buffer) |
| Resize notification | `SIGWINCH` handler sets an atomic flag and writes one byte to a self-pipe; the input wait polls stdin and the pipe together | `WINDOW_BUFFER_SIZE_EVENT` from `ReadConsoleInputW` |
| Input with timeout | `poll` on stdin and the self-pipe, then `read`; the layer parses bytes into `Event`s (CSI and SS3 sequences, the 25 ms `Esc` rule, bracketed paste, UTF-8 assembly) | `WaitForSingleObject` on the input handle, `ReadConsoleInputW`; key records become `Event`s directly, and with virtual-terminal input enabled the same escape parser handles what arrives as text |
| Output | one `write_all` per presented frame, buffered | `WriteConsoleW` or `WriteFile` with virtual-terminal processing on |
| Restore on exit | an RAII guard restores `termios`, the main screen, the cursor and paste mode; `Ui::run` returns before the guard drops | the guard restores both console modes and the code page |
| Restore on panic and abort | a panic hook installed at start writes the restore sequence and `tcsetattr`s the saved `termios` (the hook runs before `panic = "abort"` aborts) | the same hook |
| Restore on signals | handlers for `SIGTERM`, `SIGHUP` and `SIGQUIT` that only `write` the restore bytes, `tcsetattr`, and `_exit(130 + n)`: every call async-signal-safe, no allocation. `SIGINT` never arrives (`ISIG` is off, `Ctrl-C` is a key); `SIGTSTP` likewise, so `Ctrl-Z` is a key (undo) and job-control suspend is not supported in v0.2.0 | `SetConsoleCtrlHandler` for `CTRL_CLOSE_EVENT`, `CTRL_LOGOFF_EVENT`, `CTRL_SHUTDOWN_EVENT` restoring the modes before returning |
| Capabilities | `NO_COLOR`, `TERM` (`dumb` → plain), `COLORTERM` ignored (16 colours only), locale variables for Unicode | `NO_COLOR`; Unicode on because the code page is set to UTF-8; plain if virtual-terminal processing cannot be enabled (an old console) |

The simulated terminal in the platform layer is the same `FakeTerminal` the UI tests use,
exported so the CLI's `eggdb shell` (a nice-to-have) can test against it too.

## Decided by this design (for the PR and docs/decisions.md)
1. The UI holds no transaction between key presses; a write transaction lives only from confirm
   to commit, begun with `try_begin_write` and retried with `begin_write` on request.
2. The key list is a window over a scan with lending entries, so it works at every scale; the
   count is a background worker and the UI is honest about not knowing it yet.
3. Values are edited as bytes in the escaped text spelling; the pretty JSON view is display
   only, so saving never reformats what the user did not type.
4. Delete confirms with `d`, not `Enter`.
5. 16 ANSI colours only, following the terminal theme; `NO_COLOR`, `--mono`, `--ascii` and
   `TERM=dumb` are the fallbacks and the plain build doubles as the screen-reader build.
6. Mouse support is out for v0.2.0 to keep the terminal's own copy-selection working.
7. The terminal abstraction is one trait, input events in and a cell grid out, with a fake
   terminal and golden plain-text screens as the testing contract.
8. The OS parts (raw mode, size, resize, restore on exit, panic and signals) live in
   `eggdb-platform`'s `terminal` module; the UI itself in a new `eggdb-ui` crate used by the
   `eggdb` binary.
