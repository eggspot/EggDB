# Moving a database to EggDB

Moving to EggDB means restoring the backup or dump your database already makes (D13). Nothing of
the old database runs, is wrapped or is copied live (D110): the dump is read, and what it
describes is created in an EggDB file that every dialect and protocol then reads.

| From | How | Status |
|------|-----|--------|
| PostgreSQL | `pg_dump` plain SQL, restored by `eggdb restore` or `psql -f` against `eggdb serve` | this page |
| PostgreSQL CSV, any table | `eggdb import --csv` ([cli.md](cli.md)) | available |
| SQL Server | SSMS Generate Scripts, `sqlpackage /Action:Script`, restored by `eggdb restore --dialect sqlserver` or `sqlcmd -i` against `eggdb serve` | this page |
| Oracle | SQL Developer and SQLcl exports, `impdp ... SQLFILE=` for the DDL and SQLcl CSV for the rows, installers such as Oracle's sample schemas, restored by `eggdb restore --dialect oracle` | this page |

## PostgreSQL

### 1. Export with PostgreSQL's own tools

Take a dump in plain SQL form:

```sh
pg_dump --dbname=app > app.sql
```

`--inserts` and `--column-inserts` (rows as `INSERT` statements instead of `COPY` data) work too,
as do `--schema-only`, `--data-only`, `--table` and `--schema`. Dumps from PostgreSQL 16, 17 and
18 are tested, including the `\restrict` lines that `pg_dump` 17.6 and later write.

If the backup you have is an archive (`pg_dump -Fc`, `-Fd` or `-Ft`), turn it into plain SQL with
`pg_restore`, which needs no server for this:

```sh
pg_restore -f app.sql app.dump
```

EggDB never reads the archive formats themselves: they have no public specification (D13).
`eggdb restore` recognises them and prints this exact command instead of guessing. A dump
compressed with `pg_dump -Z` is refused the same way, with the command that decompresses it
(`gzip -dc app.sql.gz | eggdb restore --create app.db`).

Dumps must be UTF-8 (`pg_dump` writes the database's encoding; use `--encoding=UTF8` for any
other).

### 2. Restore into EggDB

```sh
eggdb restore --create app.db app.sql
```

or from a pipe, without a file in between:

```sh
pg_dump --dbname=app | eggdb restore --create app.db
```

`--create` builds the new file as `app.db.restoring` and renames it to `app.db` only when the
whole restore succeeded, so a failed or interrupted restore leaves nothing behind. Without
`--create` the restore goes into an existing file, and an error leaves the statements before it
in place. Each statement runs in its own transaction, as `psql -f` runs it; a table's `COPY` data
loads in one transaction, which must fit in the page cache: the cache grows with the dump up to
1 GiB, and `--cache 4g` gives a very large table more.

The same dump also restores through the PostgreSQL protocol:

```sh
eggdb serve --create app.db --password-file pw.txt &
psql "host=127.0.0.1 user=eggdb dbname=app" -f app.sql
```

psql runs the statements one by one and prints an error for each object EggDB cannot create yet,
then goes on, as it does against PostgreSQL; `eggdb restore` lists the same objects in its report
instead. When a table cannot be created (a column of an enum type, a partitioned table), psql
reads its `COPY` data as SQL and may lose the statements after it, as it would against
PostgreSQL: restore such a dump with `eggdb restore`, which skips that data, or take it with
`pg_dump --inserts` for `psql -f`.

### 3. Read the report

`eggdb restore` ends with a report: on a terminal as text, in a pipe (or with `--format json`) as
one JSON object, and with `--report restore.json` in a file while stdout keeps only the verdict.

```
restored app.sql into app.db (1,204 statements, 2.3 s)
  schemas         2
  tables          14, 1,204,311 rows
  views           3
  sequences       9
  indexes         21
  constraints     17
  comments        4
  skipped         3 (function 1, trigger 1, materialized view 1)
  consequences    1
  ignored         61 (OWNER TO 38, GRANT 20, SET 3)
rows per table
  public.orders  1,180,002
  public.users      12,480
skipped (not in EggDB yet; docs/roadmap.md)
  line 23     function public.set_updated_at()          functions come later
  line 310    trigger users_updated on public.users     triggers come later
  line 2,114  materialized view public.daily            materialized views come later
consequences (fail only because of a skipped object)
  line 2,240  REFRESH MATERIALIZED VIEW public.daily;   uses materialized view public.daily
ignored
  OWNER TO    38 statements (EggDB has no roles yet)
  GRANT       20 statements
  SET         3 statements: default_toast_compression (not EggDB parameters)
ok: every table and row of the dump is in app.db; 3 objects skipped and 1 consequence, listed above.
```

