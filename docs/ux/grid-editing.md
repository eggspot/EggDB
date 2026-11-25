# Editing data in the grid

Designed by Claude Fable (D15) for roadmap M6e: editing a cell, adding and deleting rows,
pending changes, commit and discard, errors, and the keys and confirmations for destructive
changes (client-brief.md 3c: edits accumulate in a pending-changes bar showing the generated
statements; `Apply` runs them in one transaction). Every token, rect and role not restated is
[gui.md](gui.md)'s and [workspace.md](workspace.md)'s (3 for the grid, 9.1 for dialogs, 9.3 for
the value view, 9.4 for the menu). Rects are at the 1280 × 800 golden frame, unchanged by this.

## 1. When a grid is editable
A result is editable when the statement read one table (no join, no aggregate, no `DISTINCT`,
no set operation, no view without an updatable definition), the connection is read-write, and
the result holds every column of that table's primary key (or, failing a primary key, of one
unique index whose columns are all `not null`). The engine reports the source table and key
columns with the result; the client decides nothing from the text. A result that reads one
table but lacks its key draws 3.7's banner place with the info icon (a circle with an `i` of two
rects, `textSecondary`) and `body` `textPrimary`: `Read-only: select the primary key (id) to edit
these rows.` with a ghost `Add id` button that re-runs the statement with the key columns
prepended to the select list (shown in the editor as the new text, one undo step). A table
without any key is read-only with `Read-only: orders has no primary key or unique index.` A
view, a join, a model tab's grid and a keys tab are read-only without a banner. The status
bar's left group shows `Editable · orders` after an editable run, 16 px after the tags, in
`small` `textSecondary`.

## 2. Editing a cell
| Starts editing | On an editable grid |
|-------|------|
| `F2` | the current cell, caret at the text's end |
| a printable key | the current cell, its text replaced by the key (the first keystroke), as in a spreadsheet |
| a double-click | the clicked cell, caret at the click |
| `Enter` | unchanged: opens the value view (9.3), which gains an `Edit` button (section 3) |
| `Backspace`, `Delete` | sets the cell to the empty string / `NULL` at once as a pending change (no editor), `Delete` to `NULL` only when the column allows it, else nothing and the status bar says `price is not null` for 2 s |

The cell editor is gui.md's input drawn inside the cell: `(cell.x, cell.y, cell.w, 24)`, `bg0`,
2 px `accent` rect (the current-cell rect, kept), `code` text at the cell's padding, the caret
and selection of the editor, text scrolling horizontally inside the cell when longer, never
growing the cell. Typing is the usual editing; `Ctrl/Cmd+Enter` and `Enter` commit the cell
(pending, section 4) and keep the cell current (`Enter` then moves down one row, `Shift+Enter`
up); `Tab` / `Shift+Tab` commit and move right / left (inside the grid while editing, the only
time `Tab` moves between cells); `Esc` cancels the cell's edit and restores its text; a click on
another cell commits and moves. The typed text is parsed by the column's type when committed:
numbers, booleans (`true`, `false`, `t`, `f`, `1`, `0`, `yes`, `no`), dates and times in the
engine's input forms, JSON (must parse), uuids, intervals; text takes anything. The word `NULL`
typed alone means `NULL` for a nullable column (the editor shows it in `textDisabled` as the
cell does); `''` means the empty string; to store the text `NULL`, type it in the value view. A
value that does not parse keeps the editor open with the input's invalid state (`error`
border, the strip under the grid of section 6 reading `Error: "abc" is not an integer`) until
fixed or cancelled. A blob or vector cell is edited only in the value view (`F2` opens it there).
A boolean cell edited with `Space` toggles it as a pending change without an editor.

While a cell is being edited the grid's role exposes the editor as a focused `textbox` named
`<column>` inside the `gridcell`; the commit raises the cell's value change.

## 3. The value view's Edit
9.3's view on an editable cell gains `Edit` (ghost, `(348, 2, 56, 28)`, before `Copy`); `Edit` or
`Ctrl/Cmd+E` in the view turns the body into an editor (caret, selection, typing, the same
parsing; JSON kept pretty while edited and stored compact), the button becomes `Save`
(primary) and a `Cancel` (ghost) appears at `(284, 2, 56, 28)`; `Ctrl/Cmd+Enter` saves, `Esc`
cancels (asking nothing: the view's own edit is one step), and saving makes the cell's pending
change and closes the view. For a blob the body takes hex (`0x` optional, spaces and line
breaks ignored, an odd digit count refused with `Error: hex needs an even number of digits`);
for a vector a JSON array of the column's dimension. The view's role while editing: the body an
editable `textbox`, `Save` the default button.

## 4. Pending changes
A change is one of: a cell's new value, a new row, a deleted row. They accumulate per tab until
`Apply` or `Discard`; running the tab again (`Ctrl/Cmd+Enter`) while changes are pending asks
9.1's dialog `Run Query 1 and discard 3 pending changes?` (`Discard and run` / `Keep`, default
`Keep`), and closing the tab adds ` Its 3 pending changes are discarded.` to the close dialog's
body (priority after transaction, before unsaved).

