# The EggDB GUI client: familiar layouts (D15)

Designed by Claude Fable. [gui.md](gui.md) defines EggDB's own layout: six regions (`toolbar`,
`sidebar`, `tabs`, `editor`, `results`, `statusbar`) in a tree of splits at 1280 × 800. A familiar
layout is another arrangement of the same regions with that client's shortcuts and visible labels.
What changes: where regions sit, their size, which are hidden, at most one extra named region, the
shortcut table and the label strings. What never changes: every token, component, state, drawing
rule, motion and accessibility rule in gui.md. Rects are `(x, y, w, h)` at 1280 × 800; a 6 px
splitter (gui.md) sits between adjacent regions, taken from the lower or right one. Unlisted
shortcuts keep EggDB's defaults unless a layout's key takes theirs (that EggDB action then moves to
the palette); "n/a" means the product has no such key, so EggDB's default applies. This file
replaces the per-layout files gui.md's "Later" section planned.

**Trademark note.** SQL Server Management Studio is a trademark of Microsoft Corporation; pgAdmin of
the pgAdmin Development Team; DBeaver of DBeaver Corp; DataGrip of JetBrains s.r.o.; MySQL Workbench
of Oracle Corporation; MongoDB Compass of MongoDB, Inc.; Redis Insight of Redis Ltd. EggDB uses these
names only to describe which client a layout is compatible with ("works like") and is not affiliated
with, endorsed by or sponsored by any of them. A layout reproduces an arrangement, keyboard shortcuts
and workflows only: none of their code, artwork, icons, colours, fonts or screenshots is used or imitated.

## Works like SQL Server Management Studio
Users expect Object Explorer on the left, a query window on the right, the grid under it with
`Results` and `Messages` tabs, `F5` to execute and `(3 rows affected)` in Messages.

| Region | Rect | Contents |
|--------|------|----------|
| `toolbar` | (0, 0, 1280, 44) | `New Query`, divider, `Execute` (primary), `Cancel` (disabled), `Parse`, `Display Estimated Execution Plan`, `Include Actual Execution Plan` (toggle), `Results to Grid` / `Results to Text`; dialect dropdown and `more` right-aligned as in gui.md |
| `sidebar` | (0, 44, 300, 732) | header `Object Explorer`, `Connect ▾`; tree `shop` › `Databases` › `shop` › `Tables` (`dbo.customers`…), `Views`, `Programmability`, `Security` |
| `tabs` | (300, 44, 980, 36) | `SQLQuery1.sql - shop (hai (52))*` style titles |
| `editor` | (300, 80, 980, 340) | gui.md editor |
| `results` | (300, 420, 980, 356) | header tabs `Results`, `Messages`, `Execution plan` (after a run with the actual plan on); Messages shows `(3 rows affected)` and `Completion time: …` |
| `statusbar` | (0, 776, 1280, 24) | left `Query executed successfully.`; right `shop`, `hai (52)`, `00:00:00`, `3 rows` |
| `properties` (extra, hidden) | (980, 44, 300, 732) | `F4`; the selected object's properties as a two-column grid; when shown, `tabs`, `editor` and `results` are 680 wide |

| Action | Windows / Linux (the product is Windows-only; on macOS EggDB maps `Ctrl` to `Cmd`, function keys unchanged) |
|--------|-----------------|
| Execute the selection, or the whole tab when nothing is selected | `F5`, `Ctrl+E`, `Alt+X` |
| Run the statement under the caret | n/a (`Ctrl+Enter` stays) |
| Display the estimated plan / include the actual plan on every run | `Ctrl+L` / `Ctrl+M` |
| New query tab | `Ctrl+N` (`Ctrl+T` becomes `Results to Text`) |
| Comment / uncomment lines | `Ctrl+K, Ctrl+C` / `Ctrl+K, Ctrl+U` |
| Format SQL | n/a (palette only; `Ctrl+Shift+F` is not bound) |

Labels: `Object Explorer`, `Execute` (not `Run`), `Results`, `Messages`, `Execution plan`,
`(3 rows affected)`, `Completion time`, `Query executed successfully.`, `Programmability`, `Security`.

## Works like pgAdmin 4
Users expect the Object Explorer tree on the left, the Query Tool as a tab with its own Execute,
Explain, Commit and Rollback buttons, and `Data Output` below the editor.

