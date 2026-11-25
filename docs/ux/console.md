# The admin web console, and the catalog, lineage and master data screens

Designed by Claude Fable (D15) for roadmap M6e: an admin web console served by the server (D42),
which screens it has and how it relates to the desktop client; and the console screens for the
catalog, lineage and master data (D42, D244, D51). Page tabs ([admin-screens.md](admin-screens.md)
section 1). Every token, rect and role not restated is [gui.md](gui.md)'s and
[workspace.md](workspace.md)'s. Rects are at 1280 × 800.

## 1. The admin web console

### 1.1 One client, served by the server
The console is the browser build of `eggdb-gui` (D256: the same frames drawn on a canvas),
served by `eggdb serve` at `/console/` on its HTTP listener (`--http-listen`, D34's HTTP
server; the WebAssembly binary, one HTML page and the minimal loader script D16 allows). It is
the same product as the desktop client with three differences: it is already connected to the
serving server (no connect card: the first screen is the sign-in, 1.2), its sidebar has a
`Console` group (1.3), and it has no file dialogs or OS credential store (the browser's rules
of workspace.md apply). Everything else is identical: the query workspace, the grid, the
object browser, the model tabs, the page tabs of this and the other design files, the
layouts, themes, languages and keys. A desktop client connected to the same server shows the
same `Console` group in its tree and opens the same page tabs, so an administrator uses either.
The console is off by default (`--console` turns it on, or `console = true` in the server's
config) and needs TLS on the listener unless bound to localhost (D22's defaults).

### 1.2 Sign-in
The connect card's shape (first-screen.md), `(cx, cy, 560, 300)` centred in the viewport: title
`Sign in to shop` (`headline`), under it `db.example.com:5432 · EggDB 0.41.0` in `small`
`textSecondary`; `User` and `Password` inputs (connections.md 3's `Show` button), `Remember me
on this browser` checkbox (a session cookie of 30 days, `HttpOnly`, `Secure`, `SameSite=Strict`,
issued by the server's auth, D22; unchecked: the session ends with the tab), `Sign in` primary
(`Enter`). Authentication is the server's own (SCRAM over the HTTP session, or the identity
provider when the server is configured for OpenID Connect: then one button `Sign in with
<provider>` redirects and returns). Failure shows 1.6's lines (`Error 28P01: password
authentication failed for user "ada"`). After sign-in the main window opens in the user's
stored layout (the settings live in the server's per-user console settings, `eggdb.console_settings`,
so the same person sees the same layout from any browser). `Sign out` is in the `more` menu and
on the status bar's connection item.

### 1.3 The `Console` group
In the tree, after the model groups, for a connection whose user has the admin rights: a group
`Console` (icon: a 12 × 12 gauge, an arc of 1.5 px from (1, 9) over the top to (11, 9) with a
needle (6, 9)–(9, 4)) whose rows open page tabs:

| Row | Opens | Right text |
|-----|-------|-----------|
| `Status` | Monitor › Status (admin-screens.md 3.1) | `12 connections` |
| `Sessions`, `Locks`, `Slow queries` | the Monitor page on that segment | `3 active`, `2 waiting`, nothing |
| `Users and roles` | admin-screens.md 2 | the role count |
| `Backups` | section 1.4 | `last 2 h ago` (`warning` with the icon when over the schedule's interval) |
| `Import and export` | the Import wizard; its row menu has `Export…`, `Restore…` | |
| `Catalog` | section 2 | the term count |
| `Lineage` | section 3 | |
| `Master data` | section 4 | the entity count |
| `Settings` | section 1.5 | |
| `Logs` | section 1.6 | `3 errors today` |

The rail (10.6) gains a `Console` item with the gauge icon. Without the rights the group is
absent; a page opened by URL (`/console/#/users`) without them shows the page's read-only banner.
Browser URLs: the console keeps the active page in the fragment (`#/status`, `#/users/ada`,
`#/catalog/terms/revenue`), so links can be shared and the back button returns to the previous
page tab.

### 1.4 Backups page
Page `Backups`: header with `Back up now` (primary: the Backup dialog, data-transfer.md 4, the
destination on the server's backup location), `Schedule…` (secondary), `Settings` (ghost). Body:
a `Schedule` block (`Every day at 02:00 · keep 14 · to /var/backups/eggdb` with `Edit`), a
`Point in time` block when continuous archiving is on (`Can restore to any moment since
2026-11-11 02:00 · 1.2 GB of WAL`; `Restore to…` opens the Restore wizard's `Into` step with a
`Timestamp` input), and a grid of backups: `Taken`, `Kind` (`full`, `incremental`), `Size`,
`Duration`, `Verified` (`yes` / `no` with the icon), `Location`, with row actions `Restore…`,
`Verify`, `Download` (the browser downloads it), `Delete…` (confirm). `Schedule…` is 9.1's dialog
with `Every ▾` (`hour`, `day`, `week`), a time input, `Keep` number, `Verify after writing`
checkbox and the location, writing the server's backup job (`eggdb_console.backup_schedule`).

### 1.5 Settings page
Page `Settings` with segments `Server`, `Listeners`, `Security`, `Console`: two-column forms
(labels at x 284, controls at x 544, 400 wide) of the server's configuration as the engine
exposes it (`SHOW ALL` and the config file's keys), each row with its current value, a
`default` badge or `changed` badge, and the source (`config file`, `command line`); editable
values that take effect live are inputs, those needing a restart show the restart icon and the
page's bottom bar `Restart to apply 2 changes · Restart…` (9.1's dialog naming the connected
sessions). Every change is a pending statement (`ALTER SYSTEM SET work_mem = '64MB';`) in
admin-screens.md 2.5's bar. `Listeners` lists each protocol listener (`PostgreSQL 5432 · TLS`,
`RESP 6379 · TLS`, `HTTP 8080`) with `on` toggles and TLS certificate rows (expiry with the
warning icon under 14 days, `Replace…` upload); `Security` holds the authentication methods,
the password policy and the audit log's settings; `Console` the console's own (`Session length`,
`Allowed origins`, `Sign-in page message`).

### 1.6 Logs page
Page `Logs`: a header with `Level ▾` (`error`, `warning`, `info`, `all`), `Since ▾`, a filter
field, `Follow` (toggle, data-models.md 7's behaviour), `Download`; a grid `Time`, `Level` (the
word with the error or warning icon), `Session`, `User`, `Message` (the value view shows the
whole line and its structured fields as JSON); the audit log as a second segment `Audit` (the
rows of `eggdb audit` with `Verify chain`, D121); `Slow query log` as a third. A row's menu:
`Open session` (the Sessions page on that PID), `Copy`.

### 1.7 Relationship to the desktop client, in one table
| | Desktop client | Web console |
|---|---|---|
| binary | `eggdb-gui` | the same crate as WebAssembly, served by `eggdb serve --console` |
| first screen | the connect card | sign-in to this server |
| connections | files and any server | this server only |
| settings | `gui-settings` on the computer | `eggdb.console_settings` on the server, per user |
| file dialogs, drag-out, credential store | the OS's | the browser's download, upload and drop |
| `Console` group | shown for a server connection with the rights | always, for the signed-in user's rights |
| everything else | identical | identical |

## 2. The catalog page
Page `Catalog` (the `eggdb_catalog` extension, D244; a server without it shows `Catalog needs
the eggdb_catalog extension · Enable` as admin-screens.md 3's banner). Segments `Assets`,
`Glossary`, `Tags`, `Quality`.

- **Assets**: a two-pane body: left `(260, 124, 360, 652)` a tree of every object (the object
  browser's rows, every schema and model) with a filter and `Tags ▾` filter; right the selected
  asset's **catalog card**: the name in `title`, under it the model and size; `Description`
  (an editable text block, Markdown rendered when not focused, `code` while editing, saved as
  `COMMENT ON` or `eggdb_catalog.describe`), `Owner` and `Steward` (comboboxes of roles),
  `Tags` (pills with `×` and `Add…`), `Terms` (glossary terms linked, pills), `Columns` (a grid
  `Column`, `Type`, `Description` (editable cell), `Tags`, `Classification` (dropdown: `public`,
  `internal`, `confidential`, `personal data`, with the warning icon on `personal data`),
  `Quality` (the last check's result word with its icon)), `Lineage` (a short list of upstream
  and downstream assets with `Open in lineage` → section 3), `Profile` (from
  `eggdb_catalog.profile`: per column `null %`, `distinct`, `min`, `max`, `top values` as a
  small grid with share bars; `Profile now` runs it as a job). Every edit is a pending statement
  (`UPDATE eggdb_catalog.assets SET …`, `INSERT INTO eggdb_catalog.tags …`) in
  admin-screens.md 2.5's bar.
- **Glossary**: a grid of `eggdb_catalog.terms` (`Term`, `Definition` (cut), `Owner`, `Status`
  (`draft`, `approved`, `deprecated`), `Linked assets` (count)) with `Add term`; a row opens the
  term's card (the fields editable, `Linked assets` as a list with `Link…`); terms link to
  columns from either side.
- **Tags**: a grid of tags with counts, colours never (tags are text pills), `Rename`, `Merge
  into…`, `Delete…`.
- **Quality**: a grid of `quality_rules` (`Rule`, `Table`, `Condition` (SQL, cut), `Severity`,
  `Last run`, `Result` (`pass` with the check icon, `12 failing rows` with the warning icon in
  `warning`, `error: …`), `Trend` (a 80 × 16 sparkline of the last 20 results as a `chart1` line,
  with the values in its tooltip)) with `Add rule…` (a card: `Name`, `Table`, `Condition` as a
  code box with completion, `Severity`, `Schedule` (`manual`, `hourly`, `daily` through `pg_cron`,
  D238), `Add`), `Run now` (per row or all: `eggdb_catalog.check_quality`, a job), and a row's
  `Show failing rows` (a query tab with the rule's failing-row statement, run). Roles on
  conditions as D244 says: the `Add rule…` card shows `Conditions run with the caller's rights`
  in `small` `textSecondary`.

## 3. The lineage page
Page `Lineage` (`eggdb_catalog.lineage` and `column_lineage`, D244): a header with `Asset`
(a combobox with type-ahead over every asset name, including outside ones like `crm.accounts`
and `dashboard:revenue`), `Direction ▾` (`Both`, `Upstream`, `Downstream`), `Depth 3 ▾` (1–10;
the function's 100-hop cap applies), `Columns` (toggle: column-level edges), `Fit`, `Export ▾`
(`PNG`, `DOT`, `OpenLineage JSON` from `openlineage_event`). The body is a canvas like the ER
view's (er-view.md 2 and 6: zoom, pan, minimap, keys), laid out left to right with upstream
assets to the left: nodes are cards 180 wide, 44 tall (the model's icon, the name in `bodyBold`,
the kind in `small` `textSecondary`: `table`, `view`, `job`, `file`, `dashboard`, `external`), the
chosen asset with a 2 px `accent` border; edges 1 px `borderStrong` orthogonal lines with a 6 px
arrowhead pointing downstream, the edge's `job` or `statement` name at its midpoint in `small`
`textSecondary` (the tooltip shows the statement whole); with `Columns` on, a card expands to
list its columns (20 px rows) and edges join column to column. A node's `Enter` selects it (the
inspector shows the asset's catalog card of section 2); `Ctrl/Cmd+Enter` re-centres the graph on
it; `Ctrl/Cmd` + arrows follow edges. `Add edge…` (ghost in the header) opens a card `From`,
`To`, `Via` (a statement, a job name or free text), `Add` → `INSERT INTO eggdb_catalog.lineage`;
an edge's menu has `Delete…`. Over 500 nodes the page asks for a smaller depth. Roles as the ER
view's with `Lineage of orders` as the canvas's name and `references` / `feeds` in the edge
names (`orders feeds revenue_daily via job nightly_rollup`).

## 4. The master data page
Page `Master data` (D51's master data management, the `eggdb_mdm` part of `eggdb_catalog`:
entity sets, match rules, golden records): segments `Entities`, `Match rules`, `Review`,
`Golden records`.

- **Entities**: a grid of entity sets (`Name` (`customer`), `Sources` (`crm.accounts,
  shop.customers`), `Records`, `Golden`, `Pending review` (with the warning icon when over 0),
  `Last match`) with `Add entity…` (a card: `Name`, `Sources` (a checklist of tables with a
  `Key column` dropdown each), `Add`) and a row's `Run matching` (a job, the sweeping bar in the
  header with `Matching 12,400 records…`).
- **Match rules** for the selected entity: cards, one per rule: `Name`, `Fields` (rows of
  `Field` dropdown, `Compare ▾` (`exact`, `case-insensitive`, `normalized` (trimmed, accents
  folded), `edit distance ≤ n`, `phonetic`, `numeric within n %`), `Weight` number), `Threshold`
  (`auto merge at 0.95 · review at 0.80`) as two number inputs, `Survivorship ▾` per field
  (`most recent`, `most complete`, `source priority` with an ordered list of the sources,
  `longest`), `Test…` (runs the rule on a 1,000-row sample and shows the pair counts), `Save`.
  Each save is a pending statement (`INSERT INTO eggdb_catalog.match_rules …`).
- **Review**: the pairs scored between the review and merge thresholds: left `(260, 124, 300,
  652)` a list (rows 44: the two records' names and `score 0.87`), right a **compare card**: the
  two records side by side as a grid `Field`, `A (crm.accounts #412)`, `B (shop.customers
  #1042)`, `Golden` (the survivorship pick, editable per field with a dropdown of `A`, `B` or a
  typed value), equal fields in `textSecondary`, differing fields `textPrimary` with the mono bold
  face; buttons `Merge` (primary, `Ctrl/Cmd+Enter`: the golden record is written and both records
  linked), `Keep separate` (secondary, `Ctrl/Cmd+Backspace`: the pair is marked distinct and never
  offered again), `Skip` (ghost, `Ctrl/Cmd+Right`). Keys: `Up` / `Down` move the list, `Tab` moves
  into the card. Every decision is written at once (the review is a queue, not a batch) and
  announced politely (`Merged; 41 pairs left`).
- **Golden records**: a grid of the entity's golden records (`Golden id`, the entity's fields,
  `Sources` (count), `Updated`) with a filter; a row opens its card: the fields, the source
  records as a list with `Unlink` each, `History` (one row per merge or edit with the user and
  time), `Split…` (confirm: undoes a merge).

## 5. Accessibility summary
| Element | Role | Name |
|---------|------|------|
| sign-in card | `form` | `Sign in to shop` |
| `Console` group and rows | `treeitem`s | the labels with the right text as the description |
| catalog card blocks | disclosure buttons and `textbox`es (`Description` editable) | the block names |
| classification dropdown | `combobox` | `Classification of email` |
| quality trend sparkline | `img` | `Trend: 18 of 20 passed` (values in the description) |
| lineage canvas | `group` of `button`s | `Lineage of orders` |
| review compare card | `grid` | `Compare records` with `Merge`, `Keep separate`, `Skip` `button`s |
| settings rows | `textbox`, `combobox`, `switch` (`on` toggles) | the key with `default` / `changed` and `needs restart` in the description |

Announcements: a job's end (`Profiled customers`, `Matching done: 41 pairs to review`) politely;
a merge decision politely; a settings change that needs a restart through the bottom bar's
`status`.

## 6. Acceptance checklist
- [ ] browser: `GET /console/` on a server with `--console` serves the page; without it 404; the
      sign-in card at the viewport's centre with the server line; a wrong password shows `Error
      28P01` and focuses Password; after sign-in the main window has the `Console` group with ten
      rows and the rail's `Console` item; `#/users/ada` opens the Users page on `ada`.
- [ ] png (desktop, a server with the rights): the tree's `Console` group after the model groups
      with the gauge icon and right texts; `Backups` opens the page with the schedule block, the
      point-in-time block and the grid; `Settings › Listeners` shows three listeners with toggles
      and a certificate row with the warning icon under 14 days.
- [ ] png: `Catalog › Assets` with `customers` selected: the card's `Description` editable, two
      tag pills, the `Columns` grid with `email` classified `personal data` and the icon, the
      `Profile` block's share bars; `Quality` with a failing rule's warning icon and a sparkline;
      the pending bar after a description edit reading `1 pending statement`.
- [ ] png: `Lineage` for `revenue_daily` at depth 3: upstream `orders` and `customers` to the
      left, the job name on the edges, `dashboard:revenue` downstream with the external kind;
      `Columns` on expands `orders` and joins `total` to `revenue`.
- [ ] png: `Master data › Review` with a pair selected: the compare card with differing fields in
      the bold face and the `Golden` column's dropdowns; `Ctrl+Enter` merges and the list's next
      pair is selected with the announcement.
- [ ] tree: the lineage canvas's edges are `button`s named `orders feeds revenue_daily via job
      nightly_rollup`; the settings rows carry `needs restart` in their descriptions; the
      sign-in card is a `form`.

## Decided by this design
- The web console is the client's browser build served at `/console/` by the server, signed in
  to that server, with a `Console` group of page tabs that the desktop client also shows; one
  codebase, one set of screens, settings kept per user on the server.
- The console is off by default and needs TLS off localhost; sessions are the server's own
  authentication with a `Remember me` cookie.
- Backups, settings and logs are console pages built on the engine's own functions and views;
  every change is a pending statement, and restart-needing settings say so in a bottom bar.
- The catalog, lineage and master data pages are views over `eggdb_catalog`'s tables and
  functions: assets with editable descriptions, stewards, tags, classifications and profiles;
  quality rules with trends and failing-row tabs; lineage as a left-to-right canvas with
  column-level edges and OpenLineage export; master data as entities, weighted match rules, a
  review queue with a side-by-side compare card, and golden records with history and split.