### 4.1 How a pending change looks
| Change | Cell or row | Gutter |
|--------|------|------|
| a changed cell | fill `warningFill` (section 9) under the cell's text, a 6 px right triangle in the cell's top-left corner `warning` (points (0, 0) (6, 0) (0, 6)); the new text drawn; hovering 600 ms shows a tooltip `was: 120.00` | the row's number keeps its place; a 2 px `warning` bar at the gutter's right edge `(x + 46, y, 2, 24)` |
| a new row | appended after the last fetched row (and 3.7's banner, when shown, moves below it): every cell `warningFill`, cells with a default draw the default's text in `textDisabled` (`nextval(…)`, `now()`, `NULL`) until typed | the number slot reads `+` in `warning` |
| a deleted row | every cell's text struck through with a 1 px `textDisabled` line through the x-height's middle, text `textDisabled`, fill `bg0` | the number slot reads `−` in `error`; the 2 px bar `error` |
| a failed change (section 6) | fill `errorFill`, the triangle `error` | the bar `error` |

Nothing is said by colour alone: the triangle, the `+` and `−`, the strike-through and the bar's
words carry it, and each cell's accessible state says `changed`, `added`, `deleted` or `failed`
with the old value in the description.

### 4.2 The pending-changes bar
A 28 px bar docked at the results region's bottom `(260, 748, 1020, 28)` (the grid's body
shortens by 28; the horizontal scrollbar moves above the bar), `bg1`, 1 px `border` on top,
shown while at least one change is pending:

| Item | Position | Drawing |
|------|----------|---------|
| icon | 12 × 12 at (272, 756) | the warning icon `warning` |
| text | `body` `textPrimary` at (292, 753) | `3 pending changes · 2 cells changed, 1 row added` (the parts that apply: `n cells changed`, `n rows added`, `n rows deleted`; `1 pending change`) |
| `Show statements` | ghost, right of the text with a 16 px gap | toggles the `Messages` segment to the generated statements (section 5), `Alt+S` |
| `Discard` | secondary, (1092, 748, 80, 28) | `Ctrl/Cmd+Shift+Z` from the grid; with more than one change asks `Discard 3 pending changes?` (`Discard` secondary in `error` text / `Keep` default); one change discards at once with the toast `Discarded 1 change` |
| `Apply` | primary, (1180, 748, 88, 28) | `Ctrl/Cmd+S` from the grid or the editor; section 5 |

The bar is a `toolbar` named `Pending changes` after the grid in the `Tab` order; its text a
polite `status` that reads after every change (`3 pending changes`). The first change moves
nothing: the bar appears at once (no motion) and the grid's current cell stays in view.

## 5. The statements and Apply
Every change is one statement in the tab's dialect, with the tab's quoting, keyed by the key
columns' original values:

