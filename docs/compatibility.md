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
