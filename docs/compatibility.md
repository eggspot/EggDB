# Compatibility list

The tools, drivers, ORMs and migration tools EggDB is checked with, and how each is checked.
Release notes quote this list. A tool counts as **supported** when a CI job runs it, or replays
what it sends, on every pull request. It counts as **expected to work** when EggDB answers every
catalog query and protocol message the tool is known to send, but no CI job runs the tool itself
(D163).

| Tool | Version | Protocol | Status | How it is checked |
|------|---------|----------|--------|-------------------|
| psql | 16, 17 | PostgreSQL | supported | `pg-catalog`, `pg-functions`, `pg-restore`, `tls-clients` and `pg-follow` CI jobs: `\d` family, functions, restores, TLS with `verify-full` and direct TLS |
| pgjdbc | 42.7 | PostgreSQL | supported | `pg-apps` job: CRUD, transactions, prepared statements, `DatabaseMetaData` (D131) |
| psycopg 3, SQLAlchemy Core | 3.2, 2.0 | PostgreSQL | supported | `pg-apps` job, compared byte for byte with PostgreSQL 17 (D131) |
| SQLAlchemy ORM, Flyway | 2.0, 10 | PostgreSQL | supported | `pg-apps` job (D138) |
| SQLAlchemy dialect compliance suite | 2.0.36 | PostgreSQL | supported, by pass count | `sqlalchemy-suite` job: at least the count in `scripts/sqlalchemy-suite/minimum-passes.txt` must pass, and it may only rise; its setup creates the `test_schema` and `test_schema_2` schemas SQLAlchemy's own test instructions ask for, and the `citext` and `hstore` extensions, which EggDB has built in; the whole suite runs, past SQLAlchemy's own stop after 250 failures (D163, D259, D308) |
| Npgsql | 8.0 | PostgreSQL | supported | `pg-apps` job: type loading, batches, cancel (D135) |
| pg8000 | 1.31 | PostgreSQL | supported | smoke test (D104) |
| pgAdmin 4 | 8 | PostgreSQL | expected to work | its dashboard and object-browser catalog reads answered: `pg_stat_activity`, `pg_stat_database`, `pg_locks`, `pg_extension`, the privilege functions, and the rest of `pg_catalog` (part 3.19, `pg_catalog/tests.rs`) |
| DBeaver | 24 | PostgreSQL, MySQL, TDS | expected to work | through pgjdbc, mssql-jdbc and the MySQL metadata answered (part 3.20) |
| mysql client, PyMySQL | 8.0, 1.1 | MySQL | supported | smoke test, `tls-clients` (D106) |
| go-sqlcmd | 1.10 | TDS | supported | smoke test, `tls-clients` with `-N true` and `-N strict` (D122) |
| Microsoft.Data.SqlClient | 5.2 | TDS | supported | `tds-apps` job: RPC, metadata, bulk copy, batch logic (D132, D133, D136, D137) |
| EF Core (SQL Server) | 8.0 | TDS | supported | `tds-apps` job (D138) |
| mssql-jdbc | 12 | TDS | supported | `tds-apps` job (D132, D133) |
| SSMS | 20 | TDS | expected to work | Object Explorer's metadata queries answered by `sys.*` and the catalog procedures (part 3.21) |
| pg_dump (plain format) | 16, 17 | restore | supported | `pg-restore` job (D118) |
| SSMS Generate Scripts, sqlpackage | 20, 162 | restore | supported | `restore_sqlserver` tests (D125) |
| Oracle HR sample, SQL Developer, SQLcl CSV | 23ai | restore | supported | `oracle-apps` job and `restore_oracle` tests (D127) |
| PostgreSQL logical replication | 10–17 | follow | supported | `pg-follow` job (D162) |
| SQL Server change tracking | 2016+ | follow | supported | `ss-follow` job against SQL Server 2022 (D162, D261) |
| SQL Server change data capture (CDC) | 2016+ | follow | supported | `ss-follow` job against SQL Server 2022 with SQL Server Agent: CDC and change tracking per table, cleanup detected (D261) |

## Familiar layouts in EggDB's desktop client

`eggdb-gui` can lay its main window out the way another client does, so its users keep their
habits (D15, D309). `Where are you coming from?` on the connect card, `Ctrl/Cmd+K L` in the main
window or the status bar's `Works like <product>` item opens the layout picker; the choice is
kept in `gui-settings` in EggDB's per-user state directory (`$XDG_STATE_HOME/eggdb`,
`~/.local/state/eggdb`, `%LOCALAPPDATA%\EggDB`; `EGGDB_GUI_SETTINGS=<path>` moves it) and applies
at the next start. `eggdb-gui --layout <name>` applies one for a single run without keeping it.

