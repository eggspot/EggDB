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
| SQL Server change tracking | 2016+ | follow | expected to work | the TDS client and change-tracking queries; no SQL Server in CI (D162) |