- **Created**: schemas, tables and their rows, views, sequences (identity columns included),
  indexes, constraints (primary keys, unique, check and foreign keys with their actions) and
  comments.
- **Skipped**: what EggDB cannot create yet, each with its line in the dump (`sed -n '23p'
  app.sql` shows it) and the reason: functions, procedures and aggregates, triggers and event
  triggers, rules, row-level security policies, types (enums, composites, ranges), domains,
  materialized views, partitioned tables (and their rows; partitions dumped as tables of their
  own are restored), tables with inheritance, publications and subscriptions, extensions other
  than `plpgsql`, collations, text search objects, operators, casts, foreign data, roles and
  tablespaces. They come with later items of the [roadmap](roadmap.md).
- **Consequences**: statements that failed only because they use a skipped object, such as a
  table with a column of a skipped enum type (its rows are then counted as not restored) or a
  `REFRESH` of a skipped materialized view.
- **Ignored**: `GRANT`, `REVOKE` and `OWNER TO` (EggDB has one user per server and no roles yet),
  `ALTER DEFAULT PRIVILEGES`, `SET` of parameters EggDB does not have, storage settings with no
  meaning in EggDB (`REPLICA IDENTITY`, `SET STATISTICS`, `CLUSTER ON`), and psql meta-commands:
  `\connect` (one database per file) and anything else, which is never run.
- **Transaction control** in the dump (`BEGIN`, `COMMIT`, and also `ROLLBACK` and `ABORT`) is
  ignored: each statement commits on its own. Use `--create` for all or nothing.
- **A cut dump** is an error, never a success: one that ends inside a `COPY`'s data before its
  `\.` line, inside a quoted string or comment, or after a `\restrict` with no `\unrestrict`.
- **Errors** stop the restore: a `COPY` row that breaks a constraint, bad syntax in a statement
  EggDB supports, a failed `INSERT`. The message names the dump's line, the data line for a
  `COPY`, with PostgreSQL's wording, detail and SQLSTATE; the exit code is 3. `--keep-going` runs
  on past every error, lists them all and exits 3 at the end.

Skipped objects and consequences do not change the exit code: the verdict line says whether every
table and row of the dump is in the file, or how many of them are.

### 4. Browse it with psql

`eggdb serve` the restored file and point psql at it: `\d`, `\dt`, `\di`, `\dv`, `\ds`, `\dn`,
`\d table`, `\l` and `\du` show the restored tables, indexes, constraints, defaults, views and
sequences as PostgreSQL shows them (`--database` names the database `\l` lists):

```sh
EGGDB_PASSWORD=choose-one eggdb serve shop.db --database shop
PGPASSWORD=choose-one psql -h 127.0.0.1 -U eggdb shop -c '\d customers'
```