| Layout | `--layout` | Keys it adds (Ctrl is Cmd on macOS, except pgAdmin's) |
|--------|-----------|---------------|
| EggDB | `eggdb` | EggDB's own |
| Works like SQL Server Management Studio | `ssms` | `F5`, `Ctrl+E`, `Alt+X` execute the selection or the tab; `Ctrl+L` explains |
| Works like pgAdmin 4 | `pgadmin` | `F5` executes |
| Works like DBeaver | `dbeaver` | `Ctrl+Enter` the statement, `Alt+X` the script, `Ctrl+Shift+E` explains |
| Works like DataGrip | `datagrip` | `Ctrl+Enter` the statement |
| Works like MySQL Workbench | `mysql-workbench` | `Ctrl+Enter` the statement, `Ctrl+Shift+Enter` all, `Ctrl+Alt+X` explains |
| Works like MongoDB Compass | `mongodb-compass` | `Enter` runs the filter bar |
| Works like Redis Insight | `redis-insight` | `Ctrl+Enter` runs the whole editor |

Each layout places the toolbar, sidebar, tabs, editor, results and status bar at its own rects
(`docs/ux/gui-layouts.md`) and uses its names for the run button, the result views and the
sidebar's tree. Product names are trademarks of their owners; EggDB is not affiliated with them,
and a layout copies an arrangement, shortcuts and workflows only, never their code, artwork,
icons, colours or text.

## Query tabs and their sessions in EggDB's desktop client

Each query tab in `eggdb-gui` is its own session on the database, kept from the tab's first run
until it closes, the way a connection in `psql` or a query window in another client is (D310,
`docs/ux/workspace.md` 9). A `BEGIN` in one run stays open for the next run in that tab, and the
other tabs do not see its work until it commits. EggDB runs one writer at a time, so a second
tab's write while another holds the write transaction gets the engine's error. A tab with an open
transaction shows a `warning` dot and the status bar's `Transaction open` (`Transaction failed:
ROLLBACK to continue` after an error in the block; `Transaction open in Query 2` from the other
tabs), and its dialect stays the one the transaction began in until `COMMIT` or `ROLLBACK`.

`Ctrl/Cmd+T` (or the strip's `+`) adds `Query n` after the active tab with its dialect;
`Ctrl/Cmd+W`, `Ctrl+F4`, a tab's close button or a middle-click closes one; `Ctrl+Tab` and
`Ctrl+Shift+Tab` move between them. Closing a tab that runs, holds a transaction (which rolls
back) or holds text asks first. Tabs narrow from 140 to 100 px as they are added, and from ten
tabs the strip scrolls with the wheel, always keeping the active tab in view. On a server, the
first tab that runs uses the connection the client opened, and each other tab opens its own.

## The result grid's value view, menu, column widths and scrollbars

In the grid (D310, `docs/ux/workspace.md` 9.3 to 9.6), `Enter`, `F2` or a double-click opens the
current cell's full value: text with its line breaks, JSON and arrays pretty-printed with a
2-space indent in stored order, a vector one value a line, a blob as `0x` and its full hex. The
body stops after 1 MB with `… and 2.1 MB more`, but `Copy` and `Ctrl/Cmd+C` copy the whole value;
`Esc`, `Enter` or `F2` close it. A right-click, `Shift+F10` or the menu key opens the grid's menu:
`Copy` (TSV), `Copy with header`, `Copy as INSERT` in the tab's dialect, `Copy as CSV` (RFC 4180:
a header row, `\r\n` between records, fields with `,`, `"` or line breaks quoted, NULL empty),
`Copy as JSON` (an array of objects keyed by column name, a repeated name as `name_2`; numbers
and booleans bare, NULL `null`, JSON embedded, the rest strings), `Open value` and the select
items. A right-click on the row numbers or a header selects that row or column first.

Drag a header's right edge to resize its column (48 to 960 px), double-click the edge to fit it to
its header and first 200 rows (up to 480 px), or press `Alt+Left` / `Alt+Right` (`Shift` for 32
px) and `Alt+Enter` on the current cell. A tab keeps its widths when the next run returns the same
column names in the same order. Scrollbars show only while the grid's rows or columns, or the
editor's lines, do not fit; a press on the track pages and the grid's thumb drags. The header
names the kind of value (`text`, `number`, `json`, `binary`) rather than the column's SQL type
for now.

## The object tree's row menu, splitters, the rail and motion in EggDB's desktop client

In the sidebar's tree of `eggdb-gui` (D311, `docs/ux/workspace.md` 10), a right-click, `Shift+F10`
or the menu key opens a row's menu: `Open` (`Expand` / `Collapse` on the connection and groups),
`Insert name`, `Copy name` (also `Ctrl/Cmd+C`; the connection copies its path or URL), `Show DDL`,
`Refresh` (`F5`) and `Drop…`, with the items a row cannot use shown disabled in place. `Drop…`
shows the `DROP` statement in the active tab's dialect (MySQL and SQL Server drop an index `ON` its
table, PostgreSQL a trigger) and runs it in a session of its own, never a tab's; it waits while a
tab runs or holds a transaction, and a read-only connection disables it. A table with triggers
shows a `Triggers` group (one row per trigger, its events as right text) and a `Triggers (n)`
block in its inspector; a trigger's inspector shows its action. `Enter` on a key space opens a
`Keys in <space>` tab listing its first 100 keys with their values and sizes in bytes, spelled as
`eggdb ui` spells them (a file only for now; on a server the tab is empty).

Drag or key the splitters beside the sidebar (180 to 480 px), between the editor and the results,
and beside a docked inspector (200 to 480 px): arrows move 8 px, `Shift` 32, `Home` / `End` to the
limits, `Enter` or a double-click resets. The sizes and whether the sidebar and the inspector are
shown are kept in `gui-settings` per layout (`eggdb.sidebar_width = 268`). `Ctrl/Cmd+B` collapses
the sidebar to a 36 px rail and back; the rail's items open the tree as a drawer. Windows go down
to 800 px wide; below 1440 a docked inspector closes and comes back when the window grows again,
and `Ctrl/Cmd+I` opens it as an overlay at any width. Splitters and the rail are in EggDB's own
layout only for now.

Hover and pressed fills on the toolbar's buttons, the inactive tabs and the connect screen's
buttons fade over 80 ms. They change at once, and the caret stops blinking, when the system asks to
reduce motion (macOS `Reduce motion`, Windows `Animation effects` off, X11 XSETTINGS
`Gtk/EnableAnimations` off; read at start and whenever the window gains the focus) or when
`eggdb-gui --reduced-motion` is given.
