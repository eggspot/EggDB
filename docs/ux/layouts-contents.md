# The familiar layouts' own region contents, hidden regions, splitters, `View > Layout` and the connect screen's row

Designed by Claude Fable (D15) for roadmap M6e, finishing [gui-layouts.md](gui-layouts.md)
(D309): each layout's own region contents (pgAdmin's Query Tool toolbar, DBeaver's run column,
Compass's filter bar and document cards, Redis Insight's result cards, Workbench's output rows,
DataGrip's Services header, SSMS's `F4` properties), their hidden regions and toggles, splitter
positions per layout, the `View` menu with `View > Layout`, and the connect screen's `Coming from
another client?` row. Every token, component, rect and rule not restated is gui.md's,
workspace.md's and gui-layouts.md's; the trademark note of gui-layouts.md applies: a layout
copies an arrangement, shortcuts and workflows only, never code, artwork, icons, colours or
screenshots. Rects are at 1280 × 800.

## 1. Works like SQL Server Management Studio
- **Toolbar** `(0, 0, 1280, 44)`, buttons 28 tall at y 8, 8 px apart from x 12: `New Query`
  (secondary, 88 wide), divider, `Execute` (primary, 84; `F5`), `Cancel` (secondary, 72,
  disabled until a run; the stop icon), `Parse` (secondary, 64; runs the statement with the
  engine's parse-only form, `Messages` reads `Commands completed successfully.` or the error),
  `Display Estimated Execution Plan` (ghost icon button 28 × 28 with the plan icon of gui.md's
  `Explain`; tooltip the full name and `Ctrl+L`), `Include Actual Execution Plan` (ghost icon
  toggle, `pressed` while on, `Ctrl+M`: every run also fills `Execution plan`), `Results to Grid`
  / `Results to Text` (a pair of ghost icon toggles, one pressed; `Ctrl+D` / `Ctrl+T`); right:
  the dialect dropdown and `more` as gui.md.
- **Object Explorer** header `(0, 44, 300, 32)`: `bodyBold` `Object Explorer` at (12, 51),
  `Connect ▾` ghost dropdown right-aligned (opens the connect screen's server segment as a dialog
  card). Tree from y 76, gui-layouts.md's rows.
- **Results** header tabs `Results`, `Messages`, `Execution plan`; `Results to Text` turns the
  grid into a `code` text block (fixed columns padded to width, a `-----` line under the
  header, `(3 rows affected)` at the end, as the product's text output reads) with the editor's
  scrollbars and copy. `Messages` reads `(3 rows affected)` then `Completion time: 2026-11-25
  14:12:03.1234567` in `code`.
- **Properties** (extra, hidden) `(980, 44, 300, 732)`, toggled by `F4` and `View > Properties
  Window`; a two-column grid (rows 24, `Name` 120 wide in `bg1`, `Value` the rest) of the
  selected tree row's properties as the inspector's blocks give them (`Name`, `Schema`,
  `Row count`, `Size`, `Created`, then `Columns` as a group row with one child row per column
  `name: type`), read-only, cells copyable; header `(980, 44, 300, 32)` `bodyBold` `Properties`
  with the object's name after it in `body` `textSecondary` and a close icon. Its splitter is
  polish.md 6.2's. With it shown the centre is 680 wide.
- **Status bar**: left `Query executed successfully.` (or `Query completed with errors.` in
  `error`), right `shop`, `ada (52)` (the session's PID), `00:00:00` (the last run's elapsed as
  `hh:mm:ss`), `3 rows`.

## 2. Works like pgAdmin 4
- **Query Tool toolbar**, the editor's first 36 px `(300, 80, 980, 36)`, `bg2`, 1 px `border`
  below, buttons 28 tall at y 84, 8 px apart from x 312: `Open File` and `Save` (ghost icon
  buttons 28 × 28: a folder and a floppy drawn from rects), divider, `Filter` (ghost icon: a
  funnel of three lines; opens a one-line input under the toolbar for a `WHERE` applied to a
  table opened from the tree), `No limit ▾` (ghost dropdown 96 wide: `No limit`, `1000 rows`,
  `500 rows`, `100 rows`, adding `LIMIT` to a table's `View/Edit Data` run), divider,
  `Execute/Refresh` (primary, 128; `F5`), `Explain ▾` (secondary dropdown 92: `Explain` `F7`,
  `Explain Analyze` `Shift+F7`, and check items `Verbose`, `Costs`, `Buffers`, `Timing`), `Commit`
  and `Rollback` (secondary, 76 each, enabled while the tab's transaction is open; they run 9.2's
  statements), `Macros ▾` (ghost dropdown 80: the palette's snippets); right: `Scratch Pad` (ghost
  icon toggle) and a `Autocommit` check item in the dropdown. The editor's text starts at y 116.
- **Object tabs** in `tabs`: `Dashboard` (the Monitor page's `Status`, admin-screens.md 3.1),
  `Properties` (the inspector's blocks as a page), `SQL` (the DDL as a read-only page), `Statistics`
  (a two-column grid of the table's `pg_stat` figures), `Dependencies` and `Dependents` (grids of
  the catalog's dependency rows), then the `Query Tool - shop/ada@shop` tabs.
- **Data Output** footer row `(300, 416, 980, 32)` inside the results header: ghost icon buttons
  `Add row` (`+`), `Copy`, `Paste`, `Delete`, `Save Data Changes` (the pending bar's `Apply`
  under this name; grid-editing.md), `Save results to file` (the Export dialog); the pending
  bar of grid-editing.md is not drawn: its count shows after the buttons as `3 pending changes`
  and `Save Data Changes` is enabled.
- **Scratch Pad** (extra, hidden) `(1000, 80, 280, 336)`: a plain editor (no syntax) with the
  header `Scratch Pad` and a close icon, its text kept per connection in `gui-settings`
  (`pgadmin.scratchpad.<connection>`); its splitter polish.md 6.2's.
- **Results header tabs** `Data Output`, `Messages`, `Notifications` (the engine's `NOTIFY`
  payloads received on the tab's session, rows `channel · payload · time`), `Explain` (the plan
  view), `Query History` (a list of this tab's runs: rows 44 with the statement's first line,
  `small` time and `3 rows · 4 ms`; `Enter` puts it in the editor, `Copy` copies; kept per
  connection, 200 at most).
- **Status bar**: `Total rows: 3 of 3` (`of 2,104,311` with the banner), `Query complete
  00:00:00.004`; right `Ln 1, Col 44`, dialect.

## 3. Works like DBeaver
- **Run column** `(280, 80, 32, 320)`, `bg1`, 1 px `border` at x 311: ghost icon buttons 28 × 28
  from y 84, 4 px apart: `Execute SQL statement` (the run triangle; `Ctrl+Enter`), `Execute SQL
  script` (the triangle with a bar; `Alt+X`), `Explain execution plan` (the plan icon;
  `Ctrl+Shift+E`), `Stop` (the stop square, disabled until a run); tooltips with the names and
  keys. The editor's gutter starts at x 312.
- **Projects** (extra, shown) `(0, 484, 280, 292)`: header `Projects` `bodyBold` at (12, 491) with
  a collapse chevron; tree `General` › `Scripts` (one row per saved tab file, `Script.sql`,
  `Script-1.sql`, from the client's `scripts/` folder in the state directory), `Bookmarks`
  (saved tree rows: `Add bookmark` in the row menu), `Diagrams` (saved ER views, er-view.md).
  Collapsing it (chevron, or `View > Projects`) gives the navigator the full height; the
  navigator/projects splitter is polish.md 6.2's.
- **Results** header tabs: the result's name (`customers`, the source table, else `Result 1`),
  `Statistics` (a two-column grid: `Query`, `Updated rows`, `Execute time`, `Fetch time`, `Total
  time`, `Start time`, `Finish time`), `Output` (the engine's notices and `RAISE` output),
  `Execution Log` (rows per run: time, statement, result or error, duration). Footer row
  `(280, 748, 1000, 28)`: `Grid` / `Text` / `Record` segments at x 292 (`Record` shows one row as
  a two-column `Column` / `Value` grid with `←` `→` buttons and `Row 1 of 3`), `Panels ▾` (a
  dropdown toggling a `Value` panel at the results' right, 280 wide: the value view docked),
  `Fetch next page` (ghost; polish.md 1's next page), right `3 row(s) fetched - 4 ms`.
- **Toolbar**: `New Database Connection` (secondary icon+label), `SQL Editor ▾` (new script,
  open recent script), `Commit`, `Rollback`, `Auto ▾` (`Auto` / `Manual`: Manual starts a
  transaction before the first write and keeps it, as 9.2), the active connection `shop` and
  schema `public ▾` as secondary dropdowns; right the dialect dropdown.

## 4. Works like DataGrip
- **Services header** `(300, 420, 980, 32)` (the results header): `bodyBold` `Services` at (312,
  427) with a 1 px `border` divider at x 392, then tabs `Output` (the console's log: each run's
  statement, `3 rows retrieved in 4 ms` and errors, in `code`) and one per result named by its
  source (`customers`), closable (`×` on hover), at most 10 then the oldest unpinned closes;
  right: ghost icon buttons `Hide` (collapses the results to the header; `Shift+Esc`) and `Settings`.
- **Result toolbar**, a 28 px row under the Services header `(300, 452, 980, 28)`, `bg1`: `Rerun`
  (ghost icon; runs the result's statement again), `Pin` (ghost icon toggle; a pinned result is
  not replaced by the next run, which opens a new result tab), pager `1-3 of 3` with `◀` `▶`
  ghost buttons (pages of 500: polish.md 1's pages) and a `Page size ▾`, segments `Table` /
  `Tree` (the JSON tree of data-models.md 2.2 for the rows) / `Text`, `Transpose` (ghost icon
  toggle: columns become rows, `Column` / `Row 1` … headers), right: a filter input `Filter rows`
  (a `WHERE` pushed into the query as 3's rule says) and `Export` (icon). The grid starts at y 480.
- **Console toolbar**, the editor's first 32 px: `shop ▾`, `public ▾`, `Tx: Auto ▾` (`Auto` /
  `Manual`), `Execute` (primary icon, `Ctrl+Enter`), `Stop`, `Commit`, `Rollback`; the editor's
  text from y 112.
- **Toolbar**: the egg mark, `shop ▾` (data source), a `Search Everywhere` input 240 wide (the
  palette, `Shift Shift`), `Execute` (primary), `Stop`, `Settings` (ghost icon).
- **Status bar**: `3 rows retrieved in 4 ms (execution: 1 ms, fetching: 3 ms)` (the engine's
  two figures); right `1:44`, `LF`, `UTF-8`, `Tx: Auto`, dialect.

## 5. Works like MySQL Workbench
- **Result Grid's side column** `(1184, 360, 96, 266)`: `small` labels stacked 24 px apart from y
  372, each a toggle (`pressed` fill `bg2`, 2 px `accent` stripe at its left edge): `Result
  Grid`, `Form Editor` (DBeaver's `Record` form), `Field Types` (a grid `Field`, `Schema`,
  `Table`, `Type`, `Character Set`, `Display Size`, `Precision`, `Scale`), `Query Stats` (the
  statistics grid), `Execution Plan` (the plan view). Footer `(260, 598, 924, 28)`: `Result 3 ×`
  tabs (one per run, closable), `Read Only` (or `Editable`, grid-editing.md 1), `Apply`, `Revert`
  (the pending bar's `Apply` / `Discard`; the bar is not drawn, the count shows beside `Apply`).
- **Output** (extra, shown) `(260, 626, 1020, 150)`: header `(260, 626, 1020, 28)` `bodyBold`
  `Output` with `Action Output ▾` (`Action Output` / `Text Output` / `History Output`) and a
  close icon; a grid (rows 24) `#` (right), `Time` (`14:12:03`), `Action` (the statement, cut),
  `Message` (`3 row(s) returned` or the error), `Duration / Fetch` (`0.0004 sec / 0.0000 sec`),
  with the check icon in `success` or the error icon in `error` before `#` (and the word in
  `Message`); one row per run, the newest last and scrolled into view; a double-click puts the
  action in the editor. `View > Output area` (the toolbar's `Output area` toggle) hides it; its
  splitter polish.md 6.2's.
- **Toolbar**: `New SQL tab`, `Open`, `Save` (ghost icons), divider, `Execute` (primary with
  the lightning icon drawn as a 1.5 px zigzag; `Ctrl+Shift+Enter`), `Execute current statement`
  (ghost icon; `Ctrl+Enter`), `Explain current statement` (ghost icon; `Ctrl+Alt+X`), `Stop`,
  `Toggle autocommit` (ghost icon toggle), `Commit`, `Rollback`, `Limit to 1000 rows ▾`
  (`Don't Limit`, `10`, `50`, `100`, `200`, `300`, `400`, `500`, `1000`, `2000`, `5000`, `10000`,
  `50000`: appended as `LIMIT` to a `SELECT` without one); right: the dialect dropdown and three
  ghost icon toggles `Sidebar`, `Output area`, `Secondary sidebar` (the inspector).
- **Navigator**: header `Navigator`, section label `SCHEMAS` in `small` `textSecondary`
  upper-case at (12, 84) with the filter field under it; bottom segment `(0, 744, 260, 32)`
  `Administration` / `Schemas` (Administration lists `Server Status` (Monitor › Status), `Client
  Connections` (Sessions), `Users and Privileges` (the Users page), `Data Import/Restore` (the
  Restore wizard), `Data Export` (Export)).
- **Status bar**: `3 row(s) returned 0.0004 sec / 0.0000 sec`; right `Ln 1, Col 44`, dialect.

## 6. Works like MongoDB Compass
- **Filter bar** `(300, 108, 980, 92)`, `bg1`, 1 px `border` below: label `Filter` `body` at
  (312, 128), the one-line `code` input `(360, 122, 640, 28)` with placeholder `{ field: 'value'
  }`, `Options ▾` (ghost, 1008), `Reset` (ghost), `Find` (primary, 72, `Enter` in the input),
  `Explain` (secondary); with `Options` open the region is 148 tall and a second row at y 164
  holds `Project`, `Sort`, `Collation`, `Skip`, `Limit`, `Max Time MS` as labelled inputs (120
  wide each, 8 px apart, `Project` and `Sort` 160). The input accepts the product's relaxed
  JSON (unquoted keys, single quotes) and runs data-models.md 2's find with the options;
  `Aggregations` (the sub-tab) swaps the bar for the pipeline form with stage cards (`$match`,
  `$group`, … each a card with a stage dropdown and a `code` box, `Add stage`, a `Run` and the
  preview of the first 10 documents under each stage).
- **Results** header row `(300, 200, 980, 36)`: `Add Data ▾` (`Insert document` opens the value
  view's editor on `{}`; `Import JSON or CSV file` opens the Import wizard on the collection),
  `Export Data ▾` (`Export query results`, `Export the full collection`: the Export dialog),
  `Update` and `Delete` (bulk, for the filter: 9.1's dialog with the count `Delete 42 documents
  matching the filter?`), right: `1 – 20 of 1,204` pager with `◀` `▶` (pages of 20) and segments
  `List` / `JSON` / `Table`.
- **Document cards** (`List`): data-models.md 2.2's cards, each with a hover row of ghost icon
  buttons at the card's top-right: `Edit document` (the card's body becomes editable in place,
  with `Cancel` / `Update` buttons at its bottom: a pending change as grid-editing.md 7), `Copy
  document`, `Clone document` (a new card in edit with the same body and a new `_id`), `Delete
  document` (the card marked deleted with a `Delete` / `Cancel` pair). The pending bar is not
  drawn; each card applies on its own `Update`, as the product does, with the statement shown in
  the status bar's left group for 2 s.
- **Sub-tabs** `Documents`, `Aggregations`, `Schema` (per field: name, type distribution as a
  horizontal bar of `chart1` segments with the type names and percentages printed, sample
  values), `Indexes` (a grid `Name`, `Type`, `Size`, `Usage`, `Properties`, with `Create Index`),
  `Validation` (the collection's JSON schema rule in an editor with `Update`).
- **Shell** (extra) `(300, 768, 980, 32)` collapsed bar `>_MONGOSH` (`bodyBold` at (312, 775)) with
  a chevron; expanded to `(300, 560, 980, 240)` with the results above it (`(300, 200, 980,
  360)`): a mono prompt `shop>` running the document form's language line by line, output in
  `code`, as `eggdb shell`'s document mode (cli.md 15); ``Ctrl+` `` toggles; its splitter polish.md
  6.2's.
- **Sidebar** full height, with the connection row's dot; `Create database +` at the bottom.

## 7. Works like Redis Insight
- **Result cards** `(60, 224, 1220, 552)`: one card per command of the last run, newest on top,
  8 px apart, `bg1`, `radiusCard`: header `(x, y, w, 36)` with `> GET user:1` `code` at (12, 8),
  right: `0.4 ms` `small` `textSecondary`, segments `Table` / `Text` (the reply as a small grid
  for arrays, hashes and streams, as data-models.md 8's tables; `Text` the RESP reply as
  `redis-cli` prints it), `Full screen` (ghost icon: the card alone fills the results), a
  collapse chevron and `×`; body `(x, y + 36, w, h − 36)` `bg0` inside the card with 12 px
  padding. `Group results` (the editor's footer toggle) folds the run's cards into one with a
  `12 commands · 3.2 ms` header; `Raw mode` shows every reply as `Text`; `Clear Results` removes
  every card. A card's body over 300 px scrolls inside the card (9.6's scrollbar). Keys: the
  list is a `Tab` stop; `Up` / `Down` move between cards, `Right` / `Left` expand / collapse,
  `Delete` closes, `Ctrl/Cmd+C` copies the reply text.
- **Browser** (the rail's first item): `editor` becomes the key list: a header with `Filter by
  Key Type ▾` and the key pattern input (`*`), `Scan more`, and the keys grid of 10.10 with
  columns `Key`, `Type` (as a `bg2` pill with the word), `TTL`, `Size`; `results` becomes the
  selected key's details: header `Key` name, `Type`, `TTL` (editable: `Edit TTL`), `Size`, and
  the value as the aggregate table (data-models.md 8) with `Add field` / `Remove` for a hash,
  `Edit value` for a string; every edit shows its command in the status bar for 2 s and runs
  at once (the product's way; no pending bar).
- **CLI** (extra, hidden) `(60, 536, 1220, 240)`, shown by the status bar's `CLI` pill: a mono
  prompt `localhost:6379>` running one command per line with the reply under it, as `eggdb shell`'s
  RESP mode; `Command Helper` (a second pill) opens a 280-wide panel at the CLI's right with the
  command's syntax and summary from the public command list as the user types; `Profiler` (a
  third) streams `MONITOR` lines into the CLI region. Each splitter polish.md 6.2's.
- **Toolbar**: `‹ Databases` (ghost, returns to the connect screen's recent list), the database
  name, `localhost:6379`, `db0 ▾` (`SELECT n`); right `Insights` (ghost; opens the Monitor page's
  Status for the RESP server: keys, memory, clients, ops/s from `INFO`) and the dialect dropdown
  reading `RESP commands`.

## 8. The `View` menu and `View > Layout`
gui.md's `more` button (`(1240, 8, 28, 28)`) opens the client's menu (9.4's popover, 240 wide,
rows 28, submenus opening to the left with a 4 px gap) with the groups `File`, `View`, `Help`;
on macOS the same items also live in the native menu bar's `File`, `View` and `Help` menus
(`NSMenu`, our own declarations), and on every OS `Alt+F`, `Alt+V`, `Alt+H` open the group as a
submenu of the `more` popover. The `View` group:

| Item | Shortcut | Kind |
|------|----------|------|
| `Layout ▸` | `Ctrl/Cmd+K L` opens the picker directly | a submenu listing `EggDB` and the seven `Works like …` entries as `menuitemradio`s with the active one checked; `Pick a layout…` at its end opens gui-layouts.md's picker |
| `Theme ▸` | | `System`, `Light`, `Dark`, `High contrast` as radios (localization-and-window.md 3) |
| `Language ▸` | | radios of the shipped languages (localization-and-window.md 1) |
| `Sidebar` / `Results` / `Inspector` / `Assistant` | `Ctrl/Cmd+B`, `J`, `I`, `Shift+A` | check items |
| the layout's own toggles | | `Properties Window` (`F4`) in SSMS; `Scratch Pad` in pgAdmin; `Projects` in DBeaver; `Output area`, `Secondary sidebar` in Workbench; `Shell` in Compass; `CLI`, `Command Helper`, `Profiler` in Redis Insight: check items, drawn only in that layout |
| `Density ▸` | | `Compact` (the default), `Comfortable` (32 px rows, 13 px text, client-brief.md 5) |
| `Reduce motion` | | a check item (polish.md 7.4) |
| `Lint` | | a check item (editor-and-plan.md 2.3) |

Picking a layout from the submenu applies it as the picker's `Apply` does (instant, every tab
kept, the setting written). The layout's hidden regions remember their shown state per layout
(`ssms.properties_shown`, `pgadmin.scratchpad_shown`, `dbeaver.projects_shown`,
`mysql-workbench.output_shown`, `mongodb-compass.shell_shown`, `redis-insight.cli_shown`, each
`true` / `false`) beside polish.md 6.2's sizes. Roles: the `more` button `aria-haspopup=menu`
named `Menu`; the popover a `menu`; submenus `menu`s under `menuitem`s with `aria-haspopup`;
check items `menuitemcheckbox`, radios `menuitemradio`; `Right` opens a submenu, `Left` closes it.

## 9. The connect screen's `Coming from another client?` row
In the connect screen's sidebar (connections.md 4), docked at the bottom `(0, 744, 260, 32)`,
`bg2`, 1 px `border` above: the text `Coming from another client? Pick a layout` in `small`
`accent` at (12, 753), underlined on hover, cut with `…` at x 224; a close icon (8 px, `textSecondary`,
24 × 24 hit) at `(228, 748)`. Click, `Enter` or `Space` open the layout picker; the close icon
(or `Delete` on the focused row) dismisses it for good (`layout_hint_dismissed = true`), as does
applying any layout from the picker, so the row shows only until used. It is a `Tab` stop after
the recent list and before the card, a `link` named `Coming from another client? Pick a layout`
with a sibling `button` `Dismiss`. The card's own link `Where are you coming from?` stays. With
a familiar layout active, the connect screen is laid out by that layout's regions (the
sidebar at the layout's width and the card centred in the rest; the Compass sidebar full
height; the Redis Insight rail at 60 with the recent list as its `Databases` page).

## 10. Acceptance checklist
- [ ] png per layout at 1280 × 800 after running the golden statement, light: SSMS with `F4`
      (properties at `(980, 44, 300, 732)` listing `orders`' rows and the centre 680 wide) and
      `Results to Text` showing the padded text; pgAdmin's Query Tool toolbar at `(300, 80, 980,
      36)` with its buttons and `Explain ▾` open on `Shift+F7`'s item; DBeaver's run column with
      four icon buttons and the `Record` form; DataGrip's Services header with `Output` and
      `customers` tabs and the result toolbar's pager; Workbench's side column with `Field Types`
      pressed and the Output grid's check-icon row; Compass's filter bar with `Options` open (148
      tall) and three cards with the hover buttons on the first; Redis Insight's three result
      cards with `Table` on a `HGETALL` reply and the `CLI` region shown.
- [ ] settings stub: toggling each hidden region writes its `_shown` key; dragging each extra
      splitter writes polish.md 6.2's size key; switching layouts and back restores both.
- [ ] png: the `more` menu open with `View ▸` expanded and `Layout ▸` showing eight radios with
      the active one checked, plus the SSMS layout's `Properties Window` item only in SSMS; tree:
      `menu` → `menuitem` with `aria-haspopup` → `menu` of `menuitemradio`s.
- [ ] png: the connect screen's row at `(0, 744, 260, 32)` with its text and close icon; `Enter`
      opens the picker; the close icon hides it and the setting reads `layout_hint_dismissed =
      true`; after applying `pgAdmin 4` the row is gone and the connect screen's sidebar is 300
      wide.
- [ ] tree: every layout's new buttons carry the product's shortcut as the accelerator (`F4`,
      `Shift+F7`, `Alt+X`, `Ctrl+Alt+X`, ``Ctrl+` ``), and the toggles report `pressed`.

## Decided by this design
- Each layout's extra controls are EggDB actions under the product's names, drawn with gui.md's
  components and our own icons from lines and rects; where the product applies edits at once
  (Compass cards, Redis Insight's browser) the layout does too, showing the statement in the
  status bar, and where it batches (pgAdmin's `Save Data Changes`, Workbench's `Apply`) the
  pending bar's actions take those names.
- Hidden regions remember `shown` per layout beside their sizes; their toggles live in `View`
  only while that layout is active.
- The client's menu is the `more` popover with `File`, `View`, `Help` groups (and macOS's menu
  bar); `View > Layout` is a radio submenu with the picker at its end.
- The connect screen's row is a bottom-docked link with a `Dismiss` button, gone after any
  layout is applied.