| Region | Rect | Contents |
|--------|------|----------|
| `toolbar` | (0, 0, 1280, 44) | egg mark, `File`, `Object`, `Tools`, `Help` as ghost buttons opening menus; right: connection name, dialect dropdown |
| `sidebar` | (0, 44, 300, 732) | header `Object Explorer`; tree `Servers` › `shop` › `Databases` › `shop` › `Schemas` › `public` › `Tables`, `Views`, `Sequences`, `Functions`; `Login/Group Roles` |
| `tabs` | (300, 44, 980, 36) | object tabs `Dashboard`, `Properties`, `SQL`, `Statistics`, `Dependencies`, `Dependents`, then `Query Tool - shop/hai@shop` tabs |
| `editor` | (300, 80, 980, 336) | first 36 px: the Query Tool toolbar (`Open File`, `Save`, `Filter`, `No limit ▾`, `Execute/Refresh` (primary), `Explain ▾` with `Explain Analyze`, `Commit`, `Rollback`, `Macros ▾`); editor below |
| `results` | (300, 416, 980, 360) | header tabs `Data Output`, `Messages`, `Notifications`, `Explain`, `Query History`; Data Output's row of `Add row`, `Copy`, `Paste`, `Delete`, `Save Data Changes`, `Save results to file` |
| `statusbar` | (0, 776, 1280, 24) | left `Total rows: 3 of 3`, `Query complete 00:00:00.004`; right `Ln 1, Col 44`, dialect |
| `scratchpad` (extra, hidden) | (1000, 80, 280, 336) | `Scratch Pad`, a plain text area beside the editor; when shown the editor is 700 wide |

| Action | Windows / Linux / macOS (identical: the product does not remap `Ctrl` to `Cmd`) |
|--------|-------------------------|
| Execute the query (selection or whole tab) | `F5` |
| Explain / Explain Analyze | `F7` / `Shift+F7` |
| New Query Tool tab | `Alt+Shift+Q` |
| Toggle comment / comment / uncomment | `Ctrl+/` / `Ctrl+.` / `Ctrl+,` |
| Format SQL | `Ctrl+Shift+K` |

Labels: `Object Explorer`, `Query Tool`, `Execute/Refresh`, `Explain Analyze`, `Data Output`,
`Messages`, `Notifications`, `Query History`, `Scratch Pad`, `Total rows`, `Query complete`, `Servers`.

## Works like DBeaver
Users expect a Database Navigator over a Projects panel on the left, a script editor with a vertical
run toolbar on its left edge, and results with `Grid`/`Text` presentations and an `Execution Log`.

| Region | Rect | Contents |
|--------|------|----------|
| `toolbar` | (0, 0, 1280, 44) | `New Database Connection`, `SQL Editor ▾`, `Commit`, `Rollback`, `Auto ▾` (transaction mode), active connection `shop` and schema `public ▾`; dialect dropdown right |
| `sidebar` | (0, 44, 280, 440) | header `Database Navigator`; tree `shop` › `Databases` › `shop` › `Schemas` › `public` › `Tables`, `Views`, `Indexes`, `Procedures`, `Sequences` |
| `projects` (extra) | (0, 484, 280, 292) | header `Projects`; `General` › `Scripts` (`Script.sql`, `Script-1.sql`), `Bookmarks`, `Diagrams` |
| `tabs` | (280, 44, 1000, 36) | `<shop> Script`, `<shop> Script-1` |
| `editor` | (280, 80, 1000, 320) | left 32 px column: `Execute SQL statement`, `Execute SQL script`, `Explain execution plan`, `Stop` as ghost icon buttons; editor from x 312 |
| `results` | (280, 400, 1000, 376) | header tabs: the result's name (`customers`), `Statistics`, `Output`, `Execution Log`; footer row 28 px: `Grid` / `Text` / `Record`, `Panels`, `Fetch next page`, `3 row(s) fetched - 4 ms` |
| `statusbar` | (0, 776, 1280, 24) | left `3 row(s) fetched - 4 ms`; right `Auto`, `public@shop`, `Ln 1, Col 44`, dialect |

