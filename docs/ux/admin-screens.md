# Admin screens: users and permissions, and monitoring

Designed by Claude Fable (D15) for roadmap M6e: the users and permissions screens (roles,
grants, row and column security; D22) and the monitoring screens (server status, sessions,
locks and slow queries; D25, D237). Both are **page tabs** (section 1), the shape the data
transfer wizards, the ER view, notebooks, the builder and the console screens share. Every
token, rect and role not restated is [gui.md](gui.md)'s and [workspace.md](workspace.md)'s.
Rects are at the 1280 × 800 golden frame, unchanged by this. Every change a screen makes is
shown as the statement it runs before it runs (client-brief.md principle 2).

## 1. Page tabs
A page tab is a tab in the `tabs` strip whose content is a **page** filling the editor and
results regions together: `(260, 80, 1020, 696)` at the golden frame (`(36, …)` with the rail;
narrower with the inspector docked), `bg0`, with no editor/results splitter. The toolbar's
`Run`, `Run all`, `Explain` and `Stop` are disabled, the dialect pill disabled and reading the
page's word (`Users`, `Monitor`, `Import`, `Diagram`, `Notebook`, `Builder`, `Catalog`), `Ln, Col`
absent, the status bar's tag the page's last action (`GRANT · 1 ms`). A page has a 44 px
**page header** `(260, 80, 1020, 44)`, `bg1`, 1 px `border` below: the title in `title` at
(284, 92), optional segments after it (as the results header's, 1.4's segmented style, stripe
under the active one), and the page's buttons right-aligned 12 px from the right edge, 8 px
apart. The body is below. One page tab of each kind per connection: opening it again activates
it. A page tab holds no session and closes without asking unless it has pending statements
(section 2.5) or a job in flight. Keys: `Tab` order is the header's controls left to right, then
the body's regions top to bottom; `F6` cycles the body's regions; `Esc` in the body returns to
the header's first control. Role: the page a `region` named by the title; its header a
`toolbar`; segments a `tablist`.