| Change | Statement |
|--------|-----------|
| a cell | `UPDATE orders SET total = 130.00 WHERE id = 1002;` (several cells of one row in one `UPDATE` with `, `; a `NULL` as `NULL`; text quoted and escaped; JSON and vectors as the dialect's literal with a cast where it needs one) |
| a new row | `INSERT INTO orders (id, customer_id, total) VALUES (DEFAULT, 7, 80.50);` (only typed columns, `DEFAULT` for the rest in dialects that take it, left out otherwise) |
| a deleted row | `DELETE FROM orders WHERE id = 1003;` |

`Show statements` lists them in `Messages` in `code`, one per line, in the order made (deletes
last), with `Copy` in the segment's top right. `Apply` runs them in the tab's kept session (9.2)
in one transaction: `BEGIN` (or inside the already open transaction, then nothing is committed by
`Apply` and the bar's text reads `Applied inside the open transaction · COMMIT to keep` in
`warning` until the transaction ends), the statements in order, each `UPDATE` and `DELETE`
checked for exactly one affected row (`0` means the row changed or vanished since the fetch:
section 6), then `COMMIT`; the summary reads `Applied 3 changes · 2 ms`, the status bar toasts
`Committed · 3 changes`, the changed cells lose their marks, new rows get their key values
re-read (`RETURNING` where the dialect has it, else re-selected by the typed key), deleted rows
leave the grid, and the result's row count updates. Deletes ask first: `Apply` with any deleted
row shows 9.1's dialog `Apply 3 changes to orders?` with the body `1 row is deleted. There is no
undo after COMMIT.` (`Apply` primary and default, `Cancel`); with none it asks nothing. On a
server connection whose other tabs hold a transaction, `Apply` still runs on this tab's session
(no rule changes); on a file, the single-writer rule of 9.2 applies and a `55P03` is shown as
section 6's error on the bar.

## 6. Errors
A statement that fails (a constraint, a type, a vanished row, a permission) stops the batch:
the transaction rolls back (`ROLLBACK`, or `ROLLBACK TO SAVEPOINT eggdb_apply` inside a user's
open transaction, which the client set before the first statement), every change stays pending,
the failed change's cells turn `errorFill` with the `error` triangle, and a strip like 2.5's
sits above the pending bar `(260, 720, 1020, 28)` (the grid shortens again): the error icon,
`code` text `Error 23505: duplicate key value violates unique constraint "orders_pkey" · row
1004` (the engine's first line, the row's gutter number), and a ghost `Show` button that
scrolls the grid to that row and makes its first failed cell current. A vanished or changed
row reads `Error: row id = 1003 changed since it was read · row 3` with a ghost `Reload` that
re-runs the statement, keeping the other pending changes whose rows still exist (changes on
rows that vanished are dropped with the toast `Dropped 1 change on a deleted row`). The strip is
an `alert`; `Esc` in the grid hides it; the next `Apply` clears it. After an error the bar's
`Apply` reads `Apply again`.

## 7. Adding and deleting rows
- **Add row:** `Insert` or `Ctrl/Cmd+Shift+N` in the grid, `Add row` in the grid's menu, or
  `Down` from the last row when the grid is editable (the spreadsheet habit): a new row appears
  at the end (4.1) with the current cell on its first typed-able column, in edit mode. A new
  row never typed into is dropped when the current cell leaves it. `Ctrl/Cmd+D` on a fetched row
  adds a new row copying its non-key values.
- **Delete row:** `Ctrl/Cmd+Delete` on the current row (or the selected rows), `Delete row` /
  `Delete 3 rows` in the menu: marks them deleted (4.1) at once, pending; the same key on a
  deleted row un-marks it. A deleted new row is simply dropped.
- **The grid's menu** (9.4) gains a group before `select`: `Edit cell` (`F2`), `Set NULL`
  (`Delete`), `Add row` (`Insert`), `Duplicate row` (`Ctrl/Cmd+D`), `Delete row` (`Ctrl/Cmd+Delete`),
  `Revert cell` / `Revert row` (drops that cell's or row's pending change), each disabled on a
  read-only grid and kept in place; height grows by 6 × 28 + 8.
- **Document cards** (data-models.md 2.2): `Edit…` opens the value view's editor on the whole
  document (saving runs one replace through the engine's document write, confirmed as a
  `statement` in the same bar: `db.orders.replaceOne({_id: …}, {…})`), `Delete…` marks the card
  deleted; the pending bar and `Apply` work the same with the document model's statements.

## 8. Keys, in one table
| Key (grid focused, editable) | Does |
|-----|------|
| `F2`, a printable key, double-click | edit the cell |
| `Enter` / `Shift+Enter` while editing | commit, move down / up |
| `Tab` / `Shift+Tab` while editing | commit, move right / left |
| `Esc` while editing | cancel the cell's edit |
| `Delete` / `Backspace` | `NULL` / empty string, pending |
| `Space` on a boolean | toggle, pending |
| `Insert`, `Ctrl/Cmd+Shift+N` | add a row |
| `Ctrl/Cmd+D` | duplicate the row |
| `Ctrl/Cmd+Delete` | delete / undelete the row(s) |
| `Ctrl/Cmd+S` | `Apply` |
| `Ctrl/Cmd+Shift+Z` | `Discard` (asks when more than one) |
| `Alt+S` | `Show statements` |
| `Ctrl/Cmd+Z` while not editing | reverts the most recent pending change (the grid's own undo stack, separate from the editor's); `Ctrl/Cmd+Shift+Z` is `Discard`, so redo of a reverted change is not offered |

Paste (`Ctrl/Cmd+V`) into an editable grid with a multi-cell clipboard (TSV) fills cells from
the current cell rightwards and downwards as pending changes, adding rows at the end when it
runs past the last, stopping at the last column; a cell that does not parse stops the paste
with section 6's strip naming the cell, the cells before it kept. The toast: `Pasted 12 cells`.

## 9. Tokens added
| Token | Light | Dark | High contrast | Used for |
|-------|-------|------|------|------|
| `warningFill` | `#FFF4D6` | `#3B3220` | `#000000` with a 1 px `warning` outline inside the cell | a pending cell or row |
| `errorFill` | `#FDE8E6` | `#44262A` | `#000000` with a 1 px `error` outline | a failed change |

`textPrimary` on `warningFill` is 14.6:1 light and 10.8:1 dark; on `errorFill` 14.9:1 and
10.4:1; `NULL` in `textDisabled` on either is exempt (the triangle carries the state).

## 10. Accessibility
| Element | Role | Name | Value / states |
|---------|------|------|------|
| editable grid | `grid` | `Results` | `aria-readonly=false`; a cell being edited has a child `textbox` focused |
| grid cell | `gridcell` | as 7 | states `changed` (description `was 120.00`), `added`, `deleted`, `failed` (description the error line); `aria-readonly` on key and blob cells |
| gutter marks | part of the row header's name | `row 3, deleted` | |
| pending bar | `toolbar` | `Pending changes` | its text a polite `status` |
| `Apply`, `Discard`, `Show statements` | `button` | the labels, accelerators | `Apply` disabled while a run is in flight |
| error strip | `alert` | the text | `Show`, `Reload` buttons |
| dialogs | `alertdialog` | `Apply 3 changes to orders?`, `Discard 3 pending changes?`, `Run Query 1 and discard 3 pending changes?` | modal |
| value view editing | `dialog`; body `textbox` editable | `<column> value` | `Save` default |

Announcements: the bar's status after each change; `Committed, 3 changes` through the status
bar toast; the strip as an alert; a cell commit through the cell's value-changed event.

## 11. Acceptance checklist
- [ ] png: `SELECT id, name FROM customers` is editable (`Editable · customers` in the status
      bar); `SELECT name FROM customers` shows the read-only banner with `Add id`; a click on it
      rewrites the editor to `SELECT id, name FROM customers` and runs it.
- [ ] png: `F2` on `Grace Hopper`, typing ` Jr`, `Enter`: the cell reads `Grace Hopper Jr` on
      `warningFill` with the triangle and the gutter bar, the current cell moved down, the bar
      at `(260, 748, 1020, 28)` reading `1 pending change · 1 cell changed`; `Messages` after
      `Alt+S` shows `UPDATE customers SET name = 'Grace Hopper Jr' WHERE id = 2;`.
- [ ] png: typing `abc` into `id` keeps the editor open with the `error` border and the strip
      `Error: "abc" is not an integer`; `Esc` restores `2`.
- [ ] png: `Insert`, typing `Ada II`, `Ctrl+Delete` on row 1: a `+` row on `warningFill` with
      `nextval(…)` in the id, row 1 struck through with `−`; `Ctrl+S` shows `Apply 2 changes to
      customers?` with the delete sentence; `Enter` applies, the toast `Committed · 2 changes`,
      the new row's id re-read, row 1 gone.
- [ ] png: a stub `23505` on the second of three statements rolls back, keeps the three changes
      pending, paints the failing row `errorFill`, shows the strip with the engine's line and `row
      1004`, and `Apply` reads `Apply again`; `Show` moves the current cell there.
- [ ] png: with a `BEGIN` run first, `Apply` leaves the bar reading `Applied inside the open
      transaction · COMMIT to keep` and the tab's `warning` dot; `ROLLBACK` from the status item
      brings the old values back on re-run.
- [ ] tree: an edited cell has a focused `textbox` child; after commit the cell's state is
      `changed` with `was Grace Hopper` in its description; the bar is a `toolbar` with three
      buttons and a `status`; the delete dialog is modal with `Apply` default.
- [ ] clipboard stub: pasting `1\tAda\n2\tGrace\n` at row 3, column 1 makes four pending cells
      and the toast `Pasted 4 cells`; a non-integer in the id column stops it at that cell.

## Decided by this design
- Editability comes from the engine's report of the source table and key columns, never from
  parsing the text; a result missing its key offers `Add id`.
- `F2`, typing and a double-click edit; `Enter` keeps opening the value view, which gets `Edit`
  and `Save`, so no existing key changes meaning.
- `Tab` moves between cells only while editing, the one exception to 3.3.
- Pending changes are marked by fill, a corner triangle, gutter marks and strike-through, with
  the old value in the tooltip and the accessible description; two tokens are added,
  `warningFill` and `errorFill`.
- The bar is 28 px at the results' bottom with `Show statements`, `Discard` and `Apply`
  (`Ctrl/Cmd+S`); statements are generated in the tab's dialect and keyed by the original key
  values; `Apply` runs them in the tab's session in one transaction, or inside the user's open
  one without committing it.
- Only deletes ask before `Apply`; a stale row stops the batch with `Reload`; a failure keeps
  every change pending and paints the failed one.
- Adding a row is `Insert`, `Ctrl/Cmd+Shift+N` or `Down` past the last row; deleting is
  `Ctrl/Cmd+Delete` and toggles; the grid has its own undo of pending changes with `Ctrl/Cmd+Z`.