| Action | Windows / Linux | macOS |
|--------|-----------------|-------|
| Execute SQL statement (under the caret or selected) | `Ctrl+Enter` | `Cmd+Enter` |
| Execute SQL script (whole tab) | `Alt+X` | `Option+X` |
| Explain execution plan | `Ctrl+Shift+E` | `Cmd+Shift+E` |
| New SQL script | `Ctrl+]` | `Cmd+]` |
| Toggle line comment / block comment | `Ctrl+/` / `Ctrl+Shift+/` | `Cmd+/` / `Cmd+Shift+/` |
| Format SQL | `Ctrl+Shift+F` | `Cmd+Shift+F` |

Labels: `Database Navigator`, `Projects`, `Script`, `Execute SQL statement`, `Execute SQL script`,
`Explain execution plan`, `Grid`, `Text`, `Record`, `Statistics`, `Output`, `Execution Log`, `row(s) fetched`.

## Works like DataGrip
Users expect the Database Explorer tool window on the left, a query console per data source with its
own toolbar, results in the `Services` tool window at the bottom, and `Ctrl+Enter` at the caret.

| Region | Rect | Contents |
|--------|------|----------|
| `toolbar` | (0, 0, 1280, 44) | egg mark, `shop ▾` (data source), `Search Everywhere` field, `Execute` (primary), `Stop`, `Settings`; dialect dropdown right |
| `sidebar` | (0, 44, 300, 732) | header `Database Explorer` with `Refresh`, `Jump to Query Console`; tree `shop` › `schemas` › `public` › `tables`, `views`, `routines`, `sequences`; right text shows row counts |
| `tabs` | (300, 44, 980, 36) | `console [shop]`, `console_1 [shop]` |
| `editor` | (300, 80, 980, 340) | first 32 px: console toolbar `shop ▾`, `public ▾`, `Tx: Auto ▾`, `Execute`, `Stop`, `Commit`, `Rollback`; editor below |
| `results` | (300, 420, 980, 356) | header 32 px: `Services` title, tabs `Output` and one per result (`customers`); result toolbar: `Rerun`, `Pin`, pager `1-3 of 3`, `Table` / `Tree` / `Text`, `Transpose` |
| `statusbar` | (0, 776, 1280, 24) | left `3 rows retrieved in 4 ms (execution: 1 ms, fetching: 3 ms)`; right `1:44`, `LF`, `UTF-8`, `Tx: Auto`, dialect |

| Action | Windows / Linux | macOS |
|--------|-----------------|-------|
| Execute the statement at the caret or the selection (several: a chooser) | `Ctrl+Enter` | `Cmd+Enter` |
| Run the whole console; explain plan | n/a (`Ctrl+A`, `Ctrl+Enter`); n/a (context menu `Explain Plan`, `Explain Analyze`) | n/a (`Cmd+A`, `Cmd+Enter`); same |
| Jump to / open a query console | `Ctrl+Shift+F10` | `Cmd+Shift+F10` |
| Comment line / block | `Ctrl+/` / `Ctrl+Shift+/` | `Cmd+/` / `Cmd+Shift+/` |
| Reformat code | `Ctrl+Alt+L` | `Cmd+Option+L` |

Labels: `Database Explorer`, `console [shop]`, `Services`, `Output`, `Execute`, `Tx: Auto` /
`Tx: Manual`, `Table`, `Tree`, `Text`, `Transpose`, `rows retrieved in`, lower-case tree groups.

## Works like MySQL Workbench
Users expect the Navigator with `SCHEMAS` on the left, an execute pair in the toolbar, a `Result
Grid` with vertical tabs on its right edge, and an `Output` log of every action below.

| Region | Rect | Contents |
|--------|------|----------|
| `toolbar` | (0, 0, 1280, 44) | `New SQL tab`, `Open`, `Save`, divider, `Execute` (all or selection, primary), `Execute current statement`, `Explain current statement`, `Stop`, `Toggle autocommit`, `Commit`, `Rollback`, `Limit to 1000 rows ▾`; right: dialect dropdown and panel toggles `Sidebar`, `Output area`, `Secondary sidebar` |
| `sidebar` | (0, 44, 260, 732) | header `Navigator`, section `SCHEMAS` with the filter field, tree `shop` › `Tables`, `Views`, `Stored Procedures`, `Functions`; bottom segment `Administration` / `Schemas` |
| `tabs` | (260, 44, 1020, 36) | `Query 1`, `SQL File 2*` |
| `editor` | (260, 80, 1020, 280) | gui.md editor |
| `results` | (260, 360, 1020, 266) | right 96 px column: `Result Grid`, `Form Editor`, `Field Types`, `Query Stats`, `Execution Plan` as stacked `small` labels (horizontal text; the toolkit draws no rotated text); footer row: `Result 3 ×`, `Read Only`, `Apply`, `Revert` |
| `output` (extra) | (260, 626, 1020, 150) | header `Output` with `Action Output ▾`; grid columns `#`, `Time`, `Action`, `Message`, `Duration / Fetch`; a check or error icon per row |
| `statusbar` | (0, 776, 1280, 24) | left `3 row(s) returned 0.0004 sec / 0.0000 sec`; right `Ln 1, Col 44`, dialect |