## 2. Users and permissions
Page `Users` (title `Users and roles`, `Ctrl/Cmd+Shift+U`, `File > Users and roles…`, the tree's
connection row menu `Users and roles…`). Needs the `CREATEROLE` right or ownership of the
objects; without it the page opens read-only with the banner `Read-only: ada cannot manage
roles.` (3.7's place, info icon). On a file (embedded) the same page manages the file's roles
(D22 ships them in the engine; a file without roles shows `This file has no roles yet · Add
role creates the first, and opens it with every right until another role has them`).

### 2.1 Layout
| Region | Rect | Contents |
|--------|------|----------|
| header | (260, 80, 1020, 44) | title; right: `Add role` (primary), `Refresh` (ghost) |
| role list | (260, 124, 300, 608), `bg1`, 1 px `border` at x 559 | a filter field `(268, 132, 284, 28)` `Filter roles`; a tree from y 168: rows 24, level 0 `Login roles` and `Group roles` (folders), level 1 each role with the person icon (circle d 6 at (6, 3) over an arc (1, 12)–(11, 12) of radius 5) or the group icon (two overlapping persons), right text `superuser`, `no login` or the member count (`3 members`) |
| details | (560, 124, 720, 608) | blocks as the inspector's (4.3), scrolling as one, for the selected role: `Role`, `Member of`, `Grants`, `Column grants`, `Row policies`, `Masks` |
| pending bar | (260, 748, 1020, 28) | grid-editing.md 4.2's bar: `2 pending statements` · `Show statements` · `Discard` · `Apply` (`Ctrl/Cmd+S`) |

### 2.2 The `Role` block
A two-column form (labels at x 584, controls at x 744, 320 wide): `Name` (input; renaming
makes `ALTER ROLE a RENAME TO b`), `Can log in` (checkbox), `Password` (`Set password…` ghost
button opening 9.1's dialog 420 × 192 with `New password` and `Repeat` inputs, each with the
`Show` button of connections.md 3, `Set` default; the statement is shown as `ALTER ROLE ada
PASSWORD '••••••••'` and the value never appears in `Messages`, the status bar or the audit
text), `Valid until` (input, timestamp), `Connection limit` (number, `−1` = none), `Superuser`,
`Create roles`, `Create databases`, `Bypass row security` (checkboxes; each change the matching
`ALTER ROLE … WITH …`). `Drop role…` (secondary in `error` text) at the block's end, 10.2's dialog
with `DROP ROLE ada;` and, when the role owns objects, the body `ada owns 3 tables; reassign them
first.` with `Drop` disabled. `Add role` adds a row `new_role` in edit (the list row's label is
an input until `Enter`) and `CREATE ROLE new_role NOLOGIN;` pending.

### 2.3 The `Member of` and `Grants` blocks
`Member of (2)`: the groups as `bg2` pills with an `×` each (`REVOKE analysts FROM ada`), and an
`Add…` ghost opening a combobox of group roles (`GRANT analysts TO ada`). `Grants (12)`: a grid
`(584, y, 672, …)` with rows 24: columns `Object` (schema-qualified, the model icon), `Type`
(`table`, `view`, `sequence`, `function`, `schema`, `database`, `collection`, `key space`), then
one checkbox column per privilege the type has (`SELECT`, `INSERT`, `UPDATE`, `DELETE`,
`TRUNCATE`, `REFERENCES`, `TRIGGER` for a table; `USAGE`, `CREATE` for a schema; `EXECUTE` for
a function; `READ`, `WRITE` for a key space or collection, D22's model), and `Grant option`.
Checkboxes are 16 × 16 centred in 28-wide columns; a checked box inherited from a group draws
the check in `textSecondary` with the tooltip `from analysts` and is read-only; a direct grant
`textPrimary`. Toggling makes `GRANT SELECT ON orders TO ada;` / `REVOKE …` pending (several
privileges of one row in one statement). `Add grant…` (ghost under the grid) opens a combobox of
every object (type-ahead) and adds a row with no boxes checked. The header's row of checkboxes
toggles a privilege for every listed object (`GRANT SELECT ON ALL TABLES IN SCHEMA public TO
ada;` when every table is listed). Filter field above the grid: `Filter objects`.

### 2.4 Column grants, row policies and masks
- **`Column grants`**: for the table selected in a dropdown at the block's top (`orders ▾`): a
  grid of the table's columns (`Column`, `Type`) with checkbox columns `SELECT`, `INSERT`,
  `UPDATE`, `REFERENCES`; a column box checked while the table-level privilege is also granted
  draws in `textSecondary` with `table-level grant`; toggling makes `GRANT SELECT (name, email)
  ON customers TO ada;` (the columns of one statement joined).
- **`Row policies`**: one card per policy (`bg0`, 1 px `border`, `radiusCard`, 8 px apart):
  header `orders_own_rows` `bodyBold`, `on orders · for SELECT, UPDATE · permissive` `small`
  `textSecondary`, `Enabled` toggle; body two code boxes (the DDL box's style, editable) labelled
  `USING` and `WITH CHECK`, each one to four lines; a footer `Delete…`. `Add policy…` opens a
  form card with `Name`, `Table` (combobox), `Commands` (checkboxes `ALL`, `SELECT`, `INSERT`,
  `UPDATE`, `DELETE`), `Kind` (`permissive` / `restrictive`), the two boxes, and `Add`, making
  `CREATE POLICY orders_own_rows ON orders FOR SELECT TO ada USING (customer_id =
  current_setting('app.customer')::int);` pending. Above the cards one line per table the role
  can reach whose row security is off, with a toggle: `Row security on orders: off` → `ALTER
  TABLE orders ENABLE ROW LEVEL SECURITY;` and the `warning` line `Until a policy exists,
  enabling row security hides every row from ada.`
- **`Masks`**: a grid of masked columns (`Table`, `Column`, `Mask` as a dropdown: `default`
  (`xxxx`), `partial (first 2, last 2)`, `email`, `random`, `custom…` with an expression box) and
  `Unmasked for this role` checkbox; statements `ALTER TABLE customers ALTER COLUMN email ADD
  MASKED WITH (FUNCTION = 'email()');` and `GRANT UNMASK ON customers TO ada;` in the dialect's
  spelling (SQL Server's words, which the engine takes in every dialect; D22).

A preview: `Test as ada` (ghost, in the header) opens a query tab `Query n` whose session runs
`SET ROLE ada` first (the tab's title adds `as ada` and its status item reads `Role: ada`), so
the effect of grants, policies and masks can be seen on real rows.

### 2.5 Pending statements and Apply
Every change is a statement in the pending bar's list, shown in `Messages` by `Show
statements` (`Alt+S`) in the order made, each with the role it concerns; `Apply` (`Ctrl/Cmd+S`)
runs them in one transaction in the page's own session (opened for the apply, closed after;
never a tab's), stopping at the first error, which rolls everything back and shows
grid-editing.md 6's strip above the bar with the failing statement highlighted in `Messages`
(`Error 42501: permission denied to grant on orders · statement 3`, `Show`); on success the
toast `Committed · 4 statements`, the lists re-read. A statement that removes the current
user's own rights (`REVOKE … FROM ada` when `ada` is the connection's user, dropping one's own
superuser) asks 9.1's `Remove your own rights?` (`Apply anyway` secondary in `error` text,
`Cancel` default). `Drop role` asks as 10.2. `Discard` as grid-editing.md. The bar's text reads
`2 pending statements`.

### 2.6 Roles (accessibility)
The role list a `tree` named `Roles`; the detail blocks disclosure buttons as 4.3; the form's
controls by their labels; the grants grid a `grid` named `Grants` whose checkbox cells are
`checkbox`es named `SELECT on orders` with `checked`, `readonly` (inherited) and the source in
the description; policy cards `group`s named by the policy with the two `textbox`es `USING` and
`WITH CHECK`; the pending bar as grid-editing.md 10; the password dialog modal with the fields
as password fields. Announcements: the bar's count after each change; `Committed, 4
statements`; the strip as an alert.

## 3. Monitoring
Page `Monitor` (`Ctrl/Cmd+Shift+M`, `File > Monitor…`, the connection row menu `Monitor…`), with
four segments in the header: `Status`, `Sessions`, `Locks`, `Slow queries`; right: `Refresh
every 2 s ▾` (`Off`, `1 s`, `2 s`, `5 s`, `30 s`; refreshing pauses while the tab is not active
or the window is hidden, and resumes with one refresh on return), `Refresh now` (ghost, `F5`).
Data comes from the engine's views in the tab's dialect spelling (`pg_stat_activity`, `pg_locks`,
`pg_stat_statements`, `pg_stat_database`, and the file's `eggdb stats` for an embedded file,
D25); a server that refuses them (no `pg_stat_statements` extension, no rights) shows the
segment's body as the banner `Slow queries need the pg_stat_statements extension · Enable`
(the button runs `CREATE EXTENSION pg_stat_statements;` after 10.2's dialog) or `Read-only: ada
cannot read pg_stat_activity`.

### 3.1 Status
Body `(260, 124, 1020, 652)`, 24 px padding. A row of **stat tiles** at y 148, each `(x, 148,
232, 84)`, `bg1`, `radiusCard`, 16 px apart from x 284, four across: title `small` `textSecondary`
at (12, 8), value `headline` `textPrimary` at (12, 26), footnote `small` `textSecondary` at
(12, 58):

| Tile | Value | Footnote |
|------|-------|----------|
| `Connections` | `12` | `of 100 · 3 active` |
| `Transactions` | `1,204 /s` | `committed 99.8 %` (`rolled back 3 /s` in `warning` when over 1 %) |
| `Cache hits` | `99.1 %` | `184 MB cached` |
| `Storage` | `1.2 GB` | `shop.eggdb · 42 MB free on disk` (`warning` under 5 %) |

A second row at y 248: `Uptime` (`3 d 4 h`), `Version` (`0.41.0`), `WAL` (`12 MB · last
checkpoint 40 s ago`), `Replication` (`2 standbys · lag 120 ms`, or `none`). Below, two charts
side by side at y 356, each `(x, 356, 486, 200)`: `Queries per second` (one line, `chart1`) and
`Connections` (two lines, `active` `chart1`, `idle` `chart2`, legend above), both over the last
10 minutes, drawn as data-models.md 4's line chart with a `Grid` toggle (ghost, each chart's
top-right) that swaps the chart for its table; values sampled at each refresh and kept for the
tab's life (at most 10 minutes); the newest sample at the right. A tile is a `group` named by
its title with the value and footnote as text; a value over a limit adds the warning icon
before it and the word in the footnote, never colour alone.

### 3.2 Sessions
3's grid in the body (header at y 124, rows from 152): columns `PID` (right), `User`, `Database`,
`Application`, `Client` (`10.0.0.5:51234`, or `local`), `State` (`active`, `idle`, `idle in
transaction` with the warning icon when over 60 s, `waiting`), `Wait` (`Lock: relation orders`,
`IO: DataFileRead`, or nothing), `Started` (`12 s ago`, `3 m 20 s ago`), `Transaction` (`open 45
s` or nothing), `Query` (the text's first line, cut; the value view shows it whole). The
client's own sessions (each tab's, D310) draw their `Application` as `EggDB GUI · Query 2` and
the row's gutter a 2 px `accent` bar so one never kills one's own tab by mistake without seeing
it. Row actions, in the header's right and the row menu: `Explain` (opens the query in a new
tab and explains it, 5.1, without running), `Cancel query` (`pg_cancel_backend(pid)`, 9.1's
dialog `Cancel the query of PID 4123?`, `Cancel query` secondary / `Keep` default) and
`Terminate` (`pg_terminate_backend(pid)`, body `The session is disconnected and its transaction
rolled back.`, `Terminate` secondary in `error` text); on MySQL the statements are `KILL QUERY
4123` / `KILL 4123`, on SQL Server `KILL 52`, on a file the client's own tabs can only be
stopped (`Stop Query 2`, 2.3) and other processes' sessions are not listed (a file has one
writer: the tile says `1 process`). A filter field in the header (`Filter sessions`) and a
`Hide idle` checkbox. The summary line under the header reads `12 sessions · 3 active · 1
waiting`. Sorting by a header click (`aria-sort`) is kept across refreshes, as is the current
row (by PID).

### 3.3 Locks
A grid: `PID`, `Lock` (`relation orders`, `transaction 4812`, `tuple orders (0,3)`, `advisory
42`), `Mode` (`RowExclusiveLock`, …), `Granted` (`yes` / `waiting 12 s` with the warning icon),
`Blocked by` (the PID holding it, a link that selects that row), `Query`. Above the grid a
summary `2 waiting · longest 12 s` and a **wait chain** strip when any waits: `(284, 156, 972,
28)` `bg1`: `4123 waits for 4100 waits for 4098` in `code`, each PID a link; `Terminate 4098`
ghost after it (the dialog of 3.2). The grid groups rows by PID (a 24 px group row in `bg1`
with the PID and its query, collapsible). Role: the chain a `status` (polite, read when it
changes); the links `link`s.

### 3.4 Slow queries
A grid from `pg_stat_statements` (`performance_schema.events_statements_summary_by_digest` on
MySQL, `sys.dm_exec_query_stats` on SQL Server, the file's statement statistics, D25): `Query`
(normalised text, cut), `Calls` (right), `Mean` (`ms`, right), `Total` (right), `Max`, `Rows`
(mean rows), `Hit %` (buffer hit ratio), `Last run`; sorted by `Total` descending at first;
each numeric cell carries a 4 px `bg2` / `accent` share bar of the column's maximum (the plan
view's bar), the number always printed. Header controls: `Since ▾` (`Server start`, `Last
hour`, `Last 24 h`, computed by keeping samples per refresh), `Min calls` number input, a filter
field, `Reset statistics…` (ghost; 10.2's dialog with `SELECT pg_stat_statements_reset();`).
Row actions: `Open in tab` (the text in a new query tab, not run), `Explain` (as 3.2), `Copy`.
The row's menu adds `Show plan from the last run` when the engine kept it. `Enter` on a row
opens the full query in the value view.

### 3.5 Keys and roles
Segments by `Ctrl/Cmd+1` … `4` while the page is active (overriding the region keys, which a
page has no editor for); `F5` refreshes; each grid is 3's for keys and copy; `Alt+R` focuses
the refresh dropdown. Roles: the segments a `tablist`; tiles `group`s with their title, value
and footnote; charts `img`s with the `Grid` toggle as a `button` `pressed`; each grid a `grid`
named by the segment (`Sessions`), its summary line a polite `status` read only on a user action
(never on a timer refresh, so the reader is not interrupted every 2 s: a refresh raises the
grid's rows-changed event quietly); the dialogs modal with `Keep` default.

## 4. Acceptance checklist
- [ ] png: `Users` opens with the role tree (`Login roles` › `ada`, `bob`; `Group roles` ›
      `analysts (2 members)`), `ada` selected, the `Role` form, `Member of` pills, the `Grants`
      grid with inherited checks in `textSecondary`; toggling `SELECT` on `orders` shows the bar
      `1 pending statement` and `Messages` reading `GRANT SELECT ON orders TO ada;`.
- [ ] png: `Add policy…` filled and added shows the card and the `CREATE POLICY` statement; the
      `Row security on orders: off` line with its toggle and warning; `Set password…` shows the
      dialog and the pending list shows `ALTER ROLE ada PASSWORD '••••••••'`.
- [ ] png: `Apply` with a stub `42501` on statement 3 keeps every statement pending, shows the
      strip with `statement 3`, and `Messages` highlights it; a `REVOKE` on the connection's own
      user shows `Remove your own rights?` with `Cancel` default.
- [ ] png: `Monitor › Status` draws eight tiles with the given texts and two charts with a
      legend over a 10-minute stub; `Storage` under 5 % free draws the warning icon and word.
- [ ] png: `Sessions` lists 12 stub rows, the client's own tab rows with the `accent` gutter bar,
      an `idle in transaction` row over 60 s with the icon; `Terminate` shows the dialog with the
      `error`-text button; after a refresh the current row and sort are kept.
- [ ] png: `Locks` with a stub chain draws the wait-chain strip `4123 waits for 4100` with the
      `Terminate 4100` button and the grouped rows; `Slow queries` draws the share bars and sorts
      by `Total`; a server without the extension shows the `Enable` banner.
- [ ] tree: the page is a `region` `Monitor` with a `tablist`; tiles are `group`s; a timer
      refresh raises no live announcement; `Ctrl+3` selects `Locks`.

## Decided by this design
- Page tabs are the shape of every non-query screen: one region, a 44 px header with segments
  and buttons, no session, one per kind per connection.
- Users and permissions are edited as pending statements shown in `Messages` and applied in one
  transaction in the page's own session; inherited grants are shown read-only with their
  source; passwords never appear in any text.
- `Test as ada` opens a query tab under `SET ROLE` so rights are checked on real rows.
- Monitoring refreshes on a timer that pauses when hidden and never speaks; the client's own
  sessions are marked; every kill is a confirmed statement in the dialect's spelling; slow
  queries carry share bars and open in tabs or the plan view.
