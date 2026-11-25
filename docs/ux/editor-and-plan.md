# The editor's completion and error help, and the plan view's join steps and measured Oracle plans

Designed by Claude Fable (D15) for roadmap M6e: completion and error help with `Fix` buttons in
the query editor (D275, D329; `Format statement` is built), and the plan view showing each join
step's measured time, rows and loops (D1350), Oracle's measured plans through
`DBMS_XPLAN.DISPLAY_CURSOR` and the plan tooltip in the accessibility tree (D277, D312). Every
token, rect and role not restated is [gui.md](gui.md)'s and [workspace.md](workspace.md)'s
(2.5 for errors, 5 for the plan, 9.4 for popovers, 10.9 for the editor's menu). Rects are at the
1280 × 800 golden frame, unchanged by this.

## 1. Completion

### 1.1 When it opens
The completion list opens 50 ms after a typed character that starts or extends an identifier or
keyword (a letter, `_`, a digit inside a word, `.` after an identifier, `"` or `` ` `` opening a
quoted name), when at least one candidate matches; `Ctrl+Space` opens it at once at the caret
(also with no prefix, listing everything in scope); it never opens inside a string, a comment or
a `$$` body, after `;`, or while a run or the error strip's `Fix` menu has the editor. Typing
filters it live; `Backspace` to before the word's start, `Esc`, a click elsewhere, a move of the
caret out of the word, `Space` and every key that is not a letter, digit, `_` or `.` close it.
The list closes without inserting unless accepted.

### 1.2 The list
A popover (9.4's style: `bg0`, 1 px `border`, `radiusPopover`, shadow 0 4 12 at 16 %) 360 wide,
rows 24, at most 8 rows (height `8 + 24 × n`), 4 px padding top and bottom, below the caret's
line box with a 4 px gap and its left edge at the word's start, moved left to end at x `W − 8`
and flipped above the line when it would cross the editor's bottom (the error strip included).

| Column | Position | Drawing |
|--------|----------|---------|
| kind icon | 12 × 12 at (12, row + 6) | `textSecondary`: table (gui.md's), column (a 12 × 10 rect with one vertical line at x 4), keyword (`K` in `bodyBold`, drawn as text), function (`ƒ` as text, or `f` when the font lacks it), view (the folder), alias (an arrow (2, 6)–(10, 6) with a 4 px head), schema (the database icon), snippet (two 1.5 px lines (2, 4)–(10, 4) and (2, 8)–(7, 8)) |
| label | `code` `textPrimary` at (32, row + 2) | the candidate, the typed prefix's matched characters in `bodyBold`'s weight (the mono bold face) |
| detail | `small` `textSecondary` right-aligned at x 348 | the kind and its detail: `table · 2.1M rows`, `column · integer`, `function · (text) → int`, `keyword`, `alias of orders`, `schema` |

The highlighted row is `selection` (rounded r 4, inset 4 px horizontally), the first row at
open; `Up` / `Down` move without wrapping, `PgUp` / `PgDn` by 8, `Home` / `End` to the ends;
`Enter` and `Tab` accept (insert the candidate in place of the typed prefix, quoted when the
dialect needs it or the name was typed quoted, with `(` after a function and the caret inside,
one undo step with the typing); a click accepts; hover `bg2` (fading). The list scrolls with
9.6's scrollbar when more than 8 match. Accepting a table after `FROM` or `JOIN` adds nothing
else; accepting a column adds nothing; a snippet (`SELECT * FROM … LIMIT 100;`, `INSERT INTO …
VALUES (…);`, `CREATE TABLE … (…);`) inserts its text with the caret at its first `…`, and `Tab`
jumps to the next `…` while any remain.

### 1.3 Candidates and ranking
From the connection's catalog (4.1) and the tab's text, for the tab's dialect:

| Context (the tokens before the caret) | Offered first | Then |
|------|------|------|
| after `FROM`, `JOIN`, `INTO`, `UPDATE`, `TABLE` | tables and views of the current schema (then `schema.`) | keywords that fit (`LATERAL`, `ONLY`) |
| after `SELECT`, `WHERE`, `ON`, `SET`, `BY`, `HAVING`, `AND`, `OR`, `(`, `,` | columns of the statement's tables (those named in its `FROM` so far, aliases resolved; all tables' columns when none yet, each with `table.` in the detail) | functions, keywords, tables (for `t.` completion) |
| after `alias.` or `table.` | that table's columns | |
| after `schema.` | its tables and views | |
| at a statement's start | statement keywords (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `CREATE`, `ALTER`, `DROP`, `BEGIN`, `COMMIT`, `EXPLAIN`, `WITH`) and the snippets | |
| elsewhere | keywords of the dialect, then functions, then everything above | |

Ranking inside a group: a prefix match before an infix match before a subsequence match
(`cst` matches `customers`), case-insensitive; among equals, the name used most recently in
this tab, then most often in the catalog's statistics (`pg_stat_user_tables` row counts as the
tie-breaker), then alphabetically. The list is cut at 200 candidates. Keywords are in the
dialect's case habit: upper in every dialect (the formatter's choice). The catalog is the one
already loaded; a table created in the tab's own text (a `CREATE TABLE` above the caret) is
offered from the text with the detail `in this tab`.

### 1.4 The function help pane
While the highlighted candidate is a function, or the caret is inside a function's parentheses
(`Ctrl+Shift+Space` shows it on demand), a second popover 320 wide sits to the right of the list
(or of the caret) with a 4 px gap, flipped to the left when it would cross `W − 8`: `code`
`textPrimary` the signature with the current parameter in `bodyBold`'s weight
(`date_trunc(field text, source timestamp) → timestamp`; overloads one per line, at most 4), then
`body` `textSecondary` the one-line description from the engine's function catalog
(`pg_description`, or the built-in table for the dialect's functions), at most 3 lines. It never
takes focus and closes with the list or when the caret leaves the parentheses.

### 1.5 Roles
The list is a `listbox` named `Completions` (not modal; focus stays in the editor, which exposes
the list as its active-descendant owner: `aria-controls` / `aria-activedescendant`, UIA
`ControllerFor`, AT-SPI `controller-for`), each row an `option` named by its label and described
by its detail, `selected` the highlighted one, with position and set size; opening announces
nothing by itself (the highlighted option's selection event speaks), accepting announces the
inserted text through the editor's text-changed event. The help pane is a `tooltip` named by
the signature, the editor's description while shown.

## 2. Error help and `Fix`

### 2.1 Fixes
2.5's strip shows one `Fix:` button; an error may now carry several fixes, each of one of two
kinds:

| Kind | What `Fix` does | Examples (SQLSTATE → fix) |
|------|------|------|
| `replace` | replaces the error's range in the editor with the fix's text, one undo step, the caret after it, the squiggle and strip cleared, nothing run | `42703` unknown column → `Did you mean "name"?` (the nearest column by edit distance ≤ 2, up to 3 offered); `42P01` unknown relation → `Did you mean "customers"?`; `42601` syntax error at a missing `,` or `)` → `Insert ,`; an unclosed string → `Close the string`; `42883` unknown function → `Did you mean lower()?`; `22P02` invalid text for a type → `Quote the value`; `42804` type mismatch → `Cast to integer` (`CAST(… AS integer)` around the expression); an Oracle `LIMIT` → `Use FETCH FIRST 10 ROWS ONLY` |
| `statement` | puts a new statement in a new tab `Fix 1` (2.5), not run | `42P01` → `Create table users` (`CREATE TABLE users (…);` with columns guessed from the statement's references, types `text`); `42703` on an `INSERT` → `Add column price to products`; `42501` permission → `Grant SELECT on orders to ada` (shown; needs the rights, so never run) ; `55P03` → `ROLLBACK in Query 1` (switches to that tab instead of a new one) |

Fixes are the engine's `try:` lines where it gives them (the same text as the CLI's `fix:`), plus
these client-side ones computed from the catalog; the strip never guesses from stored data
(D45). The first fix is the best (an exact-case name over a near name; the engine's over ours).

### 2.2 The strip and the menu
The strip (2.5) draws the first fix as the ghost button `Fix: name` (label `Fix: ` + the fix's
title cut at 40 characters) right-aligned 12 px from its right edge; with more than one fix the
button gains a chevron-down and reads `Fix: name ▾`, and a click, `Enter` or `Alt+Down` on it
opens 9.4's popover above the strip (left-aligned to the button, 280 wide, rows 28) listing every
fix, `replace` ones first, each with its kind as the right text (`in place`, `new tab`); `Up` /
`Down`, `Enter`, `Esc` as every menu. From the editor, `Ctrl/Cmd+.` with the caret on or inside
the squiggle opens the same menu below the squiggle's line (left-aligned to the squiggle's
start); with one fix it applies it at once. Hovering the squiggle for 600 ms shows a tooltip
(polish.md section 5) with the error's text and `Ctrl/Cmd+. for fixes`. After a `replace` fix the
status bar's left group shows `Fixed: "nmae" → "name"` for 2 s; after a `statement` fix, the new
tab is active with the caret at its text's end and the toast `Fix 1 opened; Ctrl/Cmd+Enter runs
it`. The strip's `Esc` and the rest are 2.5's.

### 2.3 Warnings
Lint findings that are not errors (a `SELECT *` on a table over 1M rows, an `UPDATE` or `DELETE`
without `WHERE`, a `JOIN` without `ON`, a comparison of a column to `NULL` with `=`) draw a 2 px
`warning` squiggle under the range when the tab's `Lint` setting is on (default on; `View >
Lint` toggles, stored `lint = true`), with the same tooltip and `Ctrl/Cmd+.` menu offering the
fix (`Add WHERE …` opens the menu's text as a `replace` that inserts `WHERE ` and leaves the
caret there; `Use IS NULL`). They never block a run; a `DELETE` or `UPDATE` without `WHERE` does
ask 9.1's dialog before running (`Run DELETE without WHERE?`, body `Every row of orders is
affected. There is no undo after COMMIT.`, `Run` secondary in `error` text, `Cancel` default)
unless the tab's transaction is already open (it can still be rolled back; the dialog is
skipped and the status bar says `DELETE ran inside the open transaction`).

### 2.4 Roles
The strip stays an `alert`; `Fix: name` is a `button` with `aria-haspopup=menu` when it has a
menu; the menu a `menu` of `menuitem`s named by their titles and described by their kind; a
warning range is an annotation of kind `warning` (UIA `AnnotationType_Warning`, AT-SPI's
`invalid` attribute with value `grammar`, ARIA `aria-describedby` on the text's span) and the
editor's `aria-invalid` stays `false` for warnings alone.

## 3. The plan view: join steps, loops and measured Oracle plans

### 3.1 Loops
With `EXPLAIN ANALYZE` (5.1) every step now carries `actual time`, `rows` and `loops` (D1350:
the outer scan once, each inner lookup or scan with `loops` equal to the rows that reached it,
`rows` per loop, and the join with the rows it produced). Row 2 of a node (5.2) becomes
`0.04 ms · rows 1 · loops 3 · buffers 12` where `0.04 ms` is the step's own time per loop as the
engine prints it; the node's **total own time** is `time × loops` minus its children's totals,
and the share bar (5.2) and the hot-node thresholds (5.3) use the totals, so a cheap lookup run
a million times reads hot. Row 2 adds `· 1.2 s total` after the loop figures when `loops > 1`,
so the number the bar shows is printed. The tooltip (5.4) and the node's description give the
same: `0.04 milliseconds per loop, rows 1, loops 3, 120 milliseconds total, 62 percent`. A hash
join's build scan, which the engine does not measure, shows row 2 without a time and the state
`unmeasured` (`· not measured` in `textSecondary`). A step whose `rows × loops` differs from
its estimate by 10× keeps 5.3's `misestimated`, computed on the totals.

### 3.2 Oracle's measured plan
In the Oracle dialect `Ctrl/Cmd+Shift+E` runs the statement (inside a transaction rolled back,
as 5.1 says), then `SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY_CURSOR(NULL, NULL, 'ALLSTATS LAST'))`,
and reads its table (`Id`, `Operation`, `Name`, `Starts`, `E-Rows`, `A-Rows`, `A-Time`,
`Buffers`, `Reads`) into the same tree: `Operation` plus `Name` is row 1 (`TABLE ACCESS FULL
EMP`), `A-Time` the total time (Oracle prints totals, so no `× loops`), `Starts` the loops,
`A-Rows` the rows, `E-Rows` the estimate for `misestimated`, `Buffers` the buffers; indentation
of `Operation` gives the parent as `DISPLAY` does (5.1). The `Text` toggle shows the
`DISPLAY_CURSOR` output as the engine prints it. A statement the engine cannot measure in this
dialect (a PL/SQL block) shows `Messages` with the CLI's `not_supported` text. SQL Server's `SET
STATISTICS PROFILE` rows (5.1) map the same way: `Executes` to loops, `Rows` to rows, with no
time (the engine gives none), so those nodes are `unmeasured` and the bar uses `EstimateCPU +
EstimateIO` as the cost form does.

### 3.3 The tooltip in the accessibility tree
5.4's node tooltip is polish.md section 5's `tooltip` role: while shown it is a child of the plan
`tree` named by its first line, and the node's description (`AXHelp`, UIA `HelpText`, AT-SPI
`description`, `aria-describedby`) points to it; when hidden the node keeps the same text as its
description, so a screen reader never needs the tooltip to be visible. `Ctrl/Cmd+Shift+Space`
shows it on the current node and `Esc` hides it; the show raises no live announcement (the
description is read with the node).

## 4. Acceptance checklist
- [ ] png: typing `SELECT na` in a `customers` tab shows the list at the word's start with
      `name` first (`column · text`, `na` in the bold face), then functions matching `na`; `Tab`
      inserts `name`; `Ctrl+Space` after `FROM ` lists the three tables with row counts; `o.`
      after `FROM orders o` lists only orders' columns.
- [ ] png: with `date_trunc(` typed the help pane shows the signature with the first parameter
      in the bold face and the one-line description; tree: the list is a `listbox` of `option`s,
      the editor's active descendant the first, the pane a `tooltip`.
- [ ] png: `SELECT nmae FROM customers` shows the strip with `Fix: Did you mean "name"? ▾`
      (two more near names); `Ctrl+.` opens the menu with `in place` / `new tab` right texts;
      `Enter` replaces `nmae` with `name`, clears the squiggle and shows `Fixed: "nmae" → "name"`
      in the status bar; `Ctrl+Z` restores it in one step.
- [ ] png: `SELECT * FROM users` shows `Fix: Create table users` (one fix, no chevron); a click
      opens `Fix 1` with the `CREATE TABLE` text, not run.
- [ ] png: `DELETE FROM orders` draws the `warning` squiggle under the statement; `Ctrl+Enter`
      shows `Run DELETE without WHERE?` with `Cancel` default; with `View > Lint` off, no squiggle
      and the dialog still shows.
- [ ] png: a stub `EXPLAIN ANALYZE` of a three-table join draws the inner lookup's row 2 as
      `0.04 ms · rows 1 · loops 3 · 120 ms total`, the bar by total own time, and the hash build
      scan with `· not measured`; tree: the node's description carries the loops and the total.
- [ ] png: an Oracle tab's `Ctrl+Shift+E` on a stub `DISPLAY_CURSOR` table draws `TABLE ACCESS
      FULL EMP` with `Starts` as loops and `A-Time` as the total; `Alt+T` shows the text form.
- [ ] tree: hovering a plan node 600 ms adds a `tooltip` child to the plan with the node's full
      text and the node's description points to it; after `Esc` the node's description is the
      same text with no tooltip node.

## Decided by this design
- Completion opens after one typed character within 50 ms, is a non-modal `listbox` driven from
  the editor, ranks prefix over infix over subsequence, and inserts with `Enter` or `Tab`.
- The function help pane is a second popover that never takes focus.
- Fixes are of two kinds, `replace` (in place, one undo step) and `statement` (a `Fix n` tab);
  several fixes make the strip's button a menu; `Ctrl/Cmd+.` is the keyboard path.
- Lint warnings use a `warning` squiggle, never block, and `DELETE` / `UPDATE` without `WHERE`
  ask before running outside a transaction.
- A step's bar and hot state use its total own time (`time × loops` minus children); the
  per-loop figure and the total are both printed.
- Oracle's measured plan comes from `DISPLAY_CURSOR` with `ALLSTATS LAST`, `Starts` as loops and
  `A-Time` as totals; SQL Server's profile rows have loops and rows but no time.
- The plan tooltip is a `tooltip` node while shown, and the node's description carries the same
  text at all times.