| Action | Windows / Linux | macOS |
|--------|-----------------|-------|
| Execute current statement | `Ctrl+Enter` | `Cmd+Enter` |
| Execute all or selection | `Ctrl+Shift+Enter` | `Cmd+Shift+Enter` |
| Explain current statement | `Ctrl+Alt+X` | `Cmd+Option+X` |
| New query tab | `Ctrl+T` | `Cmd+T` |
| Toggle comment | `Ctrl+/` | `Cmd+/` |
| Beautify query (format; the sidebar toggle moves to the toolbar's panel buttons) | `Ctrl+B` | `Cmd+B` |

Labels: `Navigator`, `SCHEMAS`, `Administration`, `Execute current statement`, `Explain current
statement`, `Result Grid`, `Form Editor`, `Field Types`, `Query Stats`, `Execution Plan`, `Action Output`.

## Works like MongoDB Compass
Users expect collections in a full-height sidebar, a tab per collection with `Documents`, `Schema`
and `Indexes` sub-tabs, a `Filter` bar with `Find`, documents as list, JSON or table, and a shell at
the bottom. The editor is the filter bar (MongoDB query language); every EggDB table is browsable
this way, rows as documents.

| Region | Rect | Contents |
|--------|------|----------|
| `sidebar` | (0, 0, 300, 800) | egg mark and connection `shop` with the connected dot; `Search connections`; `My Queries`, `Databases` › `shop` › `customers`, `orders`, `products`; `Create database +` |
| `tabs` | (300, 0, 980, 36) | workspace tabs `Databases`, `shop.customers`, `+` |
| `toolbar` | (300, 36, 980, 72) | line 1: `shop.customers`, `Documents 3`, `Indexes 1`, `Refresh`; line 2: sub-tabs `Documents`, `Aggregations`, `Schema`, `Indexes`, `Validation`; dialect dropdown right |
| `editor` | (300, 108, 980, 92) | filter bar: label `Filter`, one-line input with placeholder `{ field: 'value' }`, `Options ▾`, `Reset`, `Find` (primary), `Explain`; with Options open the region is 148 tall and adds `Project`, `Sort`, `Collation`, `Skip`, `Limit`, `Max Time MS` |
| `results` | (300, 200, 980, 568) | header row `Add Data ▾`, `Export Data ▾`, `Update`, `Delete`, right `1 – 3 of 3` pager and `List` / `JSON` / `Table`; then document cards (List), one JSON block per document (JSON) or the gui.md grid (Table) |
| `shell` (extra) | (300, 768, 980, 32) | collapsed bar `>_MONGOSH`; expanded to (300, 560, 980, 240) with `results` (300, 200, 980, 360); mono prompt `shop>` |
| `statusbar` | hidden | the connection state lives on the sidebar's connection row |

| Action | Windows / Linux | macOS |
|--------|-----------------|-------|
| Run the filter (`Find`) | `Enter` in the filter | `Enter` |
| Run all / run statement / comment / format; explain | n/a; the `Explain` button (no key) | same |
| New tab / close tab; toggle the embedded shell | `Ctrl+T` / `Ctrl+W`; ``Ctrl+` `` | `Cmd+T` / `Cmd+W`; ``Ctrl+` `` |

Labels: `Databases`, `Collections`, `My Queries`, `Documents`, `Aggregations`, `Schema`, `Indexes`,
`Validation`, `Filter`, `Options`, `Find`, `Reset`, `Add Data`, `Export Data`, `List`, `JSON`, `Table`, `MONGOSH`.

## Works like Redis Insight
Users expect a narrow rail (`Browser`, `Workbench`, `Analysis Tools`, `Pub/Sub`), a Workbench editor
submitted whole that answers with one result card per command, newest on top, and `CLI`, `Command
Helper` and `Profiler` along the bottom. The rects are the Workbench; `Browser` turns `editor` into
the key list and `results` into the selected key's details.

| Region | Rect | Contents |
|--------|------|----------|
| `toolbar` | (0, 0, 1280, 44) | `‹ Databases`, database name `shop`, `localhost:6379`, `db0 ▾`; right: `Insights`, dialect dropdown (RESP commands) |
| `sidebar` | (0, 44, 60, 732) | icon rail, items 64 tall with `small` labels: `Browser`, `Workbench` (selected), `Analysis`, `Pub/Sub` |
| `tabs` | hidden | the Workbench has one editor |
| `editor` | (60, 44, 1220, 180) | mono editor from x 72; `Run` (primary, icon only, 28 × 28) at (1240, 52); footer row 32 px: `Raw mode` and `Group results` toggles left, `Clear Results` right |
| `results` | (60, 224, 1220, 552) | result cards 8 px apart, `bg1`, `radiusCard`: header `> GET user:1`, right text `0.4 ms`, `Table` / `Text`, `Full screen`, collapse chevron, `×`; body in `code` or a gui.md grid |
| `cli` (extra, hidden) | (60, 536, 1220, 240) | shown by `CLI` in the status bar; mono prompt `localhost:6379>`; `results` becomes (60, 224, 1220, 312) |
| `statusbar` | (0, 776, 1280, 24) | left pills `CLI`, `Command Helper`, `Profiler`; right `Connected · localhost:6379 · db0`, dialect |

| Action | Windows / Linux | macOS |
|--------|-----------------|-------|
| Run the editor (every command, one card each) | `Ctrl+Enter` | `Cmd+Enter` |
| Run statement / explain / new tab / comment / format | n/a (`Enter` is a new line) | n/a |

Labels: `Browser`, `Workbench`, `Analysis Tools`, `Pub/Sub`, `Keys`, `Key`, `Type`, `TTL`, `Filter by
Key Type`, `Raw mode`, `Group results`, `Clear Results`, `CLI`, `Command Helper`, `Profiler`, `Insights`.

## Summary
| Layout | Sidebar | Editor / results split | Results tab names |
|--------|---------|------------------------|-------------------|
| Works like SQL Server Management Studio | left | editor above results | `Results`, `Messages`, `Execution plan` |
| Works like pgAdmin 4 | left | editor above results | `Data Output`, `Messages`, `Notifications`, `Explain`, `Query History` |
| Works like DBeaver | left (Navigator over Projects) | editor above results | result name, `Statistics`, `Output`, `Execution Log` |
| Works like DataGrip | left | editor above results (`Services`) | `Output`, one per result |
| Works like MySQL Workbench | left | editor above results, `Output` below both | `Result Grid`, `Form Editor`, `Field Types`, `Query Stats`, `Execution Plan` (right column) |
| Works like MongoDB Compass | left, full height | filter bar above documents | `List`, `JSON`, `Table` views under `Documents`, `Aggregations`, `Schema`, `Indexes`, `Validation` |
| Works like Redis Insight | left rail | editor above result cards | none (one card per command) |

## How switching works
- `View > Layout` lists `EggDB` first, then the seven `Works like …` entries, the active one with a
  check icon; the palette has the same entries, and `Ctrl+K L` (`Cmd+K L` on macOS) opens the list
  directly. `Enter` applies, `Esc` keeps the current layout. The switch is instant (no motion) and
  keeps every open tab, its text, caret, results and connection.
- The choice is stored per user in the client's settings, beside the theme and recent connections,
  and applies at the next start in every window. The shortcut table, labels and status bar strings
  switch with it; the palette lists the layout's names and EggDB's, so `Run`, `Execute` and `Find`
  all find the same action. Splitter positions, hidden regions and column widths are remembered
  per layout, so switching back restores what the user had.
- While a familiar layout is active, the status bar's right group gains a `Works like <product>`
  item; clicking it opens the picker and its tooltip carries the trademark note. On first start the
  connect screen's sidebar shows one row, `Coming from another client? Pick a layout`, until used or dismissed.