Every index shows as `btree`, a unique index as a `UNIQUE` constraint, and `\d+` is not there
yet; [sql.md](sql.md#the-postgresql-catalog) has the details.

### Limits

- Functions, procedures and triggers are not run in this release (D110): a database whose rules
  live in triggers keeps its data, without the triggers.
- One database per file: `pg_dumpall`'s roles and several databases are not restored; restore
  each database's `pg_dump` into its own file.
- `pg_dump --clean` is not supported (EggDB's `public` schema cannot be dropped); restore into a
  new file instead.
- Large objects (`lo_*`), and dumps in encodings other than UTF-8, are not restored.

## SQL Server

### 1. Export with SQL Server's own tools

In SQL Server Management Studio, right-click the database, then **Tasks, Generate Scripts**:
script the entire database, and under **Advanced** set **Types of data to script** to *Schema and
data* and **Script Indexes** to *True*; leave **Check for object existence** at *False*. Save to
one file, as Unicode (SSMS's default, UTF-16 with a byte-order mark) or UTF-8.

`sqlpackage /Action:Script` writes a schema-only script with sqlcmd's `:setvar` lines; take the
data from Generate Scripts with *Data only* and restore it after the schema. `bcp` files load
with `eggdb import --dialect sqlserver --csv` (bcp's character format, `bcp ... out -c`), and
`bcp in` and `SqlBulkCopy` load over TDS (see "Bulk loads with bcp and SqlBulkCopy" in sql.md).

EggDB never reads `.bak` or `.trn` backups, BACPAC or DACPAC packages, or `.mdf`, `.ndf` and
`.ldf` files: they have no public specification (D13). `eggdb restore` recognises them by their
content and says what to do: restore the backup (or import the BACPAC with `sqlpackage
/Action:Import`, or attach the files) into a SQL Server you run, then Generate Scripts there.

### 2. Restore into EggDB

```sh
eggdb restore --dialect sqlserver --create shop.db shop.sql
```

`--dialect mssql` and `--dialect tsql` mean the same. The script runs batch by batch (`GO` ends a
batch, `GO 3` repeats it), each statement in its own transaction, through EggDB's Transact-SQL
dialect: `[dbo]` is EggDB's `public` schema, and names are kept in lower case, as `sqlcmd` users
of EggDB see them (D122). sqlcmd's `:setvar` variables are replaced in the lines after the
script's first sqlcmd command; `:r`, `!!` and every other sqlcmd command are listed and never
run. `--create`, `--keep-going`, `--report` and the exit codes work as for a PostgreSQL dump.

An SSMS script also restores through the TDS protocol, with Microsoft's go-sqlcmd:

```sh
eggdb serve --create shop.db --password-file pw.txt --tds-listen 127.0.0.1:1433 &
SQLCMDPASSWORD=secret sqlcmd -S 127.0.0.1,1433 -U eggdb -d shop -N disable -x -i shop.sql
```

go-sqlcmd reads the UTF-16 file by its byte-order mark; `-x` keeps a `$(...)` inside the data as
written. Statements EggDB does not run print an error and sqlcmd goes on (without `-b`); the
report of `eggdb restore` lists the same objects instead. A sqlpackage script restores only
through `eggdb restore`: its own `:on error exit` stops sqlcmd at the first refused batch.

### 3. Read the report

The report is the PostgreSQL one with one more list, **known differences**: what is in the file
but not as SQL Server had it. For each, the script line, the object and the difference:

```
known differences (restored, but not as the source had it)
  line 88   sales.Orders.PlacedAt   values are stored in UTC; their offsets are lost
  line 102  dbo.Coupons.Id          IDENTITY(1000, 10) restored as IDENTITY(1, 1); new values continue after the highest stored one
  line 140  dbo.Products.Margin     computed column not kept
  line 377  FK_OrderLines_Products  not created: existing rows violate it (...)
```

Skipped objects (procedures, functions, triggers, synonyms, users and roles, batch logic such as
SSMS's `IF` blocks) are listed with their line; `USE`, `CREATE DATABASE`, `ALTER DATABASE`,
`GRANT`, `PRINT`, extended properties and the like are counted as ignored; sqlcmd's commands are
listed as client commands.

### Limits

- **Never read:** `.bak`, `.trn`, BACPAC, DACPAC, `.mdf`, `.ndf`, `.ldf` (above).
- **Not run:** procedures, functions, triggers, synonyms and table types; users, logins, roles
  and permissions; rules and bound defaults; Service Broker; full-text, XML, spatial and
  columnstore indexes, and XML schema collections; Transact-SQL batch logic (`IF`, `WHILE`,
  `DECLARE`, `BEGIN ... END`, including SSMS's *Check for object existence* guards, item 47);
  `MERGE`, `DBCC`, `BULK INSERT` and other statements EggDB does not have, which are skipped and
  listed.
- **Not kept, and listed as known differences:** computed and `rowversion` columns; `IDENTITY`
  seeds and increments other than (1, 1); `DEFAULT` expressions other than constants,
  `GETDATE()` and `NEWID()`; `INCLUDE` columns, a non-unique index's filter and `DESC`;
  `datetimeoffset` offsets (values are kept in UTC); non-binary collations (text compares by code
  point); constraints disabled with `NOCHECK CONSTRAINT` (enforced here); constraints the restored
  rows violate (not created); an index whose name another table's index already has (EggDB's
  index names are unique per schema, not per table: not created); `SET ANSI_NULLS`,
  `QUOTED_IDENTIFIER` or `ANSI_PADDING OFF`.
- **Not kept, without a note:** default-constraint names; extended properties (`MS_Description`);
  filegroups and partitioning (`ON [PRIMARY]`, `ON ps(col)`).
- **Tables skipped, with their rows counted:** columns of `sql_variant`, `xml`, `hierarchyid` or
  the spatial types; alias types; tables with `IGNORE_DUP_KEY = ON`, FILESTREAM, sparse, masked,
  Always Encrypted, memory-optimized, temporal or system-versioned columns; an identity column
  that is not the first primary-key column, or a heap with one.
- **Scripts:** UTF-8 or UTF-16LE with its byte-order mark (UTF-16 big-endian is refused with the
  `iconv` command to convert it); three-part names; a batch holding more than 4 MiB of statements
  other than one `INSERT` must be split with `GO`; `GO 2` and more only after a batch under 1 MiB;
  a syntax error in one part of a batch does not stop the batch's earlier parts, as SQL Server's
  compile-first would.
- Behaviour of restored `CHECK`s and views used from another dialect follows that dialect's
  conversions and error numbers (D124).

## Oracle

### 1. Export with Oracle's own tools

SQL*Plus, SQLcl and SQL Developer cannot connect to EggDB (EggDB has no Oracle wire protocol,
D10), so a schema moves as a script that `eggdb restore --dialect oracle` reads.

- **SQL Developer:** Tools, **Database Export**: DDL and data, data in *insert* format, one file,
  encoding UTF-8, *Include Drop Statements* off. Set the NLS date and timestamp formats first
  (Preferences, Database, NLS: `YYYY-MM-DD HH24:MI:SS` and `YYYY-MM-DD HH24:MI:SS.FF6`), so no
  time of day or century is lost.
- **SQLcl:** `ddl` for the schema, then `set sqlformat insert` with `spool` for the rows; or
  `UNLOAD` per table for CSV, after `ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD HH24:MI:SS'`
  and the timestamp format.
- **A Data Pump `.dmp` file:** EggDB never reads it (no public specification, D13). Write its DDL
  with `impdp` and take the rows from the database it came from:

  ```sh
  impdp system DIRECTORY=dp DUMPFILE=hr.dmp SCHEMAS=hr SQLFILE=hr_tables.sql EXCLUDE=REF_CONSTRAINT,STATISTICS
  impdp system DIRECTORY=dp DUMPFILE=hr.dmp SCHEMAS=hr SQLFILE=hr_refs.sql INCLUDE=REF_CONSTRAINT
  ```

  `impdp` writes the file in the database's character set: convert it to UTF-8 if that is not
  AL32UTF8 (`iconv -f CP1252 -t UTF-8 hr_tables.sql > hr_tables_utf8.sql`). The rows come as CSV
  from SQLcl's `UNLOAD` (`REGIONS_DATA_TABLE.csv` and so on), or from SQL*Plus with
  `SET MARKUP CSV ON QUOTE ON`, `SET FEEDBACK OFF`, `SET TERMOUT OFF` and `SPOOL`.
- **An RMAN backup or a legacy `exp` file:** restore it into an Oracle database first, then use
  one of the routes above. `eggdb restore` recognises these binary files by their content and
  prints the `impdp` route.

### 2. Restore into EggDB

```sh
eggdb restore --dialect oracle --create hr.db hr_export.sql
```

For the Data Pump route, restore the tables, load each CSV (in any order), then add the foreign
keys:

```sh
eggdb restore --dialect oracle --create hr.db hr_tables.sql
eggdb import --dialect oracle hr.db --csv EMPLOYEES_DATA_TABLE.csv --table hr.employees --header
eggdb restore --dialect oracle hr.db hr_refs.sql
```

Installers such as Oracle's HR sample schema (`hr_install.sql`) restore as they are, following
their `@@` includes: `eggdb restore --dialect oracle --create hr.db hr_install.sql`.

The script is read as SQL*Plus reads it. A statement ends at `;` (outside strings, `q'[...]'`
quotes, quoted names and comments) or at a line holding only `/`; a PL/SQL unit (`CREATE
PROCEDURE`, `PACKAGE`, `TRIGGER`, `TYPE`, `DECLARE`, `BEGIN`) ends only at a `/` line, and a line
holding only `.` ends a statement without running it. Each statement runs in its own transaction;
an anonymous block of `INSERT`, `UPDATE`, `DELETE`, `MERGE` and `COMMIT` statements (HR loads its
rows that way) runs as one transaction, all or nothing. In the CSV route, an empty field is NULL
and dates are read by Oracle's NLS format (`DD-MON-RR`), then as ISO 8601.

SQL*Plus commands:

- `DEFINE`, `UNDEFINE`, `SET DEFINE`, `SET ESCAPE` and `SET CONCAT` work, and `&name`, `&&name`
  and `&name.` are replaced as SQL*Plus replaces them. `ACCEPT` never prompts: its variable takes
  its `DEFAULT`, or stays undefined. `--define name=value` defines a variable from the command
  line. An undefined variable is an error only in a statement that runs, a user or schema name or
  an included file's name.
- `@file`, `@@file` and `START file` include another script, with `.sql` added when the name has
  no extension: `@@` from the including script's folder, `@` from the working directory, regular
  files only, at most 20 deep and 10,000 in one restore, never a URL.
- `CONNECT user` puts the objects after it in schema `user` (no login is made, and the password is
  never stored or shown); `EXIT` and `QUIT` end the restore.
- `SET SQLTERMINATOR`, `SET BLOCKTERMINATOR`, `SET SQLPREFIX` and `SET SQLBLANKLINES OFF` change
  how a script is read and are refused. `WHENEVER` is listed: `--keep-going` decides.
- Every other command (`SET`, `PROMPT`, `SPOOL`, `COLUMN`, `HOST`, ...) is listed and never run.

Users become schemas: `CREATE USER hr`, `CONNECT hr`, `ALTER SESSION SET CURRENT_SCHEMA = hr` and
`CREATE TABLE hr.t` make schema `hr` when it is missing. Query it in Oracle SQL after `ALTER
SESSION SET CURRENT_SCHEMA = HR`, or in PostgreSQL SQL as `hr.employees`.

### 3. Read the report

The report is the SQL Server one, in Oracle's words. **Skipped** lists stored PL/SQL (procedures,
functions, packages, triggers, types, Java), PL/SQL blocks that are not plain SQL, synonyms,
database links, materialized views and the like, each with its script and line. **Ignored**
counts `GRANT`, users (*schema created, login not*), `COMMIT`, queries, storage settings
(`ALTER INDEX ... NOPARALLEL` and the like), PL/SQL settings (`ALTER TRIGGER ... DISABLE`,
`ALTER PROCEDURE ... COMPILE`) and the `ALTER SESSION` parameters EggDB does not have
(`EVENTS`). SQL*Plus commands are listed as client commands; a script included with `@@` shows
as `hr_populate.sql line 61`.

A constraint disabled with `ALTER TABLE ... DISABLE CONSTRAINT` is dropped and added again by the
matching `ENABLE` (HR loads its departments that way), checking the rows then. Known differences:

```
known differences (restored, but not as the source had it)
  line 12   emp.emp_boss_fk  existing rows are checked here; Oracle's NOVALIDATE doesn't check them
  line 13   emp              kept enforced: EggDB doesn't disable keys
  end       c.c_fk           disabled in the source: not created here, so not enforced
```

### Limits

- **Never read:** Data Pump `.dmp` and legacy `exp` files, RMAN backups (above).
- **Not run:** stored PL/SQL and PL/SQL blocks other than plain SQL ones (PL/SQL comes after
  v0.2.0); synonyms, database links, materialized views, directories, tablespaces, roles,
  profiles and the other objects EggDB does not have; logins, passwords and grants; queries in
  scripts.
- **Not kept:** storage clauses, `ORGANIZATION INDEX`, LOB storage and other physical properties
  (accepted with no effect); a separate index taken over by a primary key (its name becomes the
  key's); collations other than binary.
- **Not kept, and listed as known differences:** constraints left disabled (not created);
  disabled keys (enforced); `ENABLE NOVALIDATE` (rows are checked); identity columns with
  options (the column and its values are kept, the identity is not); virtual columns (left out);
  partitioning (one unpartitioned table); `ROLLBACK` (the statements before it were committed).
- **SQL*Plus behaviour:** `/` after a statement ended by `;` does not run it again; blank lines
  never end a statement (SQL*Plus's `SQLBLANKLINES ON`); a `/` line inside a string or a block
  comment is part of it; `WHENEVER` is decided by `--keep-going`; `ACCEPT` never prompts;
  `SQLPATH` is not searched; scripts from URLs are never fetched; SQL*Plus commands do not work in
  `eggdb sql` or `eggdb shell`.
- **Scripts:** UTF-8 or UTF-16LE with its byte-order mark; other encodings are refused with an
  `iconv` example. An anonymous block is one transaction, so one larger than the page cache fails
  at its commit.
- **Taking over an index:** a primary key or unique constraint takes over an index of the same
  name on the same columns; an index named differently in `USING INDEX` is not taken over yet
  (the key gets its own index).
