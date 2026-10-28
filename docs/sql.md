# SQL in EggDB

EggDB runs SQL on the same file, the same transactions and the same crash safety as its
key-value data (D2). The PostgreSQL dialect comes first (D10): a statement that runs here runs
on PostgreSQL with the same result, and one that EggDB does not run yet fails with SQLSTATE
`0A000` (feature not supported) and a hint, never with a different answer.

Run it with [`eggdb sql`](cli.md#eggdb-sql):

```sh
eggdb sql --create shop.db -c "CREATE TABLE items (id integer PRIMARY KEY, name text NOT NULL, price double precision)"
eggdb sql shop.db -c "INSERT INTO items VALUES (1, 'egg', 0.25), (2, 'bread', 2.5) RETURNING id"
eggdb sql shop.db -c "SELECT name, price * 2 AS doubled FROM items WHERE price > 1 ORDER BY name"
eggdb sql shop.db -c "CREATE TABLE sales (id integer PRIMARY KEY, item_id integer, quantity integer)"
eggdb sql shop.db -c "SELECT i.name, count(s.id), sum(s.quantity) FROM items i LEFT JOIN sales s ON s.item_id = i.id GROUP BY i.id ORDER BY 3 DESC"
```

These are the first parts of the SQL core (roadmap items 19, 19b and 30). What comes next is listed
at the end. The same tables also speak MySQL's SQL: see "The MySQL dialect" and "Connecting with
mysql" below (roadmap item 22, D106), SQLite's: see "The SQLite dialect" (item 24, D107), and
Oracle's: see "The Oracle dialect" (item 25, D108).

## Connecting with psql

`eggdb serve` serves a file over the PostgreSQL wire protocol v3 (roadmap item 21, D103), so
psql and PostgreSQL drivers connect to it unchanged:

```sh
export EGGDB_PASSWORD='choose-one'
eggdb serve --create shop.db                 # listening on 127.0.0.1:5432 …; Ctrl-C stops it
```

and in another terminal:

```sh
export PGPASSWORD='choose-one'
psql -h 127.0.0.1 -p 5432 -U eggdb shop
shop=> CREATE TABLE items (id integer PRIMARY KEY, name text NOT NULL);
shop=> CREATE TABLE sales (id integer PRIMARY KEY, item_id integer, quantity integer);
shop=> INSERT INTO items VALUES (1, 'egg'), (2, 'bread');
shop=> INSERT INTO sales VALUES (1, 1, 3), (2, 2, 1), (3, 1, 4);
shop=> SELECT i.name, sum(s.quantity) FROM items i JOIN sales s ON s.item_id = i.id GROUP BY i.name;
shop=> \conninfo
```

A driver connects the same way, for example `postgresql://eggdb@127.0.0.1:5432/shop` with the
password, and runs parameterized queries (`SELECT name FROM items WHERE id = $1`).

**Loopback only, without TLS (D102).** Until EggDB has its own TLS, the server listens only on
a loopback address (`127.0.0.1`, `::1`), so only programs on the same machine connect, and it
answers a request for TLS with no: `sslmode=prefer` (the default) connects without TLS,
`sslmode=require` fails. This note goes when TLS lands.

What the server does:
- **Authentication is always required**: SCRAM-SHA-256, with one user per server (`--user`,
  default `eggdb`) whose password comes from `EGGDB_PASSWORD` or `--password-file`, never the
  command line. A wrong password is `28P01`, and repeated failures are answered more slowly.
- **The simple query protocol**, several statements in one message included, and the
  **extended protocol** (`Parse`, `Bind`, `Describe`, `Execute`, `Close`, `Sync`, `Flush`) with
  named and unnamed statements and portals, `$1..$n` parameters in text or binary form
  (`bool`, `int2`, `int4`, `int8`, `float4`, `float8`, `numeric` in its base-10000 form,
  `date`, `time`, `timetz`, `timestamp`, `timestamptz`, `interval`, `text`, `varchar`, `bpchar`,
  `bytea`, `uuid`, `json`, `jsonb`), results in text or
  binary form with each type's OID, and row limits on `Execute`. A `timestamptz` is sent in
  the session's `TimeZone` when the rows are sent.
- **PostgreSQL's answers**: type OIDs in `RowDescription`, command tags (`SELECT 2`,
  `INSERT 0 1`, `CREATE TABLE`, …), errors with SQLSTATE, severity, message, detail, hint and
  position, notices, and `ReadyForQuery`'s `I`, `T` and `E`. After an error inside `BEGIN`,
  statements fail with `25P02` until `ROLLBACK`, as in PostgreSQL.
- **What drivers ask at connect**: `server_version` (`16.0 (EggDB 0.2.0)`), `client_encoding`
  and `server_encoding` (`UTF8`), `DateStyle`, `TimeZone`, `integer_datetimes`,
  `standard_conforming_strings` and the other reported parameters; `SET`, `SHOW`, `RESET`,
  `version()`, `current_user`, `current_database()` and `current_setting()`.
- **`COPY`** (D117): `COPY ... FROM STDIN` takes `CopyData` messages in pieces of any size,
  ending with `CopyDone`; `CopyFail` fails it with `57014`, and `Flush` and `Sync` are ignored
  until it ends, as libpq expects. `COPY ... TO STDOUT` sends one `CopyData` per row, then
  `CopyDone`. Both work from the simple and the extended protocol, so psql's `\copy` and
  drivers' copy calls work; see [COPY](#copy).
- **Several connections**, one thread and one session each, up to `--max-connections` (64),
  at most 16 of them still logging in; readers run side by side, writers one at a time (a
  second waits 5 s, then `55P03`: retry once the other session's transaction ends).

Not yet (each fails readably or is ignored harmlessly): TLS, psql's `\d` commands and the
catalog queries of GUI tools (`pg_catalog`), `COPY` in the binary format, `LISTEN`/`NOTIFY`, cancelling a running
query, the implicit transaction around a multi-statement `Query` or an extended-protocol
pipeline (each statement commits on its own outside `BEGIN`), stored users and roles, and
non-ASCII passwords.

## The MySQL dialect

`eggdb sql --dialect mysql` and `eggdb shell --dialect mysql` read MySQL's SQL, and every
connection through `eggdb serve --mysql-listen` does (D106). All the dialects work on the same
tables, so a table made through psql is queried with `mysql` and the other way round.

```sh
eggdb sql --create --dialect mysql shop.db -c "CREATE TABLE \`items\` (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(40) NOT NULL) ENGINE=InnoDB"
eggdb sql --dialect mysql shop.db -c "INSERT INTO items (name) VALUES ('egg'), ('bread'); SELECT LAST_INSERT_ID()"
eggdb sql --dialect mysql shop.db -c "INSERT INTO items VALUES (1, 'eggs') ON DUPLICATE KEY UPDATE name = VALUES(name)"
eggdb sql --dialect mysql shop.db -c "SELECT * FROM items LIMIT 0, 10; SHOW TABLES; DESCRIBE items"
```

What it reads, on top of the statements of "What runs today":
- **Lexical rules**: backtick identifiers (folded to lower case like unquoted names, so
  `lower_case_table_names` is 1), `"..."` strings, backslash escapes in strings, `#` and `-- `
  comments, `/*! ... */` run as SQL, `?` placeholders.
- **Statements**: `LIMIT offset, count`, `AUTO_INCREMENT`, `INSERT IGNORE`, `REPLACE`,
  `INSERT ... ON DUPLICATE KEY UPDATE` with `VALUES(col)`, `SHOW TABLES`, `SHOW DATABASES`,
  `DESCRIBE`/`SHOW COLUMNS`, `SHOW CREATE TABLE`, `SHOW VARIABLES [LIKE ...]`, `SHOW WARNINGS`,
  `SHOW COUNT(*) WARNINGS`, `USE`, `SET` of session and user variables (`SET NAMES`, `SET
  autocommit`, `SET @x = ...`), `START TRANSACTION`, `LOCK TABLES`/`UNLOCK TABLES` (accepted),
  table options such as `ENGINE=` and `DEFAULT CHARSET=`; `KEY`/`INDEX` and `UNIQUE [KEY]`
  entries and `UNIQUE` columns make indexes (an unnamed one is named after its first column,
  then `_2`, `_3`), and `CREATE [UNIQUE] INDEX name [USING BTREE] ON t (...)`, `DROP INDEX name
  ON t`, `SHOW INDEX|INDEXES|KEYS FROM t` and `EXPLAIN SELECT` (MySQL's columns: `type` `ALL`,
  `const`, `ref`, `range` or `index`, the `key`, and `Extra`) work as in MySQL; `SHOW CREATE
  TABLE` lists the keys and `DESCRIBE` marks `UNI` and `MUL` columns.
- **Types** as the engine's: `TINYINT(1)` and `BOOLEAN` are booleans (shown as `1` and `0`),
  `TINYINT` and `SMALLINT` are `smallint`, `MEDIUMINT` and `INT` are `integer`, `BIGINT` is
  `bigint`, `UNSIGNED` takes the next wider type, `FLOAT` is `real`, `DOUBLE` and `REAL` are
  `double precision`, `DECIMAL(p,s)`, `DEC`, `NUMERIC` and `FIXED` are exact `numeric(p,s)`
  (`DECIMAL` alone is `DECIMAL(10,0)`; `p` up to 65 and `s` up to 30, else error 1426 or 1425),
  `DATE` is `date`, `DATETIME(fsp)` is `timestamp(fsp)`, `TIMESTAMP(fsp)` is `timestamptz(fsp)`
  (stored in UTC, shown in the session's time zone without an offset, as MySQL shows it; `fsp`
  defaults to 0), `JSON` is `jsonb` (MySQL also sorts keys by length and keeps the last
  duplicate), `CHAR(n)` and `VARCHAR(n)` are `varchar(n)`, the `TEXT` family is `text`,
  `TIME(fsp)` is `time(fsp)` (with `TIME '...'` literals; `fsp` defaults to 0), and `BLOB`,
  `TINYBLOB`, `MEDIUMBLOB`, `LONGBLOB`, `BINARY(n)` and `VARBINARY(n)` are `bytea` (D113);
  `YEAR` and `ENUM` answer error 1235. A value out of a `DECIMAL`'s range is error 1264
  and a malformed or impossible date error 1292. On the wire, `DECIMAL` columns are
  `MYSQL_TYPE_NEWDECIMAL`, dates `DATE`, `DATETIME` and `TIMESTAMP` and times `TIME` (packed in
  the binary protocol), `JSON` columns `MYSQL_TYPE_JSON`, and `bytea` columns `MYSQL_TYPE_BLOB`
  with the binary collation and their raw bytes; a bound parameter sent as a `BLOB`, or whose
  bytes are not UTF-8, is a `bytea`, and a string given to a binary column is stored as its bytes.
  `CURTIME()` and `CURRENT_TIME` give the local time to the second.
- **Date and time functions**: `NOW()`, `SYSDATE()`, `CURRENT_TIMESTAMP[()]`, `LOCALTIME`,
  `LOCALTIMESTAMP` (the session's local time, to the second), `UTC_TIMESTAMP()`, `CURDATE()` and
  `CURRENT_DATE`, also as a column's `DEFAULT`; `EXTRACT(unit FROM d)` with MySQL's units
  (`MICROSECOND` to `YEAR`, `WEEK` in mode 0, and the compound units `SECOND_MICROSECOND` to
  `YEAR_MONTH`) as integers; another unit is error 1064. `TIMESTAMP '...'` and `DATE '...'`
  literals. A binary-protocol row with a date outside years 0 to 9999 is error 1292.
- **Subqueries, `WITH` and set operations** as in "What runs today", with MySQL's rules: a
  derived table needs an alias (error 1248) and unique column names (error 1060), `WITH RECURSIVE` stops after 1,000 steps (error 3636,
  `cte_max_recursion_depth`'s default), there is no `FULL JOIN` (`FULL` is not a keyword, 1064)
  and no `MINUS`, a set operation of numbers and strings gives strings, and the errors are 1242
  ("Subquery returns more than 1 row"), 1241 ("Operand should contain 1 column(s)") and 1222.
  `GREATEST` and `LEAST` are NULL when any argument is.
- **Functions and operators**: `IFNULL`, `IF`, `CONCAT`, `LENGTH` (bytes), `CHAR_LENGTH`,
  `UPPER`/`UCASE`, `LOWER`/`LCASE`, `COALESCE`, `NULLIF`, `ABS`, `DATABASE()`, `SCHEMA()`,
  `VERSION()`, `USER()`, `CURRENT_USER()`, `CONNECTION_ID()`, `LAST_INSERT_ID()`, `@@var`
  (`@@version`, `@@sql_mode`, `@@max_allowed_packet`, ...), `@var`, `DIV`, `MOD`, `/` in
  floating point, `<=>`, `||` and `&&` as `OR` and `AND`, `!`. A result column without an alias
  is named by its text (`SELECT 1+1` gives a column `1+1`).
- **Errors** with MySQL's numbers, SQLSTATEs and wording: `ERROR 1146 (42S02): Table
  'eggdb.missing' doesn't exist` (`eggdb` is the database name in `eggdb sql` and `eggdb shell`;
  through the server it is `--database`'s), 1064 for syntax errors (`... near '...' at line 1`), 1054,
  1062, 1048, 1050, 1051, 1049, 1193 and the rest. `SHOW WARNINGS` lists the last statement's
  warnings and notes, or its error.
- **Transactions as MySQL has them**: an error inside `START TRANSACTION` ... `COMMIT` does not
  end the transaction when the failing statement had not written yet; `SET autocommit = 0`
  keeps a transaction open from the first write to `COMMIT` or `ROLLBACK`; `CREATE TABLE`,
  `DROP TABLE` and `START TRANSACTION` commit an open transaction first.

## The SQLite dialect

`eggdb sql --dialect sqlite`, `eggdb shell --dialect sqlite` and the embedded API
(`eggdb::sql`, [embedded.md](embedded.md#sql)) read SQLite's SQL, as sqlite.org documents it
(roadmap item 24, D107). It works on the same tables as the other dialects. Reading and writing
SQLite's own file format is a later item: EggDB keeps its own file, and SQLite's SQL runs on it.

```sh
eggdb sql --create --dialect sqlite notes.db -c "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL, stars INT DEFAULT 0)"
eggdb sql --dialect sqlite notes.db -c "INSERT INTO notes (body) VALUES ('first'), ('second'); SELECT last_insert_rowid()"
eggdb sql --dialect sqlite notes.db -c "INSERT INTO notes VALUES (1, 'again', 5) ON CONFLICT (id) DO UPDATE SET stars = stars + excluded.stars"
eggdb sql --dialect sqlite notes.db -c "PRAGMA table_info(notes); SELECT name, sql FROM sqlite_schema"
```

What it reads, on top of the statements of "What runs today":
- **Lexical rules**: `"..."`, `` `...` `` and `[...]` identifiers (all case-insensitive, like
  unquoted names), `'...'` strings without backslash escapes, `--` and `/* */` comments (an
  unterminated `/*` ends at the end of the text), the parameters `?`, `?NNN` (1 to 32766),
  `:name`, `@name` and `$name` numbered as SQLite numbers them; a parameter without a value is
  NULL.
- **Type affinity** from the declared type name, by datatype3.html's rules in order: a name
  containing `INT` is an integer (`bigint`, 64 bits), `CHAR`, `CLOB` or `TEXT` text (lengths
  such as `VARCHAR(10)` are not enforced, as in SQLite), `REAL`, `FLOA` or `DOUB` a real
  (`double precision`), no type at all text; the rest (`NUMERIC` affinity) is a real, except
  `BOOLEAN` (an integer) and names containing `DATE` or `TIME` (text, as SQLite's date
  functions keep them: `DATE`, `DATETIME` and `TIMESTAMP` columns hold ISO 8601 text, as an
  SQLite application writes it). `DECIMAL(p,s)` is a real, by the same rule. `BLOB` columns
  wait for the blob type.
- **Row ids**: a column declared exactly `INTEGER PRIMARY KEY` (on the column or as the table's
  one-column key) is the row id: a NULL or missing value takes one more than the largest in
  the table; `AUTOINCREMENT` is accepted on it (and refused elsewhere, as in SQLite);
  `last_insert_rowid()`, `changes()` and `total_changes()` report as SQLite does.
- **Statements**: `CREATE TABLE [IF NOT EXISTS]` with `WITHOUT ROWID` and `STRICT` accepted,
  `REFERENCES` and `FOREIGN KEY` accepted and not enforced (SQLite's default, `PRAGMA
  foreign_keys` off), `ON CONFLICT ABORT|FAIL` in constraints; `INSERT OR
  REPLACE|IGNORE|ABORT|FAIL|ROLLBACK`, `REPLACE INTO`, upserts (`ON CONFLICT [(cols)] DO
  NOTHING` and `DO UPDATE SET ... excluded.col`), `RETURNING`; `LIMIT n OFFSET m`, `LIMIT m, n`
  and a negative `LIMIT` for no limit; `BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE] [TRANSACTION]`,
  `COMMIT`, `END`, `ROLLBACK`, with SQLite's errors for a `BEGIN` inside a transaction and a
  `COMMIT` or `ROLLBACK` outside one; a failing statement inside a transaction leaves it open,
  and `INSERT OR ROLLBACK` rolls it back.
- **Names starting with `sqlite_`** are reserved, as in SQLite: `CREATE TABLE sqlite_x` is
  refused with `object name reserved for internal use`.
- **Length limit**: a function result longer than 1,000,000,000 bytes (SQLite's default
  `SQLITE_MAX_LENGTH`) is refused with `string or blob too big` (`SQLITE_TOOBIG`, 18) before it
  is built.
- **The schema table**: `sqlite_schema` and `sqlite_master` (with `type`, `name`, `tbl_name`,
  `rootpage` and `sql`) list the tables; `sqlite_temp_schema`/`sqlite_temp_master` are empty.
- **PRAGMAs**: `table_info(t)`, `table_list[(t)]`, `index_list(t)`, `index_info(i)`,
  `foreign_keys`, `user_version`, `journal_mode`, `synchronous` and `encoding`; unknown PRAGMAs
  do nothing, as in SQLite.
- **Indexes**: `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON t (...)`, `DROP INDEX [IF
  EXISTS] name`, `UNIQUE` columns and table constraints (indexes named
  `sqlite_autoindex_t_N`, and listed in `sqlite_schema` with their `sql`), and `EXPLAIN QUERY
  PLAN` with SQLite's lines (`SCAN t`, `SEARCH t USING INDEX i (a=? AND b>?)`, `SEARCH t USING
  INTEGER PRIMARY KEY (rowid=?)`, `USE TEMP B-TREE FOR ORDER BY`). A duplicate is `UNIQUE
  constraint failed: t.a` with `SQLITE_CONSTRAINT_UNIQUE` (2067).
- **Expressions**: `||`, `==`, `IS`/`IS NOT` between any two values, `LIKE` ignoring ASCII
  case, `GLOB` with `*` and `?`, integers as conditions (`WHERE active`, `NOT 0`), `TRUE` and
  `FALSE` as 1 and 0, comparisons giving 1 and 0, division and remainder by zero giving NULL,
  and result columns named by their text (`SELECT count(*)` gives a column `count(*)`).
- **Compound selects and subqueries**: `UNION [ALL]`, `INTERSECT` and `EXCEPT`, all of one
  precedence, left to right, without parentheses around operands and without `INTERSECT ALL`;
  values keep their own types (`SELECT 1 UNION SELECT '1'` is two rows). A scalar subquery
  gives its first row, as SQLite's does. `WITH` names a recursive query with or without
  `RECURSIVE`; `RIGHT` and `FULL` joins as SQLite 3.39 added them; no `ANY`/`ALL` and no
  `greatest`/`least` (`max` and `min` take several arguments). Errors: "sub-select returns 2
  columns - expected 1", "SELECTs to the left and right of UNION do not have the same number of
  result columns".
- **Functions**: `ifnull`, `nullif`, `coalesce`, `iif`, `typeof`, `length`, `substr` and
  `substring`, `upper`, `lower` (ASCII only, as SQLite's), `trim`, `ltrim`, `rtrim`, `replace`,
  `instr`, `abs`, `round`, scalar `min` and `max`, `random`, `hex`, `quote`, `printf` and
  `format`, `like`, `glob`, `sqlite_version()` (`3.46.0`), `last_insert_rowid`, `changes`,
  `total_changes`, and the aggregates `count`, `sum`, `avg`, `min` and `max`.
- **Values print as SQLite prints them**: reals with up to 15 significant digits and a decimal
  point (`2.0`, `0.333333333333333`, `1.0e+20`).
- **Errors** with SQLite's wording and result codes: `Error: near "FORM": syntax error`, `no
  such table: t`, `table t already exists`, `no such column: x`, `UNIQUE constraint failed:
  t.id` (`SQLITE_CONSTRAINT_PRIMARYKEY`, 1555), `NOT NULL constraint failed: t.c`
  (`SQLITE_CONSTRAINT_NOTNULL`, 1299), `no such function: f`, `wrong number of arguments to
  function f()`, `datatype mismatch` (`SQLITE_MISMATCH`, 20) and the rest as `SQLITE_ERROR` (1).
  `eggdb sql` and `eggdb shell` print the message and a `Result code:` line;
  `SqlError::vendor_code` gives the extended result code and `sqlite_result_code_name` its
  name.

## The Oracle dialect

`eggdb sql --dialect oracle`, `eggdb shell --dialect oracle` and the embedded API
(`eggdb::sql`, [embedded.md](embedded.md#sql)) read Oracle Database's SQL, as Oracle's SQL
Language Reference documents it (roadmap item 25, D108), on the same tables as the other
dialects. There is no Oracle wire protocol: Oracle's own clients and drivers (SQL*Plus, JDBC
thin, OCI) cannot connect, and none is implemented until a lawful public specification of the
protocol exists (D10). Oracle's SQL runs here, through the CLI and the embedded API.

```sh
eggdb sql --create hr.db --dialect oracle -c "CREATE TABLE emp (empno NUMBER(4) PRIMARY KEY, ename VARCHAR2(10) NOT NULL, sal NUMBER(7,2))"
eggdb sql hr.db --dialect oracle -c "INSERT INTO emp VALUES (7839, 'KING', 5000); INSERT INTO emp VALUES (7698, 'BLAKE', 2850)"
eggdb sql hr.db --dialect oracle -c "SELECT ename, NVL(TO_CHAR(sal, 'FM99,999.00'), '-') pay FROM emp ORDER BY sal DESC FETCH FIRST 1 ROWS ONLY"
```

- **Names fold to upper case**: `emp`, `EMP` and `"EMP"` are one table, and result columns
  are named `ENAME`, `PAY`, `NVL(SAL,0)`. Unquoted names in the other dialects fold to lower
  case, so the catalog keeps a name whose letters are all one case in the other case: a table
  made as `emp` in Oracle is `emp` in PostgreSQL, and one made as `"Emp"` stays `Emp`
  everywhere (D108). Oracle's reserved words (`SIZE`, `LEVEL`, `COMMENT`, `USER`, ...) need
  double quotes as names, as in Oracle.
- **The empty string is NULL**: `''` is NULL in inserts, updates and expressions (`WHERE x =
  ''` matches nothing), and a function result that would be empty is NULL (`SUBSTR('abc', 5)`,
  `TRIM('  ')`, `REPLACE('aa', 'a')`). `||` and `CONCAT` treat NULL as the empty string.
- **Queries**: `FROM DUAL` (also `SELECT` without `FROM` is `ORA-00923`, as in Oracle),
  `OFFSET n ROWS` and `FETCH FIRST|NEXT n ROW|ROWS ONLY` (negative counts as 0), `ROWNUM` in
  `WHERE` with Oracle's meaning (`ROWNUM <= n` keeps the first n rows found, before `GROUP BY`
  and `ORDER BY`; `ROWNUM = 1`; `ROWNUM > 1` and `ROWNUM = 2` find nothing), `CASE`, joins,
  `GROUP BY` and `HAVING`, `LIKE` (case-sensitive), `!=`, `^=` and `~=` for `<>`, `q'[...]'`
  and `N'...'` literals, `:name` and `:1` bind variables, and `/` that divides exactly where
  it can (`7/2` is `3.5`).
- **Types**: `VARCHAR2(n [BYTE|CHAR])`, `NVARCHAR2(n)` and `VARCHAR(n)` as `varchar(n)`
  (the length counts characters), `CHAR(n)`/`NCHAR(n)` as `varchar(n)` (not blank-padded),
  `NUMBER(p)` and `NUMBER(p,0)` as `smallint` (p up to 4), `integer` (up to 9) or `bigint` (up
  to 18), `NUMBER(p,s)` with a scale and `NUMBER(19..38)` as exact `numeric(p,s)` (a value
  that does not fit is `ORA-01438`), `NUMBER` without precision as `numeric`, `FLOAT`, `REAL`,
  `DOUBLE PRECISION` and `BINARY_DOUBLE` as `double precision`, `BINARY_FLOAT` as `real`,
  `INTEGER`/`INT`/`SMALLINT` (Oracle's `NUMBER(38)`) as `bigint`, `CLOB`, `NCLOB` and `LONG` as
  `text`, `BOOLEAN` (23ai), `DATE` as `timestamp(0)` (a date with a time of day, to the second,
  as Oracle's), `TIMESTAMP(p)` as `timestamp(p)` (`p` defaults to 6; 7 to 9 keep 6),
  `TIMESTAMP(p) WITH [LOCAL] TIME ZONE` as `timestamptz(p)` and `JSON` as `jsonb`. `DATE
  '...'` and `TIMESTAMP '...'` literals, `SYSDATE` and `CURRENT_DATE` (the session's local time
  to the second), `SYSTIMESTAMP`, `CURRENT_TIMESTAMP` and `LOCALTIMESTAMP` work; dates show as
  `YYYY-MM-DD HH24:MI:SS`. `EXTRACT(field FROM d)` takes `YEAR`, `MONTH`, `DAY`, `HOUR`,
  `MINUTE`, `SECOND`, `TIMEZONE_HOUR` and `TIMEZONE_MINUTE`; `HOUR`, `MINUTE` and `SECOND` of a
  `DATE` and a zone field of a time stamp without a zone are `ORA-30076`, another field
  `ORA-00907`. `CHAR(n)` and `NCHAR(n)` are blank-padded `character(n)` (compared without
  their trailing blanks; `LENGTH` counts them, as Oracle's does), and `RAW(n)`, `LONG RAW` and
  `BLOB` are `bytea` (D113).
- **Indexes**: `CREATE [UNIQUE] INDEX name ON t (...)` with `TABLESPACE`, `ONLINE`, `LOGGING`
  and `COMPUTE STATISTICS` accepted, `DROP INDEX name`, `UNIQUE` columns and `CONSTRAINT name
  UNIQUE (...)` (an unnamed one is `SYS_C` and seven digits), `USER_INDEXES`/`ALL_INDEXES`
  (`INDEX_NAME`, `INDEX_TYPE`, `TABLE_NAME`, `UNIQUENESS`) and
  `USER_IND_COLUMNS`/`ALL_IND_COLUMNS`; `ORA-00001` for a duplicate, `ORA-01452` when `CREATE
  UNIQUE INDEX` finds one, `ORA-00955` for a name in use and `ORA-01418` for a missing index.
- **Identity columns**: `GENERATED [ALWAYS | BY DEFAULT [ON NULL]] AS IDENTITY` on the first
  column of the primary key, numbered as MySQL's `AUTO_INCREMENT` (D106).
- **Statements**: `CREATE TABLE` (also `IF NOT EXISTS`), `DROP TABLE [IF EXISTS] t [CASCADE
  CONSTRAINTS] [PURGE]`, `TRUNCATE TABLE`, `INSERT`, `UPDATE`, `DELETE [FROM]`, `COMMIT
  [WORK]`, `ROLLBACK [WORK]`.
- **Transactions as Oracle's**: the first change starts a transaction that stays open until
  `COMMIT` or `ROLLBACK`; `CREATE TABLE`, `DROP TABLE` and `TRUNCATE TABLE` commit what is open
  before and after them. `eggdb sql` and `eggdb shell` commit an open transaction when they
  end, as SQL*Plus does on exit; the embedded API rolls it back on `close` unless you commit.
- **Subqueries, subquery factoring and set operators**: `UNION [ALL]`, `INTERSECT [ALL]`,
  `MINUS [ALL]` and `EXCEPT [ALL]` (23ai), all of one precedence, left to right; `WITH name
  (columns) AS (...)` is recursive when it names itself (no `RECURSIVE` keyword); `RIGHT` and
  `FULL [OUTER] JOIN`; derived tables without column lists, and with unique column names
  (`ORA-00918` otherwise). Errors: `ORA-01427` (single-row
  subquery returns more than one row), `ORA-00913` (too many values), `ORA-01789` and
  `ORA-01790` (a set operator's columns differ in number or type).
- **Functions**: `NVL`, `NVL2`, `COALESCE`, `NULLIF`, `DECODE` (NULL matches NULL, the first argument
  evaluated once, up to 255 arguments as in Oracle), `CONCAT`,
  `LENGTH`, `SUBSTR` and `INSTR` (negative positions count from the end), `UPPER`, `LOWER`,
  `INITCAP`, `TRIM([LEADING|TRAILING|BOTH] [c FROM] s)`, `LTRIM`, `RTRIM`, `LPAD`, `RPAD`,
  `REPLACE`, `CHR`, `ASCII`, `ABS`, `ROUND`, `TRUNC`, `MOD`, `CEIL`, `FLOOR`, `POWER`, `SQRT`,
  `SIGN`, `GREATEST`, `LEAST` (NULL if any argument is NULL), `TO_CHAR` for numbers (format
  elements `9`, `0`, `.`, `D`, `,`, `G`, `$`, `S`, `MI`, `FM`; `#`s when the number does not
  fit), `TO_NUMBER`, the aggregates `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, and `USER`. Other
  dialects' functions are not there (`CHAR_LENGTH` is `ORA-00904`), and Oracle's are not in the
  other dialects.
- **Length limit**: `LPAD`, `RPAD`, `REPLACE`, `||`, `UPPER`, `LOWER` and `INITCAP` refuse a result longer than
  1,000,000,000 bytes with `ORA-01489` before they build it.
- **The data dictionary**: `USER_TABLES` and `ALL_TABLES` (`TABLE_NAME`, `TABLESPACE_NAME`,
  and `OWNER` in `ALL_`), `USER_TAB_COLUMNS` and `ALL_TAB_COLUMNS` (`TABLE_NAME`,
  `COLUMN_NAME`, `DATA_TYPE`, `DATA_LENGTH`, `DATA_PRECISION`, `DATA_SCALE`, `NULLABLE`,
  `COLUMN_ID`), with `WHERE`, `ORDER BY` and the rest of `SELECT` on them. The owner is the
  session user in upper case (`EGGDB` by default), and `EGGDB.EMP` names the same table as
  `EMP`.
- **Errors** with Oracle's numbers and wording: `ORA-00942: table or view does not exist`,
  `ORA-00904: "X": invalid identifier`, `ORA-00001: unique constraint (EGGDB.EMP_PKEY)
  violated`, `ORA-01400: cannot insert NULL into ("EGGDB"."EMP"."ENAME")`, `ORA-01407`,
  `ORA-00955`, `ORA-00923`, `ORA-00933`, `ORA-00936`, `ORA-00900`, `ORA-01722: invalid
  number`, `ORA-01476: divisor is equal to zero`, `ORA-12899`, `ORA-01426`, `ORA-01428`,
  `ORA-00938`, `ORA-00939`, `ORA-00906`, `ORA-01741`, `ORA-01756`, `ORA-00972`, and
  `ORA-03001: unimplemented feature: ...` for what EggDB does not do yet.
- **Values print as Oracle prints numbers**: no trailing zeros and no zero before the point
  (`.5`), `TRUE` and `FALSE`.

## The SQL Server dialect

`eggdb sql --dialect sqlserver` (also `mssql` and `tsql`), `eggdb shell --dialect sqlserver`,
MCP's `dialect: "sqlserver"` and every connection over SQL Server's protocol (`eggdb serve
--tds-listen`) read Microsoft SQL Server's Transact-SQL, as Microsoft's Transact-SQL reference
documents it (roadmap item 23a, D122), on the same tables as the other dialects:

```sql
CREATE TABLE [dbo].[Bakes] ([id] INT IDENTITY(1,1) NOT NULL, [name] NVARCHAR(40) NOT NULL,
  CONSTRAINT [PK_Bakes] PRIMARY KEY ([id]))
INSERT INTO bakes (name) VALUES (N'bread'), (N'egg')
SELECT TOP 1 [name], GETDATE() AS at FROM dbo.bakes ORDER BY id DESC
SELECT @@VERSION
```

- **Names**: `[brackets]` (with `]]` for a `]`) and `"double quotes"` delimit names, and every
  name and alias folds to lower case, so `[Bakes]`, `bakes` and `"BAKES"` are one table, as in
  SQL Server's default case-insensitive collations; results show names in lower case. Schema
  `dbo` is PostgreSQL's `public`, so a table made in Transact-SQL is `public.bakes` to psql.
- **Statements need no semicolons**: a statement ends where the next one's first word (`SELECT`,
  `INSERT`, `BEGIN`, `SET`, ...) begins. `GO` separates batches in `sqlcmd`, which removes it
  before sending; EggDB reads it as a syntax error, as SQL Server does (scripts with `GO` for
  `eggdb restore` come with item 40).
- `SELECT TOP n` and `TOP (expression)`; `N'...'` strings; `--` and nested `/* */` comments;
  the operators `+ - * / % = <> != < > <= >= !< !>`.
- **Types**: `int`, `bigint`, `smallint`, `decimal`/`numeric(p,s)` (18,0 without a precision,
  at most 38 digits), `float(n)`, `real`, `char`/`nchar`/`varchar`/`nvarchar(n | max)`
  (1 when no length is given), `text`, `ntext`, `date` and `datetime2(n)`.
- **`IDENTITY` and `IDENTITY(1,1)`** number an integer column that is the first column of the
  primary key, inline or as a table-level `CONSTRAINT ... PRIMARY KEY`. An `INSERT` without a
  column list leaves it out, and an explicit value is error 544, as with `IDENTITY_INSERT OFF`.
- `GETDATE()` and `CURRENT_TIMESTAMP` (milliseconds), `SYSDATETIME()`, `GETUTCDATE()`,
  `SYSUTCDATETIME()` (the session's time zone is UTC), `UPPER`, `LOWER`, `ABS`, `COALESCE`,
  `NULLIF`, `CAST` and the aggregates. `@@VERSION`, `@@SPID`, `@@SERVERNAME`, `@@TRANCOUNT`,
  `@@LANGUAGE`. Another function is error 195, another `@@` name or `@variable` error 137.
- **Transactions**: each statement commits on its own unless `BEGIN TRAN[SACTION]` opened a
  transaction; `BEGIN TRAN` nests (`@@TRANCOUNT`), the outermost `COMMIT [TRAN | WORK]`
  commits, `ROLLBACK [TRAN | WORK]` rolls everything back, and without a transaction they are
  errors 3902 and 3903. DDL is transactional.
- `USE` the served database (another is error 911), and `SET NOCOUNT`, `ANSI_NULLS`,
  `QUOTED_IDENTIFIER`, `ANSI_PADDING`, `ANSI_WARNINGS`, `ARITHABORT`, `CONCAT_NULL_YIELDS_NULL`
  and `ANSI_NULL_DFLT_ON` `ON`, `IMPLICIT_TRANSACTIONS OFF`, `TEXTSIZE`, `LANGUAGE us_english`,
  `DATEFORMAT mdy`, `LOCK_TIMEOUT` and `TRANSACTION ISOLATION LEVEL` (all run serializable).
- **Errors** carry SQL Server's numbers, severities and wording: `Msg 208, Level 16, State 1:
  Invalid object name 'missing'.` in `eggdb sql`, and the same fields in TDS's ERROR token.
  Features EggDB lacks are 40517 (`... is not supported in this version of EggDB.`).

## Connecting with mysql

`eggdb serve --mysql-listen <address>` also serves the file over MySQL's client/server protocol
(roadmap item 22, D106), beside the PostgreSQL listener, to the same user and password:

```sh
export EGGDB_PASSWORD='choose-one'
eggdb serve --create shop.db --mysql-listen 127.0.0.1:3306
```

and in another terminal:

```sh
MYSQL_PWD='choose-one' mysql -h 127.0.0.1 -P 3306 -u eggdb shop
mysql> CREATE TABLE items (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(40) NOT NULL);
mysql> INSERT INTO items (name) VALUES ('egg'), ('bread');
mysql> SELECT * FROM items;
mysql> SHOW TABLES;
```

A driver connects the same way (PyMySQL, mysql-connector, Connector/J, mysql2, ...) with
`127.0.0.1`, port 3306, user `eggdb`, the password and database `shop`, and runs
parameterized queries, client-side or as server prepared statements.

What the server does:
- **Authentication is always required**: `caching_sha2_password` (MySQL 8's default; its fast
  path) or `mysql_native_password` (MariaDB's clients use it), one user per server, and error
  1045 `Access denied for user ...` for a wrong password, answered more slowly after repeated
  failures. Passwords must be ASCII, as for the PostgreSQL listener.
- **Loopback only, without TLS (D102)**: the handshake does not offer TLS, so `mysql` and
  drivers with TLS preferred (the default) connect without it on loopback, and those requiring
  it fail. `--mysql-listen` refuses addresses that are not loopback.
- **`COM_QUERY`** with text result sets, several statements per query for clients that ask for
  it (the `mysql` client does), OK packets with affected rows, the `AUTO_INCREMENT` value and
  the warning count, and status flags for open transactions and `autocommit`.
- **Prepared statements**: `COM_STMT_PREPARE`, `COM_STMT_EXECUTE` with binary parameters and
  binary rows, `COM_STMT_SEND_LONG_DATA`, `COM_STMT_RESET`, `COM_STMT_CLOSE`.
- **What clients ask at connect**: `select @@version_comment limit 1`, `SET NAMES`, `SET
  autocommit`, `@@max_allowed_packet`, Connector/J's list of session variables, `SHOW
  VARIABLES`, `SHOW WARNINGS`; `COM_INIT_DB`, `COM_PING`, `COM_RESET_CONNECTION`,
  `COM_FIELD_LIST`, `COM_SET_OPTION`. The server reports itself as `8.0.36-EggDB`.
- **Several connections**, up to `--max-connections` (64) for this listener, at most 16 of them
  still logging in; the next gets error 1040. A stop ends idle connections with error 1053.

Not yet: TLS, compression, `LOAD DATA LOCAL INFILE`, `CLIENT_DEPRECATE_EOF`, query attributes,
`information_schema` and the catalog queries of GUI tools (MySQL Workbench, DBeaver), dates and
times, stored users, and `caching_sha2_password`'s full authentication (which needs TLS or RSA;
the fast path always applies, since the server checks every password itself).

## Connecting with sqlcmd

`eggdb serve --tds-listen <address>` also serves the file over SQL Server's TDS protocol, version
7.4 from Microsoft's [MS-TDS] specification (roadmap item 23a, D122), beside the PostgreSQL
listener, to the same user and password as a SQL login:

```sh
export EGGDB_PASSWORD='choose-one'
eggdb serve --create shop.db --tds-listen 127.0.0.1:1433
```

and in another terminal, with Microsoft's `sqlcmd` (go-sqlcmd, MIT-licensed):

```sh
export SQLCMDPASSWORD='choose-one'
sqlcmd -S 127.0.0.1,1433 -U eggdb -d shop -Q "SELECT @@VERSION"
sqlcmd -S 127.0.0.1,1433 -U eggdb -d shop -i schema.sql
```

What the server does:
- **No encryption**: the server answers that encryption is not available. go-sqlcmd's default
  (`-N` not given) and `-N disable` connect without it; `-N true`, `mandatory` and `strict`
  (TDS 8) fail, as do drivers set to `Encrypt=true`. The login sends its password only
  obfuscated, so in effect in the clear, which is why the listener is loopback only (D102).
- **SQL logins only**, one per server, TDS 7.4 only: a wrong password, an unknown user, Windows
  (SSPI) or Azure AD logins are error 18456 `Login failed for user '...'`, answered more slowly
  after repeated failures; another database than the served one is error 4060.
- **SQL batches** run with SQL Server's rules: the whole batch is parsed first, so a syntax error
  runs nothing; a duplicate key or other constraint violation ends only its statement; any other
  error ends the batch. Results come back with their columns' types, `(n rows affected)` counts
  unless `SET NOCOUNT ON`, and transaction changes.
- **Several connections**, up to `--max-connections` (64) for this listener, at most 16 of them
  still logging in; the next gets error 17809. `--log-batches` logs the first 200 characters of
  each batch, to see what a client sends.

Not yet: TLS, Microsoft.Data.SqlClient and mssql-jdbc (they send parameterized queries as RPC
requests, item 23b), bulk load (`bcp`, item 46), MARS, cancelling a running batch (attention is
answered when the batch ends), and SSMS's and Azure Data Studio's catalog queries (item 41).

## What runs today

**Statements**
- `CREATE TABLE [IF NOT EXISTS] name (column type [NOT NULL | NULL | PRIMARY KEY | UNIQUE | DEFAULT expr | CHECK (expr) | REFERENCES parent [(columns)] [actions]] ..., [PRIMARY KEY (columns)], [UNIQUE (columns)], [CHECK (expr)], [FOREIGN KEY (columns) REFERENCES parent [(columns)] [actions]] ...)`.
  A table without a primary key keeps its rows in insertion order under a hidden row number.
  `CONSTRAINT name` names a constraint (otherwise PostgreSQL's names: `table_pkey`,
  `table_column_key`, `table_column_check`, `table_column_fkey`). At most 1,600 columns, as in
  PostgreSQL. Constraints are described under
  [Sequences, constraints and ALTER TABLE](#sequences-constraints-and-alter-table).
- `ALTER TABLE [IF EXISTS] [ONLY] name action [, ...]`, `CREATE SEQUENCE`, `ALTER SEQUENCE`
  and `DROP SEQUENCE [IF EXISTS] name [, ...] [CASCADE | RESTRICT]`: described under
  [Sequences, constraints and ALTER TABLE](#sequences-constraints-and-alter-table).
- `CREATE [UNIQUE] INDEX [IF NOT EXISTS] [name] ON table [USING btree | hash] (column [ASC | DESC] [NULLS FIRST | LAST], ...)`
  and `DROP INDEX [IF EXISTS] name [, ...] [RESTRICT]`: secondary indexes, described under
  [Indexes](#indexes). An unnamed index is `table_columns_idx`.
- `EXPLAIN statement` for `SELECT`, `UPDATE` and `DELETE`: the plan as PostgreSQL prints it
  without costs (`Seq Scan on t`, `Index Scan using t_b on t`, `Index Scan Backward ...`,
  `Sort`, `Limit`, `Nested Loop`, `Hash Join`).
- `DROP TABLE [IF EXISTS] name [, ...] [CASCADE | RESTRICT]`: removes the definition and every
  row, in one transaction, and the sequences its columns own. A table another table's foreign
  key references, or whose owned sequence another table's default uses, is `2BP01` unless
  `CASCADE`, which drops those foreign keys or defaults (not the tables).
- `INSERT INTO name [(columns)] VALUES (...), (...) | DEFAULT VALUES [RETURNING items]`, with
  `DEFAULT` in a values list.
- `SELECT [ALL | DISTINCT] items [FROM from_item [join ...]] [WHERE expr] [GROUP BY expr, ...] [HAVING expr] [ORDER BY expr [ASC | DESC] [NULLS FIRST | LAST], ...] [LIMIT n | ALL] [OFFSET n [ROW | ROWS]]`.
  `ORDER BY` takes an output column's number or alias, or any expression over the tables (with
  `DISTINCT`, only expressions in the select list, else `42P10`); a `SELECT` without `FROM`
  returns one row.
- **Joins**: `a [INNER] JOIN b ON cond`, `a LEFT [OUTER] JOIN b ON cond`, `a RIGHT [OUTER] JOIN
  b ON cond`, `a FULL [OUTER] JOIN b ON cond`, `JOIN ... USING (columns)`,
  `a CROSS JOIN b` and `FROM a, b`, chained left to right, each table with an optional alias.
  A right join keeps every row of its right table, a full join every row of both; the rows of
  the right table that matched nothing come after the others, extended with NULLs. With
  `RIGHT JOIN ... USING`, the `USING` column is the right table's; `FULL JOIN ... USING`
  answers `0A000` (write `ON` and `coalesce`).
  Columns are named `name` or `alias.name`; `alias.*` expands one table and `*` all of them,
  with a `USING` column once, first, as PostgreSQL does. A left join extends a left row that
  matches nothing with NULLs; conditions in `ON` decide the match, conditions in `WHERE`
  filter afterwards. A `JOIN` binds tighter than a comma, so `FROM a, b JOIN c ON ...` cannot
  name `a` in its `ON` (`42P01`, "invalid reference to FROM-clause entry").
- **Subqueries**: a scalar subquery `(SELECT ...)` anywhere a value goes (NULL when it returns
  no row, `21000` "more than one row returned by a subquery used as an expression" when it
  returns two, `42601` when it has more than one column); `expr [NOT] IN (SELECT ...)`,
  `[NOT] EXISTS (SELECT ...)` and `expr op ANY | SOME | ALL (SELECT ...)` with `=`, `<>`, `<`,
  `<=`, `>`, `>=`, in three-valued logic as the manual describes (`x NOT IN` a list holding a
  NULL is never true; `ALL` over no rows is true, `ANY` false). A subquery in `FROM`,
  `(SELECT ...) [AS] alias [(column, ...)]`, is a derived table; the alias may be left out.
  A derived table or `WITH` query may have two columns of one name (`SELECT * FROM t x, t y`):
  `*` returns both, and naming that column is `42702` ("column reference "a" is ambiguous").
  A subquery may name the columns of the query around it (a correlated subquery) in its own
  select list, `WHERE`, `HAVING` and `JOIN ... ON`, and stand in that query's select list,
  `WHERE` or `ORDER BY` (in a grouped query, in `WHERE` only); it then runs once for each
  distinct set of the outer values it names, read by key or index where its conditions allow,
  and the others run once per statement; an `EXISTS` whose outer references are only
  `inner = outer` conditions in `WHERE` runs once, into a hash set the outer rows look up. `INSERT ... VALUES`, `UPDATE` and `DELETE` take subqueries
  that do not name the changed table's columns.
- **`WITH` queries**: `WITH name [(columns)] AS [[NOT] MATERIALIZED] (query), ...` before a
  query or a subquery, each name visible to the ones after it and to the query; every `WITH`
  query is run once and its rows kept for the statement, however often it is named.
  `WITH RECURSIVE name (columns) AS (non-recursive-term UNION [ALL] recursive-term)` repeats the
  recursive term over the rows the last step added until a step adds none (`UNION` drops rows
  seen before); a recursion still adding rows after 1,000,000 steps fails with `54001`.
- **Set operations**: `query UNION | INTERSECT | EXCEPT [ALL | DISTINCT] query`, with operands in
  parentheses (which may have their own `ORDER BY` and `LIMIT`), `INTERSECT` binding tighter
  than `UNION` and `EXCEPT`, and `ORDER BY`, `LIMIT` and `OFFSET` after the last operand applying
  to the whole result; `ORDER BY` there takes result columns by name or number (an expression is
  `0A000`, as in PostgreSQL). Columns take one type by the manual's `UNION` rules: integers
  widen to `bigint`, then `numeric`, `real`, `double precision`; `varchar` and `text` give
  `text`; `date`, `timestamp`, `timestamptz` widen in that order; a constant string or NULL
  takes the other side's type; anything else is `42804` ("UNION types integer and boolean
  cannot be matched"); different column counts are `42601`. The result is named after the first
  operand. Duplicates are found by value, NULLs equal, as `DISTINCT` does.
- **Conditional expressions**: `CASE` (simple and searched), `coalesce`, `nullif`, `greatest`
  and `least` (which skip NULLs); `coalesce`, `greatest` and `least` give the common type of
  their arguments.
- **Grouping**: `GROUP BY` takes expressions over the tables, an output column's number, or an
  output alias that is not also an input column. Every column the select list, `HAVING` or
  `ORDER BY` uses outside an aggregate must be grouped (`42803`), except the columns of a table
  whose whole primary key is grouped, as PostgreSQL allows. A query with an aggregate or
  `HAVING` and no `GROUP BY` returns one row, even over no rows. Groups come out in no promised
  order; use `ORDER BY`.
- **Aggregates**: `count(*)`, `count(expr)`, `sum`, `avg`, `min`, `max`, each with `DISTINCT` or
  `ALL`. They skip NULLs; `count` of nothing is 0, the others are NULL. Result types are
  PostgreSQL's: `count` is `bigint`; `sum` of `smallint` or `integer` is `bigint`, of `bigint`
  or `numeric` is an exact `numeric`, of `real` is `real`, of `double precision` is `double
  precision`; `avg` of an integer or `numeric` is an exact `numeric` (divided at PostgreSQL's
  scale, `avg` of 1 and 2 is `1.5000000000000000`), of a float `double precision`; `min` and
  `max` keep their argument's type and order NaN above every number, as `ORDER BY` does. `sum`
  and `avg` of text, and `min` and `max` of booleans and `json`, fail with `42883`, as there.
- `UPDATE name [[AS] alias] SET column = expr | DEFAULT, ... [WHERE expr] [RETURNING items]`.
  Changing a primary key moves the row; the uniqueness check runs after every row moved, so
  `UPDATE t SET id = id + 1` succeeds (PostgreSQL may refuse it, depending on the order it
  visits rows).
- `DELETE FROM name [[AS] alias] [WHERE expr] [RETURNING items]`.
- `BEGIN [WORK | TRANSACTION] [READ ONLY | READ WRITE] [ISOLATION LEVEL ...]`,
  `START TRANSACTION`, `COMMIT`/`END`, `ROLLBACK`/`ABORT`.
- `SET [SESSION] name {TO | =} value | DEFAULT`, `SET TIME ZONE`, `SET NAMES`, `RESET name | ALL`,
  `SHOW name | ALL`: PostgreSQL's run-time parameters (D104). `client_encoding` takes `UTF8`
  (and `SQL_ASCII`, which passes bytes through) only; settings EggDB does not apply yet, such as `statement_timeout` and `search_path`, are
  remembered and shown; `SET LOCAL` and `SET TRANSACTION` are not supported yet.
- `$1`, `$2`, …: parameters, given through the PostgreSQL protocol's extended query messages
  (D104); outside it, `$1` is `42P02` (there is no parameter `$1`).
- `version()`, `current_user`, `session_user`, `user`, `current_database()`, `current_schema`
  and `current_setting('name')`.
- `serial`, `bigserial` and `smallserial` (`serial4`, `serial8`, `serial2`) columns, and
  `GENERATED ALWAYS AS IDENTITY` and `GENERATED BY DEFAULT AS IDENTITY` on a `smallint`,
  `integer` or `bigint` column: the column takes the next number when an `INSERT` leaves it out
  (D111). The column must be the first column of the primary key; a named sequence and
  `DEFAULT nextval('s')` have no such limit; identity options (`START WITH ...`) in `CREATE TABLE`
  and generated columns (`GENERATED ALWAYS AS (expr) STORED`) answer `0A000`. `ALTER TABLE t
  ALTER COLUMN c ADD GENERATED {ALWAYS | BY DEFAULT} AS IDENTITY [(SEQUENCE NAME s START WITH
  ...)]`, as `pg_dump` writes identity columns, makes a sequence owned by the column (`s`, or
  `t_c_seq`) and the default `nextval('s'::regclass)`, so `setval('s', ...)` works and the column
  has none of the auto-increment column's limits; `INSERT ... OVERRIDING SYSTEM VALUE` is
  accepted (every identity column takes a given value), `OVERRIDING USER VALUE` answers `0A000`
  (D118).
- `CREATE UNLOGGED TABLE` makes an ordinary, logged table (D118); `TEMPORARY` tables answer
  `0A000`.
- `x = ANY (ARRAY[a, b])`, `x <> ALL (ARRAY[a, b])` and the other comparisons with `ANY`, `SOME`
  or `ALL` over a constant `ARRAY[...]` list, also in parentheses and with a cast such as
  `::text[]` (how `pg_dump` writes the `IN` lists of `CHECK` constraints): `= ANY` is `IN`, `<> ALL`
  is `NOT IN`, and the others compare with each item in three-valued logic, on a column or a
  constant (D118).

**Types** (one type system for every model, D19)

| Type | Also written | Stored as |
|------|--------------|-----------|
| `smallint` | `int2` | 16-bit signed integer |
| `integer` | `int`, `int4` | 32-bit signed integer |
| `bigint` | `int8` | 64-bit signed integer |
| `real` | `float4`, `float(1)` to `float(24)` | IEEE 754 single precision |
| `double precision` | `float8`, `float`, `float(25)` to `float(53)` | IEEE 754 double precision |
| `boolean` | `bool` | true or false |
| `text` | | UTF-8 text of any length |
| `character varying(n)` | `varchar(n)`, `varchar` | UTF-8 text of at most `n` characters |
| `numeric(p, s)` | `decimal(p, s)`, `dec`, `numeric` | exact decimal: up to 1,000 digits before the point and 1,000 after, or `NaN` |
| `date` | | a day, 4714-11-24 BC to 5874897-12-31 AD |
| `timestamp(p) [without time zone]` | `timestamp` | a date and time of day to the microsecond, 4714-11-24 BC to 294276-12-31 AD |
| `timestamp(p) with time zone` | `timestamptz` | an instant in the same range, stored in UTC |
| `time(p) [without time zone]` | `time` | a time of day, `00:00:00` to `24:00:00`, to the microsecond |
| `time(p) with time zone` | `timetz` | a time of day and its offset from UTC (up to 15:59:59 either way) |
| `interval fields (p)` | `interval` | months, days and microseconds, kept apart (D113) |
| `character(n)` | `char(n)`, `char`, `bpchar(n)`, `bpchar` | UTF-8 text padded with spaces to `n` characters (`char` is `char(1)`, `bpchar` unpadded) |
| `bytea` | | bytes, up to 1,000,000,000 |
| `uuid` | | 16 bytes |
| `json` | | JSON text, checked and kept exactly as written |
| `jsonb` | | JSON, normalized: keys sorted by length then bytes, the last duplicate kept, numbers as `numeric` |

Values follow PostgreSQL's rules: integer arithmetic stays in the wider operand's type and
fails with `22003` on overflow (`integer out of range`); `/` on integers truncates toward zero;
division by zero fails with `22012`; floating-point results that overflow fail; `real` and
`double precision` print in the shortest form that reads back exactly, with an exponent from
`1e+15` (`1e+06` for `real`), `NaN` and `Infinity`; `NaN` equals itself and sorts above every
other number. Assigning a longer string to `varchar(n)` fails with `22001` unless the excess is
spaces, and a cast truncates; a value of any type assigned to a text column is stored as its
text (`true` as `true`). A quoted constant such as `'42'` takes the type its context needs
(`WHERE id = '42'`, `INSERT INTO t (flag) VALUES ('yes')`), as PostgreSQL's "unknown" type does.

`numeric` follows PostgreSQL's rules too (D111): a decimal constant (`1.5`, `1e3`) is `numeric`,
and keeps the digits after the point it was written with (`1.50` prints `1.50` and equals
`1.5`). `+`, `-` and `*` are exact, with the larger scale (the sum of the scales for `*`); `/`
rounds to PostgreSQL's scale for a quotient (at least 16 significant digits: `1 / 3::numeric` is
`0.33333333333333333333`); `%` keeps the dividend's sign. Mixed with an integer the result is
`numeric`, mixed with a float it is `double precision`. `numeric(p, s)` rounds to `s` digits,
half away from zero, and a value with more than `p - s` digits before the point fails with
`22003` (`numeric field overflow`); a negative `s` rounds to tens, hundreds and so on. A cast to
an integer rounds half away from zero; a float becomes `numeric` with its 15 (`real`: 6)
significant digits, as PostgreSQL's cast. `round`, `trunc` (both with an optional number of
places), `ceil`/`ceiling`, `floor` and `abs` take `numeric` (and floats). `NaN` is accepted,
equals itself and sorts above every number. A result with more than 1,000 digits before the
point fails with `22003` (`value overflows numeric format`) before it is computed, and `^` with
an integer exponent is exact up to that size (otherwise computed in double precision, through
logarithms past double precision's range, and rounded): `0.5 ^ 1e400` is 0, `1 ^ 1e400` is 1
and `10 ^ 400.5` has 401 digits before the point.

`date`, `timestamp` and `timestamptz` read ISO 8601 and PostgreSQL's ISO style: `2024-02-29`,
`2024-02-29 13:45:01.123456`, `2024-02-29T13:45:01Z`, an offset `+05:30`, `-0330` or `+05`
(`timestamptz` without one is read in the session's `TimeZone`), and ` BC` for years before 1
AD; more than six fractional digits round to the microsecond, and `24:00:00` is the next
midnight. They print in the ISO style (`2024-02-29 13:45:01.5`, `0044-03-15 BC`, a
`timestamptz` as `2024-02-29 08:15:01+00` in the session's time zone). An impossible date
(`2023-02-29`) fails with `22008`, text that is no date with `22007`, a value outside the range
with `22008`. `timestamp(p)` rounds to `p` fractional digits. `date + integer` and `date -
integer` move by days and `date - date` is the number of days between; casts between the three
types, and comparisons across them, use the session's time zone where one side has a zone.
`now()`, `current_timestamp`, `transaction_timestamp()`, `statement_timestamp()` and
`clock_timestamp()` (all the statement's start time), `localtimestamp` and `current_date`;
`extract(field FROM value)` (a `numeric`) and `date_part('field', value)` (a `double
precision`) with `year`, `month`, `day`, `hour`, `minute`, `second`, `milliseconds`,
`microseconds`, `dow`, `isodow`, `doy`, `week`, `isoyear`, `quarter`, `decade`, `century`,
`millennium`, `julian`, `epoch` and the `timezone` fields; `date_trunc('field', value)` from
`microseconds` to `millennium`. `TimeZone` takes `UTC` (and its aliases such as `Etc/UTC`) and
fixed offsets written as POSIX does (`UTC+3`, `EST5`, `+05:30`: the sign means west of
Greenwich, as in PostgreSQL); a named region such as `Europe/Rome` is remembered and shown but
acts as UTC until the time zone database comes (D111).

`time` reads `04:05:06.789`, `04:05 PM` and the time of a date-time string, and `timetz` the
same with an offset (`04:05:06+02`; without one, the session's); both print in the ISO style,
`time(p)` rounds to `p` fractional digits, `24:00:00` is the last valid time and anything later
fails with `22008`. `interval` reads PostgreSQL's format (`1 year 2 mons 3 days 04:05:06`,
`3 days ago`, units from `microseconds` to `millennium` with their abbreviations, fractions such
as `1.5 months` spilling into days and hours, the SQL standard's `1-2` and `3 4:05:06`) and ISO
8601's (`P1Y2M3DT4H5M6S`, `P2W`), and prints PostgreSQL's `postgres` style (`-1 days
+02:03:04`). `interval year to month`, `day to second(3)` and the other field restrictions
truncate, and a bare number counts in the last field (`interval '5' day`); a field past its
32-bit (months, days) or 64-bit (microseconds) range fails with `22015`. Intervals are equal
when their lengths are, with a month counted as 30 days and a day as 24 hours: `'1 mon' =
'30 days'`, as in PostgreSQL, so one unique index or `GROUP BY` group holds both (D113).
`timestamp ± interval`, `date ± interval` (a `timestamp`), `timestamptz ± interval` (calendar
arithmetic in the session's time zone), `time ± interval` (wrapping at midnight),
`interval ± interval`, `interval * number`, `number * interval`, `interval / number`, unary `-`,
`timestamp - timestamp` (days and time), `time - time` and `date + time` work as in PostgreSQL;
adding months keeps the day of the month or clamps it to the month's last day (`2024-01-31 +
1 mon` is `2024-02-29`), and a result out of range fails with `22008`. `justify_days`,
`justify_hours`, `justify_interval`, `age(t1, t2)` and `age(t)`, `current_time`, `localtime`, and
`extract`/`date_part` of an interval (`year` to `microseconds`, `epoch` with 365.25-day years)
or a time work.

`bytea` reads the hex format (`'\xDEADBEEF'`, an odd number of digits is `22023`) and the escape
format (`\\` and `\ooo` octal; another backslash is `22P02`), prints hex and compares byte by
byte; `||`, `length`, `octet_length`, `substring(b FROM s FOR n)` or `substring(b, s, n)`,
`get_byte(b, n)` (`2202E` out of range), `encode(b, 'hex' | 'base64' | 'escape')` and
`decode(text, ...)` work as in PostgreSQL, and a result over the size limit fails with `54000`
before it is built. `uuid` reads upper or lower case, with braces, without hyphens or with a
hyphen after any four digits, prints lower case with hyphens, and `gen_random_uuid()` makes a
random version 4 UUID from the operating system's secure generator. `character(n)` pads with
spaces when stored and fails with `22001` when a value is longer (unless the excess is spaces; a
cast truncates); its trailing spaces do not count when it is compared, grouped or indexed and
are removed when it becomes text (`||`, `lower`, `LIKE`, a cast to `text`), so
`'ab'::char(4) = 'ab'` is true; `length` ignores them and `octet_length` counts them.

`json` checks its text with EggDB's own parser (nesting deeper than 256 levels fails with
`54001`) and keeps it as written; `jsonb` stores the normalized text. `->` (a key or an array
index, negative from the end), `->>` (the same as text), `#>` and `#>>` (a path such as
`'{a,0}'`), `@>` and `<@` (containment, `jsonb` only), `json_typeof`/`jsonb_typeof` and
`json_array_length`/`jsonb_array_length` work as in PostgreSQL. `jsonb` values compare, group
and index by their normalized text with every number written without trailing zeros, so
`'{"a": 1.0}'::jsonb = '{"a": 1}'::jsonb` is true and a unique index holds them once, while each
value keeps the number as written (`1.0`); `json` has no equality, as in PostgreSQL (`42883`),
and cannot be indexed or be a key (`42704`). Stored `json` and `jsonb` are parsed again when a
row is read, so a damaged one is `XX001`.

**Expressions**
- Arithmetic `+ - * / % ^`, unary `-` and `+`, string concatenation `||`.
- Comparisons `= <> != < <= > >=`, `BETWEEN`, `IN (list)`, `LIKE` and `ILIKE` with `%`, `_`
  and `\` escapes, `IS [NOT] NULL`, `ISNULL`, `NOTNULL`, `IS [NOT] TRUE | FALSE | UNKNOWN`,
  `IS [NOT] DISTINCT FROM`, each with its `NOT` form.
- `AND`, `OR`, `NOT` with SQL's three-valued logic: `NULL AND false` is false, `NULL OR true`
  is true, anything else with NULL is NULL, and `WHERE` keeps only rows where the condition is
  true.
- `CAST(expr AS type)`, `expr::type` and `type 'literal'`.
- `CASE [operand] WHEN ... THEN ... [ELSE ...] END`.
- Functions `coalesce`, `nullif`, `abs`, `length` (`char_length`, `character_length`),
  `octet_length`, `lower`, `upper` and `substring(s FROM start FOR count)` (or with commas).
- Operator precedence is PostgreSQL's: `-2 ^ 2` is 4, `NOT a = b` is `NOT (a = b)`, and
  `a < b < c` is a syntax error.

**Names** follow PostgreSQL: unquoted names fold to lower case, `"Quoted"` names keep their case,
names are cut to 63 bytes, and `public.name` is the same table as `name`.

**Errors** carry PostgreSQL's SQLSTATE codes and messages, with the position they refer to:
`42601` syntax errors (`syntax error at or near "x"`), `42P01` unknown tables, `42703` unknown
columns, `42702` an ambiguous column, `42712` a table named twice in `FROM`, `42803` an ungrouped
column or an aggregate where none is allowed (`WHERE`, `ON`, `GROUP BY`, `UPDATE`, `VALUES`,
`RETURNING`, `DEFAULT`, `LIMIT`), `42809` `sum(*)` and `count()`, `42P07` a table or index name that exists, `42704` an index that does not, `23505` a duplicate primary key or unique value (with
`Key (id)=(1) already exists.`, and `could not create unique index` when `CREATE UNIQUE INDEX` finds one), `23502` a NULL in a `NOT NULL` column, `22P02` text that is
not a number, `42804` and `42883` type mismatches, `25P02` statements in a failed block, `0A000`
what is not supported yet. An expression nested, or a chain of operators or casts, deeper than 64 levels fails with
`54001` instead of exhausting the stack; `AND` and `OR` chains of any length are fine.

**Transactions.** Outside a block every statement is its own transaction: it commits whole or
not at all, so a multi-row `INSERT` that hits a duplicate key leaves no row behind. `BEGIN`
takes the database's one write transaction until `COMMIT` or `ROLLBACK`, so another writer
waits for it (and gives up with `55P03` after the write timeout). A statement that only reads
runs on a snapshot and never waits for the writer; `eggdb sql` itself still opens the file for
writing, even for a script of `SELECT`s, so another program that opens the same file for
writing is refused (exit code 5) while it runs.
`BEGIN READ ONLY` reads one snapshot and refuses changes with `25006`. A failed statement inside
a block rolls the block back at once and the session answers `25P02` until `ROLLBACK`; `COMMIT`
of a failed block reports `ROLLBACK`, as PostgreSQL does. DDL is transactional: a rolled-back
block that created a table leaves no table.

## Indexes

A secondary index keeps one entry per row: the indexed columns' values in the same
order-preserving encoding as primary keys, then the row's primary key (or hidden row number),
so equal values sort in primary key order (D109). Entries are written in the same transaction
as their rows by every `INSERT` (with `IGNORE`, `REPLACE` and upserts), `UPDATE`, `DELETE` and
`TRUNCATE`, so a crash or a rollback never leaves an index that disagrees with its table.
`CREATE INDEX` on a table that has rows fills the index in the same transaction, a thousand rows
at a time within the statement's 1 GiB work memory; `DROP INDEX` and `DROP TABLE` remove the
entries.

**Uniqueness.** A unique index (`UNIQUE` on a column or a table, `CREATE UNIQUE INDEX`, MySQL's
`UNIQUE KEY`) refuses two rows with the same values, and allows any number of rows with a NULL
in an indexed column, as PostgreSQL, MySQL and SQLite do. It is checked across the whole
statement: a multi-row `INSERT` that repeats a value fails whole, and an `UPDATE` checks after
every row moved, so swapping two unique values in one `UPDATE` works. `CREATE UNIQUE INDEX` on
a table that already holds a value twice fails and leaves nothing behind.

**When an index is used.** Without statistics, the planner follows fixed rules: for each of the
primary key and the indexes it takes the longest leading run of columns compared with
constants by `=`, `IN (list)` or `IS NULL`, then a range on the next column (`<`, `<=`, `>`,
`>=`, `BETWEEN`, or `LIKE 'prefix%'` where `LIKE` is case-sensitive), and picks, in order, a
unique key fully matched by `=`, the longest equality run, a range, then the primary key over
an index. `ORDER BY` columns that follow the matched columns in a key, all ascending or all
descending, read the key in order (backward for `DESC`) and skip the sort; with `LIMIT` the
read stops early, and with no `WHERE` an index is read in order when the query has a `LIMIT`.
`ORDER BY` with `NULLS FIRST` on an ascending column (or `NULLS LAST` on a descending one) still
sorts unless the column is `NOT NULL`. A join looks rows of its inner table up through an index
on its join columns when the outer side is a key lookup or the inner table does not fit the
64 MiB hash table. Every other condition still filters the rows an index finds, so results are
the same as a full scan's: a test runs random queries and changes with and without indexes and
compares them. A constant that converts to the column's type inexactly (`int_column > 2.5`)
and `OR` do not use an index yet.

**Size of an entry.** An index entry's key is 14 bytes, then the indexed values (a number is
its width plus one byte, a text value its UTF-8 length plus three bytes), then the row's primary
key encoded the same way. It must fit the store's largest key, an eighth of a page: 512 bytes
with the default 4 KiB pages ([embedded.md](embedded.md)). A longer entry fails the statement
with `54000` (`a key of 522 bytes is longer than the 512 bytes a key may have with this page
size`), so index long text columns only with a larger page size.

**Checking.** `Session::check_indexes` (the embedded API) reads every table and checks that each
row has exactly its entries, no entry lacks a row, every entry decodes and no unique index holds
a value twice; a disagreement is `XX001` naming the index, repaired by dropping and creating it
again. `eggdb check` checks the file's pages and keys and does not run it.

## Sequences, constraints and ALTER TABLE

**Sequences** (D114) are catalog objects beside the tables, sharing their names:
`CREATE SEQUENCE [IF NOT EXISTS] name [AS smallint | integer | bigint] [INCREMENT [BY] n]
[MINVALUE n | NO MINVALUE] [MAXVALUE n | NO MAXVALUE] [START [WITH] n] [CACHE n] [[NO] CYCLE]
[OWNED BY table.column | NONE]`, `ALTER SEQUENCE [IF EXISTS] name` with the same options and
`RESTART [[WITH] n]`, `OWNER TO` (accepted) or `RENAME TO`, and `DROP SEQUENCE`. The defaults and
checks are PostgreSQL's: `bigint`, increment 1, an ascending sequence from 1 to the type's
maximum, a descending one from -1 down to the type's minimum, `INCREMENT must not be zero`,
`MINVALUE (x) must be less than MAXVALUE (y)`, `START value (x) cannot be less than MINVALUE
(y)`. The functions are PostgreSQL's: `nextval('name')`, `currval('name')` (`55000` before this
session's first `nextval`), `lastval()`, `setval('name', n [, is_called])`, with the name
resolved as PostgreSQL resolves a `regclass` (lower case unless quoted, `public.` accepted) and
`'name'::regclass` printing the name; `pg_catalog.nextval(...)` and the other `pg_catalog.`
spellings work. Past its limit a sequence fails with `2200H` (`nextval: reached maximum value of
sequence "s" (9223372036854775807)`) or, with `CYCLE`, starts again; arithmetic never wraps.
`nextval` is not undone by a rollback, as in PostgreSQL, and is durable with the commit of any
transaction that used the value. Oracle's sequences are the same objects: `CREATE SEQUENCE s
START WITH 1 INCREMENT BY 1 NOCACHE NOCYCLE` (with `NOMAXVALUE`, `NOMINVALUE`, `ORDER` and the
rest accepted), `s.NEXTVAL` and `s.CURRVAL`, with ORA-02289, ORA-08002 and ORA-08004. MySQL and
SQLite have no sequences, and `nextval` there is an unknown function. A column `DEFAULT
nextval('s')` numbers rows, as `pg_dump` writes `serial` columns; EggDB's own `serial` and
identity columns still use the auto-increment column (D111).

**Constraints.** `NOT NULL`, `UNIQUE` and `PRIMARY KEY` are as before. `CHECK (expr)` refuses a
row for which `expr` is false (NULL passes) with `23514 new row for relation "t" violates check
constraint "c"`; it may name only its table's columns and holds no subquery or aggregate.
`FOREIGN KEY (columns) REFERENCES parent [(columns)]` (or `REFERENCES` on a column) needs the
parent's primary key or a unique constraint on exactly those columns (`42830` otherwise) and
types that compare (`42804`). A child row with a NULL in any key column is not checked (`MATCH
SIMPLE`); otherwise its key must be in the parent (`23503 insert or update on table "c" violates
foreign key constraint "f"`, `Key (a)=(5) is not present in table "p".`). Deleting or changing
a parent key runs `ON DELETE` / `ON UPDATE`: `NO ACTION` (the default) and `RESTRICT` refuse
while children remain, `CASCADE` deletes or updates them, `SET NULL` and `SET DEFAULT` change
their keys (a default must itself be in the parent). Cascades go any depth, self-references
included, through a queue charged to the statement's 1 GiB work memory. Foreign keys are
checked when the statement ends, so a multi-row `INSERT` may insert a child before its parent;
`DEFERRABLE` and `INITIALLY DEFERRED` are accepted and behave as `IMMEDIATE`. A child is found
through an index on its key columns when there is one: create one on large children, as in
PostgreSQL. `TRUNCATE` of a referenced table is `0A000`. At most 1,024 checks and 1,024 foreign
keys a table, a foreign key of at most 32 columns. In SQLite foreign keys are enforced only after
`PRAGMA foreign_keys = ON` (off by default, as in SQLite), with `FOREIGN KEY constraint failed`
and `CHECK constraint failed: name` (an unnamed check is named by its text); MySQL answers 1452,
1451 and 3819 with its constraint text, Oracle ORA-02291, ORA-02292 and ORA-02290.

**ALTER TABLE** takes several actions separated by commas, all in one transaction:
- `ADD [COLUMN] [IF NOT EXISTS] column type [constraints]`: existing rows read the new column's
  default, each row its own (`DEFAULT nextval('s')` numbers them); `NOT NULL` without a default
  on a table with rows is `23502`. Adding a primary key, `serial` or identity column this way
  is `0A000`.
- `DROP [COLUMN] [IF EXISTS] column [CASCADE | RESTRICT]`: drops the indexes and checks on the
  column, the foreign keys that use it, and the sequences it owns; another table's foreign key
  that references it is `2BP01` unless `CASCADE`.
- `RENAME [COLUMN] a TO b`, `RENAME TO name`: foreign keys, owned sequences and check
  expressions follow.
- `ALTER [COLUMN] c SET DEFAULT expr | DROP DEFAULT | SET NOT NULL | DROP NOT NULL |
  [SET DATA] TYPE type`: `SET NOT NULL` checks every row; `TYPE` converts every row as an
  assignment would (`42804 column "c" cannot be cast automatically to type t` otherwise;
  `USING` is not supported yet).
- `ADD [CONSTRAINT name] PRIMARY KEY (columns) | UNIQUE (columns) | CHECK (expr) | FOREIGN KEY
  ...`: existing rows are checked first (`could not create unique index "t_pkey"`, `check
  constraint "c" of relation "t" is violated by some row`). `DROP CONSTRAINT [IF EXISTS] name
  [CASCADE | RESTRICT]` drops a check, foreign key, unique constraint or the primary key.
- `OWNER TO role` is accepted and does nothing, as `pg_dump` writes it.

In MySQL a `REFERENCES` clause on a column is parsed and ignored, as MySQL does; write `FOREIGN
KEY (column) REFERENCES ...`. MySQL's spellings: `ADD [COLUMN]`, `DROP [COLUMN]`, `MODIFY [COLUMN] definition` and `CHANGE
[COLUMN] old definition` (the whole definition is replaced, as in MySQL), `ALTER [COLUMN] c SET
DEFAULT | DROP DEFAULT`, `RENAME [TO | AS] name`, `RENAME COLUMN`, `ADD [CONSTRAINT [name]]
PRIMARY KEY | UNIQUE | FOREIGN KEY | CHECK`, `DROP PRIMARY KEY`, `DROP FOREIGN KEY name`, `DROP
CHECK name`, `DROP INDEX name` and `AUTO_INCREMENT = n` (accepted; the next value stays the
highest one plus one). SQLite's limited `ALTER TABLE`: `ADD [COLUMN]`, `RENAME TO`, `RENAME
[COLUMN] a TO b` and `DROP [COLUMN]` only. Oracle's: `ADD (column definitions)` or `ADD
column`, `MODIFY (column [type] [DEFAULT expr] [NOT NULL | NULL], ...)`, `DROP COLUMN c`, `DROP
(columns)`, `RENAME COLUMN a TO b`, `RENAME TO name`, `ADD CONSTRAINT ...`, `DROP CONSTRAINT
name` and `DROP PRIMARY KEY`.

`DROP COLUMN`, `TYPE` and adding or dropping a primary key rebuild the table's rows and indexes a
thousand rows at a time within the statement's transaction; `ADD COLUMN` with a default rewrites
the rows in place; the other actions change only the definition (D114).

## Schemas, views and comments

**Schemas** (D116), in the PostgreSQL dialect: `CREATE SCHEMA [IF NOT EXISTS] name
[AUTHORIZATION role]`, `DROP SCHEMA [IF EXISTS] name [, ...] [CASCADE | RESTRICT]` (`2BP01`
while it holds objects unless `CASCADE`, which drops them, the views that use them in other
schemas and the foreign keys that reference them), `ALTER SCHEMA name RENAME TO new` and `OWNER
TO` (accepted). Tables, views, sequences and indexes live in a schema; write `schema.name`
anywhere a table name goes (`app.t`, `public.t`, `REFERENCES app.t (id)`, `nextval('app.s')`,
`OWNED BY app.t.id`, `DROP INDEX app.i`, `COMMENT ON COLUMN app.t.c`), and move a table with
`ALTER TABLE t SET SCHEMA s`. `public` always exists and cannot be dropped or renamed;
`pg_catalog`, `information_schema` and names starting with `pg_` are reserved (`42939`). Errors:
`3F000 schema "x" does not exist`, `42P06 schema "x" already exists`, and messages show
qualified names as `app.t`.

Unqualified names follow `search_path`, a session setting (default `"$user", public`): `SET
search_path [TO | =] app, public`, `RESET search_path`, `SHOW search_path`, `SELECT
pg_catalog.set_config('search_path', '', false)` (constant arguments only), `current_schema()`
and `current_schemas(bool)` (shown as `{pg_catalog,app,public}`). A name is looked up in each
schema of the path that exists, in order; `pg_catalog` is searched first for functions
(`pg_catalog.nextval`, `pg_catalog.set_config` and the other functions that exist). New objects
are created in the first schema of the path that exists (`3F000 no schema has been selected to
create in` when there is none). The other dialects have one schema: SQLite's `main.t` (`temp.t`
is `0A000`: no temporary tables yet), Oracle's `USER.t` for the session's user (another schema
is ORA-00942), MySQL's `db.t` for the connected database.

**Views**, in every dialect: `CREATE [OR REPLACE] VIEW name [(columns)] AS query`, `DROP VIEW
[IF EXISTS] name [, ...] [CASCADE | RESTRICT]`, and in PostgreSQL `ALTER VIEW [IF EXISTS] name
RENAME TO new | SET SCHEMA s | OWNER TO role`. A view may use joins, groups, subqueries, `WITH`,
set operations and other views; it is read-only (`INSERT`, `UPDATE`, `DELETE` and `TRUNCATE` on
it are `0A000`). Its query is checked when it is created and expanded where it is used, as a
derived table. `SELECT *` is expanded when the view is created, so columns added to the table
later do not appear. `OR REPLACE` keeps the existing columns' names and types and may add
columns at the end (`42P16` otherwise); a view that would refer to itself, directly or through
other views, is `42P17`. Views nest to 32 query levels and 1,024 `SELECT`s for a statement and
everything it expands (`54001`); a view's query text is at most 1 MiB, and a statement expands at
most 64 MiB of view text in all (`54000`). A view written with `SELECT *` keeps the columns it
had: PostgreSQL's by position (its tables cannot be dropped or changed under it), MySQL's and
Oracle's by name, so a table dropped and created again with its columns in another order still
answers correctly (a missing column is MySQL's 1356 or ORA-04063), and SQLite's expands `*`
again against the table as it is now. A `BEGIN` block that rolls back restores `search_path`, as
PostgreSQL restores the settings a rolled-back transaction changed. In PostgreSQL, dropping a table or view that views use is
`2BP01` unless `CASCADE` (which drops those views), changing or dropping a column a view uses is
`2BP01`, and renaming a table or view that a view uses is `0A000`. MySQL's `CREATE [OR REPLACE]
[ALGORITHM = ...] [DEFINER = ...] [SQL SECURITY ...] VIEW` (the clauses are accepted and
ignored), `SHOW CREATE VIEW` and `SHOW FULL TABLES` (`VIEW`); SQLite's `CREATE VIEW [IF NOT
EXISTS] [main.]name`; Oracle's `CREATE [OR REPLACE] [[NO] FORCE] VIEW ... [WITH READ ONLY]` and
`USER_VIEWS`/`ALL_VIEWS`. In MySQL, SQLite and Oracle a table dropped or changed under a view
leaves the view, which fails when used, as those databases do. Views have no privileges yet
(EggDB has no roles).

**Comments**: `COMMENT ON TABLE | VIEW | SEQUENCE | INDEX | SCHEMA name IS 'text'`, `COMMENT ON
COLUMN table.column IS 'text'`, `COMMENT ON CONSTRAINT name ON table IS 'text'` and `COMMENT ON
EXTENSION` (accepted) in PostgreSQL; `IS NULL` removes a comment. Oracle has `COMMENT ON TABLE`
and `COMMENT ON COLUMN`, listed by `USER_TAB_COMMENTS` and `USER_COL_COMMENTS`; MySQL keeps the
`COMMENT 'text'` column option and `COMMENT = 'text'` table option and shows them in `SHOW
CREATE TABLE` and `SHOW FULL COLUMNS`. The shell's `.schema` prints them as `COMMENT ON`
statements. A comment goes with its object when the object is dropped or renamed.
`obj_description` and `col_description` come with item 37.

**What `pg_dump` writes.** Besides the above, `GRANT`, `REVOKE` and `ALTER DEFAULT PRIVILEGES`
are accepted with a warning and ignored (EggDB has no roles yet), `CREATE EXTENSION [IF NOT
EXISTS] plpgsql [WITH SCHEMA ...]` does nothing (another extension is `0A000`), the `SET`
parameters of its scripts are accepted, and view text with parenthesized joins (`FROM ((a JOIN
b ON ...) JOIN c ON ...)`) runs. `CREATE MATERIALIZED VIEW`, `CREATE TYPE`, `CREATE DOMAIN` and
`WITH CHECK OPTION` are `0A000` with a message naming them.

## COPY

`COPY` loads a table from the client and unloads a table or a query to it, in PostgreSQL's
text and CSV formats (D117, PostgreSQL dialect only; the other dialects refuse it with their own
syntax error):

```sql
COPY table [(column, ...)] FROM STDIN [[WITH] (option, ...)]
COPY {table [(column, ...)] | (query)} TO STDOUT [[WITH] (option, ...)]
```

The older forms that `pg_dump` and psql write work too (`WITH CSV HEADER`, `DELIMITER [AS]
'x'`, `NULL [AS] 'x'`, `QUOTE`, `ESCAPE`, `FORCE QUOTE`, `FORCE NOT NULL`, `BINARY`), and
`STDIN` and `STDOUT` may be written either way round, as in PostgreSQL. Options:

| Option | Meaning |
|--------|---------|
| `FORMAT text \| csv` | text (the default) or CSV; `binary` is `0A000` for now |
| `DELIMITER 'c'` | one one-byte character between columns: tab in text, comma in CSV |
| `NULL 's'` | the text for NULL: `\N` in text, an unquoted empty field in CSV |
| `DEFAULT 's'` | `COPY FROM` only: the text that stands for the column's default |
| `HEADER [true \| false \| match]` | a first line of column names: written by `TO`, skipped by `FROM`, or with `match` checked against the columns (`22P04` on a mismatch) |
| `QUOTE 'c'`, `ESCAPE 'c'` | CSV only: the quote (`"`) and the character that escapes it inside quotes (the quote itself) |
| `FORCE_QUOTE (cols) \| *` | CSV `TO` only: quote these columns' non-NULL values |
| `FORCE_NOT_NULL (cols) \| *`, `FORCE_NULL (cols) \| *` | CSV `FROM` only: an unquoted null string is an empty string, or a quoted one is NULL |
| `ENCODING 'UTF8'` | UTF-8 only; another encoding is `0A000` |
| `FREEZE`, `ON_ERROR stop` | accepted; `ON_ERROR ignore` is `0A000` |

The text format is the manual's: one row per line ending in `\n` (`\r\n` and `\r` are read
too, but one file uses one), columns separated by the delimiter, `\N` for NULL (matched before
backslashes are removed, so `\\N` is the text `\N`), and the escapes `\b \f \n \r \t \v`,
`\` and one to three octal digits, `\x` and one or two hex digits, and a backslash before any
other character for that character (so `\\`, and the delimiter, and a newline). A line of just
`\.` ends the data. `COPY TO` writes the same escapes. CSV follows the manual's rules: fields
with the delimiter, the quote, a carriage return or a newline are quoted, a quote inside is
doubled, an unquoted empty field is NULL and a quoted one an empty string, quoted fields may
span lines, and a value `\.` alone on a line is quoted on output. Every value goes through its
type's text input and output, the same as a quoted constant and a result in psql: numeric,
dates and time stamps (`timestamptz` in the session's `TimeZone`), `bytea` (`\x` hex),
`uuid`, `json` and `jsonb`, intervals.

A `COPY FROM` is one statement: its rows go through the path of `INSERT ... VALUES`, so
constraints (`NOT NULL`, `CHECK`, `UNIQUE`, primary and foreign keys), defaults, `serial` and
identity columns, sequences and indexes behave as they do there, and any error rolls back every
row of it (in a `BEGIN` block, the block fails as after any failed statement). Errors carry
PostgreSQL's SQLSTATEs (`22P04` for data that breaks the format, `22P02`, `22003`, `22021` for
bytes that are not UTF-8, `23505`, `23502`, `23503`, `23514`, ...) and a context line naming
where: `COPY t, line 3, column b: "x"` (psql and `eggdb sql` print it as `CONTEXT:`). Rows are
read and stored one at a time, so a load holds one line of the data and one piece of input (a
protocol message, or 64 KiB of a file) in memory at a time, plus the rows of a foreign key to
its own table until the end; a line is at most 1 GiB (`54000`).
The whole load is one transaction, so its pages must fit in the page cache: `eggdb serve
--cache 1g` for large loads (`eggdb sql` and `eggdb import --csv` size the cache from the data
they are given). `COPY TO` of a table streams its rows in primary-key order; `COPY (query) TO`
runs the query first, with the memory bound of any query.

`COPY` never touches files or programs on the server: `COPY t FROM 'file'` and `... PROGRAM
'cmd'` are refused with `42501`, as PostgreSQL refuses them to a role that is not allowed to
read server files. Copy client-side instead: psql's `\copy`, `eggdb shell`'s `\copy`, or
`eggdb import --csv` ([cli.md](cli.md)). Where the data comes from:
- **psql and drivers** send it over the protocol ([Connecting with psql](#connecting-with-psql)).
- **A script** run by `eggdb sql file.sql`, `eggdb sql < file.sql` or `psql -f` carries it after
  the statement, from the next line up to a line `\.`, as `pg_dump` writes its data sections;
  statements after the `;` on the `COPY`'s own line run after the data, as in psql.
- **`eggdb sql -c "COPY ... FROM STDIN"`** reads it from standard input; `COPY ... TO STDOUT`
  prints the rows without a table or a tag.
- **The embedded API**: `Session::execute_with_copy` and `Session::execute_script` take a
  `CopyChannel` that supplies and receives the data.

## Restoring a pg_dump

`eggdb restore` restores a PostgreSQL dump in plain SQL form, and `psql -f dump.sql` against
`eggdb serve` runs the same statements (D118); [migrate.md](migrate.md) is the guide. What a dump
of supported objects writes runs as it is: the `SET` preamble (parameters EggDB does not have are
ignored by `eggdb restore`, and are errors over the protocol, which psql reports and passes),
`set_config('search_path', '', false)` with schema-qualified names (`nextval('public.s')` and
`setval('public.s', ...)` find the sequence whatever the search path), `CREATE SCHEMA`, tables
with `CHECK` constraints written as `= ANY (ARRAY[...])`, sequences and `OWNED BY`, identity
columns added with `ALTER COLUMN ... ADD GENERATED`, `DEFAULT nextval(...)`, `COPY ... FROM
stdin` data, `--inserts` and `--column-inserts` (with `OVERRIDING SYSTEM VALUE`), `setval`,
`ALTER TABLE ONLY ... ADD CONSTRAINT`, `CREATE INDEX`, views, `COMMENT ON`, `OWNER TO`,
`GRANT` and `CREATE EXTENSION plpgsql`. Functions, triggers, types, domains, materialized views,
partitioned tables and other extensions are not created yet: `eggdb restore` lists them in its
report, and over the protocol they fail with an error that psql prints before going on.

## How SQL is stored

SQL lives in the database's one ordered key space, under keys that begin with the five bytes
`0xff 'S' 'Q' 'L' 0x00` (D98). Below that prefix:

| Key | Value |
|-----|-------|
| prefix, `0x01` | the catalog's format version and the next table number |
| prefix, `0x02`, table name | the table's definition: its number, columns, types, `NOT NULL`, defaults and primary key |
| prefix, `0x03`, table number (4 bytes, big-endian), primary key | the row |
| prefix, `0x04`, table number, index number (4 bytes each, big-endian), indexed values, primary key | empty: one entry of a secondary index |
| prefix, `0x05`, sequence name | the sequence: its type, start, increment, bounds, cache, last value, whether it was called, cycle and owner (D114) |
| prefix, `0x06`, parent table name (length-prefixed), child table name | empty: the child has a foreign key to the parent (D114) |
| prefix, `0x07`, schema name | the schema: a version byte (D116) |
| prefix, `0x08`, relation name (length-prefixed), view name | `0x01`: the view uses the relation (D116) |
| prefix, `0x09`, owner name (length-prefixed), kind byte, part name | a comment: a version byte and its text (D116) |

A table definition is written in format version 1, or version 2 when it has an
`AUTO_INCREMENT` column, and in version 3 only when it has secondary indexes: version 3 adds
after the primary key a count of indexes and, for each, its number, name, whether it is unique
and its column numbers (D109). Tables without indexes keep their bytes, so older releases
still read them; an older release refuses a version 3 table by name rather than misreading it.
A table with a column of one of the types item 19d added is written in version 4 (D111): the
version 3 layout, with the count of indexes always present (it may be 0) and the type tags 9
(`numeric`, then its precision and scale as two 16-bit little-endian numbers, 0 and 0 without
them), 10 (`date`), 11 (`timestamp`) and 12 (`timestamptz`, each then one byte of fractional
digits, `0xff` without), 13 (`json`) and 14 (`jsonb`). Every other table keeps its version and
bytes; an older release refuses a version 4 table by name. A table with a column of one of the
types item 31 added is written in version 5 (D113): the version 4 layout with the type tags 15
(`time`) and 16 (`timetz`, each then one byte of fractional digits), 17 (`interval`, then its
fields as one byte, 0 without, and its fractional digits), 18 (`bytea`), 19 (`uuid`) and 20
(`character`, then its length as a 32-bit little-endian number, 0 without). Tables without them
keep their version and bytes; an older release refuses a version 5 table by name. A table with
`CHECK` or `FOREIGN KEY` constraints, or a primary key named other than `table_pkey`, is written
in version 6 (D114): the version 5 layout followed by the primary key's name (a flag byte, then
the name), the checks (count; each its name, the dialect it was written in as one byte and its
expression text) and the foreign keys (count; each its name, its column numbers, the parent's
name, the parent's column names, the `ON DELETE` and `ON UPDATE` actions as one byte each and a
`DEFERRABLE` flag). A table that another table's foreign key references, or
that owns a sequence (`OWNED BY`), is written in version 6 too, with a flags byte after the
primary key's name saying so, so that an older release refuses it by name instead of deleting a
referenced row or dropping the table without its sequence. Other tables keep their version and
bytes; an older release refuses a version 6 table by name. A table in a schema other than
`public` is stored under its schema's name, a zero byte and its name, and written in version 7
(D116): the version 6 layout with the constraints part always present; `public` tables keep
their names, versions and bytes, and an older release refuses a version 7 table by name. A view
is stored under its name in the same key space as version 8: its dialect, a flag for `*` in its
select list, its query text, its columns with their types, the schemas of the search path when
it was created and the relations it names; an older release refuses it by name.

Primary keys are encoded so that byte order is value order (D98): each column is a marker byte
followed by the value; integers are big-endian at their width with the sign bit flipped, floating
point is IEEE 754 with the sign handled so negatives sort first (`-0` stored as `0`, every NaN as
one NaN above infinity), booleans are one byte, and text is UTF-8 with `0x00` written as
`0x00 0xff` and ended by `0x00 0x01`. Text compares byte by byte, as PostgreSQL's `C`
collation does; other collations come with D30. A `date` is its day number and a time stamp
its microseconds, big-endian with the sign bit flipped; a `jsonb` is keyed as text. A `time`
is its microseconds the same way, a `timetz` its time in UTC and then its offset west of UTC,
an `interval` its length in microseconds (a month as 30 days) as 16 bytes, so equal intervals
have one key, a `uuid` its 16 bytes, and `bytea` and `character` (without its trailing spaces)
are keyed as text is. A
`numeric` is a class byte (`0x01` negative, `0x02` zero, `0x03` positive, `0x04` NaN) and,
unless zero or NaN, its decimal exponent (16 bits) and its significant digits one byte each
ended by a terminator, every byte inverted for a negative number, so `1.5` and `1.50` have one
key and a unique index holds them once (D111). A table without a primary key uses an 8-byte
row number instead.

A row is a format version byte, the column count, and per column a type byte (0 for NULL) and
the value: a `numeric` as a flags byte, its scale (16 bits), a limb count and its base-10^9
limbs; a `date` as 4 bytes, a time stamp or a `time` as 8, a `timetz` as 8 and 4, an
`interval` as its months, days (4 bytes each) and microseconds (8), a `uuid` as 16 bytes, `json`,
`jsonb`, `bytea` and `character` length-prefixed. A value outside its
type's range or limits is damage. A row shorter than its table reads its missing trailing columns as NULL, which leaves
room for `ALTER TABLE ... ADD COLUMN` without rewriting rows. Every definition and row is read
strictly: a damaged one is an error (`XX001`, exit code 4), never a panic; a text column that a
join does not name is skipped by its length without checking its UTF-8. `eggdb check` checks
the file's pages and keys, not SQL rows and definitions, so it can pass on a file whose SQL
data is damaged; restore such a table from a backup or an export.

Because SQL data is keys and values, everything the engine does covers it: atomic commits,
crash recovery, snapshots, `eggdb check`, `export` and `import`. The key-value commands show
the SQL keys as they are; writing under the prefix by hand changes what SQL reads. `import`
copies SQL keys like any others, so importing an export that holds SQL tables into a file that
already has SQL tables can give two tables the same number or overwrite the catalog: import SQL
exports into a new file, or into a file without SQL tables.

## In the shell and the UI
`eggdb shell app.db` is an interactive prompt for SQL and the key-value commands, with history,
tab completion over tables, columns and keywords, and `BEGIN` blocks that span prompts; `eggdb
ui app.db` has a query screen (`s`, run with `F5`). Both print results and errors exactly as
`eggdb sql` does ([cli.md](cli.md)).

## Known differences from PostgreSQL
Schemas and views (D116): a view is computed in full each time a statement uses it (no
condition is pushed into it and its tables' indexes are not used through it), so a point lookup
through a view over a large table reads the whole view; views are read-only (no automatically updatable views, `WITH CHECK
OPTION` is `0A000`), there are no materialized, temporary or recursive views, renaming a table
or view that a view uses is `0A000` (PostgreSQL follows the rename), a view resolves its names
with the search path it was created with when it is used, where PostgreSQL binds them once, a
column `DEFAULT nextval('s')` finds an unqualified `s` through the search path when it runs, and
messages name objects outside `public` as `schema.name`. There are no roles, so `GRANT`,
`REVOKE` and owners are accepted and ignored; `current_schemas` returns text, not `name[]`.

`COPY` (D117): the binary format and `ON_ERROR ignore` are `0A000`; `ENCODING` takes only
UTF-8; `FREEZE` is accepted and changes nothing (PostgreSQL refuses it unless the table was
created or truncated in the same transaction); `COPY ... WHERE` is `0A000`; files and programs
on the server are always `42501`; a `\.` inside a text-format field reads as `.` where
PostgreSQL may refuse it; `COPY TO` of a table writes rows in primary-key order (PostgreSQL in
storage order); and some option errors carry other SQLSTATEs or wording than PostgreSQL's.

The correctness suite (`crates/eggdb-sql/correctness/postgresql.test`, [correctness.md](correctness.md))
checks the PostgreSQL manual's documented behaviour; these cases are marked there as differing
until they are fixed:
- `VALUES` as a query (`WITH RECURSIVE t(n) AS (VALUES (1) UNION ALL ...)`) answers `0A000`;
  write `SELECT 1`.
- The string functions `substr`, `trim`, `ltrim`, `rtrim`, `btrim`, `replace`, `concat`,
  `position(... IN ...)`, `left`, `right`, `repeat` and `reverse` are missing (`42883`, or
  `42601` for the SQL-standard syntax and for `left`/`right`, which parse as join keywords;
  roadmap: the rest of PostgreSQL's string functions, below).

- One statement at a time is planned by simple rules without statistics ([Indexes](#indexes)):
  a `WHERE` that fixes every primary-key column with `=` reads one row (in a join, of the first
  table), conditions on a leading part of the primary key or an index read that range, and
  anything else reads the whole table. Each statement's result
  is built in memory before it prints (a `SELECT` without `ORDER BY` holds only the rows it
  returns; `ORDER BY` holds every matching row and sorts them, with no top-N for `LIMIT` yet,
  unless a key gives the order).
- Indexes: partial indexes (`WHERE`), indexes on expressions, `INCLUDE`, `CONCURRENTLY`,
  `NULLS [NOT] DISTINCT` and index methods other than B-tree answer `0A000`; `USING hash` makes
  an ordinary index. `DESC` and `NULLS FIRST` in an index are accepted and the index is kept in
  ascending order, which serves `ORDER BY ... DESC` by reading it backward. An index name is
  checked against tables and other indexes when the index is made, not when a table is made.
  `DROP INDEX` also drops the index of a `UNIQUE` constraint, which PostgreSQL refuses
  (`2BP01`). `EXPLAIN` prints the plan's nodes without costs, conditions or `Filter` lines, and
  `EXPLAIN ANALYZE` and `EXPLAIN (options)` answer `0A000`.
- Joins read the first table row by row and every later table once, except a table read
  through an index on its join columns, which is read once per row. A table whose whole
  primary key the `ON` or `USING` equalities fix is read by key for each row when the first
  table is read by its key, and otherwise held in a hash table on that key while it fits in
  64 MiB (else read by key); one joined by other equalities of plain columns of the same kind of
  type (integers, floats, text, booleans) is held in a hash table; anything else is held in
  memory and tried against each row. Only the columns a statement names are decoded. So a join
  of a small filtered table with a very large one by its key reads up to 64 MiB of the large one
  before falling back to lookups; table statistics come later. Joins go
  left to right as written, with no reordering, and each `AND` part of `WHERE` is applied as soon as
  the tables it names are joined. `GROUP BY` holds one entry per group, `DISTINCT` one
  per distinct row or value. What a statement holds this way (joined tables, groups, distinct
  rows, sorted rows and its result) is counted, and past 1 GiB the statement fails with `53200`
  (`out of memory`) instead of exhausting the process; nothing spills to disk yet.
  A `FROM` clause joins at most 64 tables (`54000`).
- `numeric` holds at most 1,000 digits before the point and 1,000 after (PostgreSQL: 131,072
  and 16,383), input with more fails with `22003`; `Infinity` is not a `numeric` yet (`22P02`;
  PostgreSQL 14 added it). `^` with a non-integer exponent, or an integer one whose exact result
  would need more than 2,000 digits, is computed in double precision (15 significant digits).
- `date` and time stamps take ISO 8601 and PostgreSQL's ISO style only: `infinity`, `epoch`,
  `now`, `today`, month names (`Feb 29 2024`) and `MDY`/`DMY` forms (`02/29/2024`) answer
  `22007`, and so do `allballs`, `noon` and the other special `time` inputs. Named time zones
  act as UTC (above). `now()` is the statement's start time, where PostgreSQL's is the
  transaction's. `to_char`, `to_date`, `to_timestamp`, `make_date`, `make_interval`,
  `date_trunc` of an interval and `AT TIME ZONE` are missing (`42883` or a syntax error).
- `interval` (D113): only the `postgres` `IntervalStyle` is printed; PostgreSQL 17's
  `infinity` intervals are not supported, so `interval * 'Infinity'` and a NaN factor fail with
  `22008`; text input longer than 4,096 bytes fails with `22007`. `DISTINCT`, `min` and `max` of
  intervals that are equal but written differently (`'1 mon'`, `'30 days'`) return the first one
  read, which PostgreSQL leaves unspecified too. `time` and `timetz` cannot be compared with each
  other (`42883`), and `timetz` has no time zone functions yet.
- Values of different types that one expression returns row by row (`CASE` branches of
  `numeric` and `double precision`) are grouped and made distinct by their value where a `numeric` is exactly a
  `double precision` (`2.5`); a `numeric` such as `0.1` and the double nearest it compare equal
  but group apart. PostgreSQL gives such an expression one type before it runs.
- `jsonb`'s `?`, `?|`, `?&`, `-`, `||` and `#-` operators, the JSON builder, aggregate, set and
  path (`jsonb_path_query`) functions and SQL/JSON (`JSON_TABLE`) are missing (item 36). `jsonb`
  sorts by its normalized text, not by PostgreSQL's jsonb ordering, and a `jsonb` value more than
  16 times larger than its text once its numbers are written out (`1e999`) fails with `54000`.
- `serial` and identity columns follow the table's auto-increment column: a NULL or `0` given
  to it also takes the next number, `GENERATED ALWAYS` accepts a given value, the number of a
  deleted last row is given again, and the column must be the first of the primary key; a
  sequence with `DEFAULT nextval(...)` (as `pg_dump` writes `serial`) has none of these limits.
- Constraints are checked when each statement ends: `INITIALLY DEFERRED` waits for nothing, and
  `RESTRICT` behaves as `NO ACTION`, so a statement that changes a referenced key and puts it
  back, such as the key swap `UPDATE p SET id = 3 - id` over ids 1 and 2, passes where
  PostgreSQL's immediate `RESTRICT` refuses it. `NOT VALID`, `RENAME CONSTRAINT`, `ALTER COLUMN ... TYPE ... USING` and
  `ADD COLUMN` of a `serial` or identity column answer `0A000`.
- A sequence's `CACHE` is remembered, not used: every value is saved. A value handed out by a
  transaction that rolled back can be handed out once more if another connection's writer starts
  between the rollback and the re-saving of the sequence (D114); a committed value never is.
  `serial` and identity columns have no visible sequence (`pg_get_serial_sequence` waits for
  item 37); `nextval` takes a name, not an OID.
- `\d`, `SHOW CREATE TABLE`, `sqlite_schema` and `.schema` do not show `CHECK` and `FOREIGN KEY`
  constraints yet.
- An older EggDB release refuses a table that has constraints, is referenced by a foreign key or
  owns a sequence (format version 6), but it does not know sequences: on a file this release
  wrote, it ignores them, so it could create a table under a sequence's name or call a dropped
  table's sequence a stranger's. Open such a file with this release or a newer one.
- Subqueries and `WITH` (item 30, D112): a correlated subquery may name only the query
  directly around it (a name two or more levels out is `0A000`), and stand only in the select
  list, `WHERE` or `ORDER BY`, and in a query with `GROUP BY` or aggregates only in `WHERE`
  (elsewhere `0A000`); an aggregate inside a subquery whose argument names only the outer
  query's columns (`SELECT (SELECT sum(t.b) FROM u) FROM t`), which belongs to the outer query
  in PostgreSQL and SQLite, answers `0A000`; a correlated subquery runs once per distinct outer
  value, so one over a million distinct values runs a million times, with no statement timeout
  yet (item 19e); a derived table
  or `WITH` query cannot name the outer query's columns, and `UPDATE`, `DELETE` and `INSERT`
  cannot have a correlated subquery (`0A000`). Subqueries nest at most 32 levels and a statement
  holds at most 1,024 `SELECT`s (`54001`). Every `WITH` query runs, even one nothing names, and
  `MATERIALIZED` and `NOT MATERIALIZED` change nothing; data-modifying `WITH` queries, `SEARCH`
  and `CYCLE`, `ANY`/`ALL` over anything but a constant `ARRAY[...]` (such as `'{1,2}'::int[]`,
  an array column or a list), row constructors (`(a, b) IN (SELECT ...)`),
  `LATERAL`, `FULL JOIN ... USING` and `EXPLAIN` of a query with subqueries answer `0A000`. A
  derived table, a `WITH` query and the operands of a set operation are held in memory, counted
  with the statement's other rows against the 1 GiB limit, and a correlated subquery keeps up
  to 32 MiB of results by outer value. An `IN` list of constants is compared item by item (a list of
  more than 256 items is not used to choose a key range either), so a very long list against
  many rows is slow; `IN (SELECT ...)` looks values up in a hash set.
- `NATURAL` joins, `DISTINCT ON`,
  `GROUPING SETS`, `ROLLUP` and `CUBE`, window functions, `FILTER`, ordered aggregates and the
  other aggregates (`string_agg`, `bool_and`, `array_agg`, ...) fail with `0A000`.
  When the output stops being read (`eggdb sql ... | head`), the command stops before the next
  statement.
- `UPDATE` and `DELETE` collect the rows they change before changing them, in memory.
- A whole transaction must fit in the page cache (`--cache`), as for every write (D86).
- Type checks happen as rows are evaluated, so a type mismatch in a `WHERE` on an empty table
  is not reported (PostgreSQL reports it when planning).
- `DEFAULT` expressions are checked for column references when the table is created and
  evaluated at each `INSERT`, so a default that fails (`DEFAULT 1/0`) fails the `INSERT`, not
  the `CREATE TABLE`.
- `LIKE` and `ILIKE` match with backtracking, which takes time proportional to the text's length
  times the pattern's in the worst case (as PostgreSQL's does).
- Some syntax PostgreSQL accepts still answers with a syntax or type error instead of `0A000`:
  adjacent string literals on separate lines, `U&'...'` strings, octal and Unicode escapes in
  `E'...'` strings, `CASE` branches of different types; `lower` and `upper` change case by
  Unicode rules, where PostgreSQL's `C` collation changes only ASCII letters.
- Speed: a point `SELECT` by primary key takes about 1.2 times SQLite's time on an
  Apple-silicon Mac (about 0.91 on CI's Linux runners, D100), and multi-row `INSERT`s of constants about 0.9
  times (0.83 on Linux), measured with the same SQL text on both and gated so they can only improve; the rest of
  the point-select gap is roadmap item 19e (D99). A `GROUP BY` with four aggregates over 10,000
  rows takes about 0.94 times SQLite's time on the Mac and 0.86 on Linux, and a 10,000-row join
  by primary key about 0.99 times on the Mac and 1.4 on Linux, gated the same way; the Linux join
  must reach 1.0 before v0.3.0 (item 19e, D100).

## Known differences from MySQL
A view is read-only, `SHOW CREATE VIEW` shows the query as it was written (MySQL rewrites it),
a view that used `SELECT *` keeps the columns it had when it was created (as MySQL's do), and
`CREATE DATABASE`, `DROP DATABASE` and `USE` of another database are not supported yet: a
server serves one database.

The MySQL suite (`crates/eggdb-sql/correctness/mysql.test`, 223 cases from the MySQL 8.0
Reference Manual's documented behaviour) marks these as differing until they are fixed:
- `7 / 2` is `3.5` (a `double`) where MySQL gives the `DECIMAL` `3.5000`, and decimal constants
  outside a `DECIMAL` column are doubles (`0.1 + 0.2` is `0.30000000000000004`); stored into a
  `DECIMAL` column they are exact.
- `1 / 0` is error 1365 where MySQL's `SELECT` gives `NULL` with a warning.
- Text compares byte by byte (`'a' = 'A'` is false) where `utf8mb4_0900_ai_ci` ignores case and
  accents; collations come with D30.
- `TIME` holds PostgreSQL's range, `00:00:00` to `24:00:00`: MySQL's negative times and times
  past a day (up to `838:59:59`, D113) fail with an error.

And more, not in the suite yet:
- Comparisons and `IS` tests are booleans, shown as `1` and `0`, but arithmetic on them (`TRUE +
  1`) and `IF(1, ...)` with an integer condition are type errors where MySQL treats booleans as
  integers. Strings are not converted to numbers implicitly (`'5' + 1` with a typed string).
- A statement that fails after it wrote rows inside a transaction rolls the whole transaction
  back, where MySQL rolls back only the statement (no statement savepoints yet).
- `AUTO_INCREMENT` continues from the largest value in the table, so the value of a deleted last
  row is given again, which MySQL 8 does not do; `AUTO_INCREMENT=n` table options are ignored.
- `INSERT IGNORE` skips duplicate keys only; other errors still fail the statement.
- `CHAR(n)` is `varchar(n)`: trailing spaces a value is given are kept, where MySQL removes them
  when the value is read (D113).
- `BINARY(n)` is not padded with zero bytes and `VARBINARY(n)` and the `BLOB` sizes do not limit
  a value's length. A string given to a binary column is stored as its bytes, as in MySQL, and
  `CONCAT` with a binary argument gives binary bytes; `UPPER`, `LOWER`, `SUBSTRING`, `LIKE` and
  comparisons with a text column answer an error on a binary value instead of treating it as a
  binary string. `eggdb sql` shows a blob as MySQL's client does with `--binary-as-hex`
  (`0x616263`); the wire carries its bytes (D113).
- `UPDATE` reports rows matched, as with `CLIENT_FOUND_ROWS`, not rows changed.
- `BIGINT UNSIGNED` is refused; `UNSIGNED` columns accept negative values.
- `SET` remembers session variables without applying them (`sql_mode`, `time_zone`,
  `transaction_isolation`: every transaction is serializable); `SET GLOBAL` is refused.
- `DESCRIBE` shows the engine's types (`smallint` for `TINYINT`).
- Dates and times: `DATE` and `DATETIME` take years 4714 BC to 5874897 (MySQL: 1000 to 9999),
  zero dates (`0000-00-00`) are refused, a `DATETIME(fsp)` prints without trailing zeros in its
  fraction, `TIME`, `YEAR`, `DATE_FORMAT`, `STR_TO_DATE`, `DATE_ADD`, `UNIX_TIMESTAMP` and
  `ON UPDATE CURRENT_TIMESTAMP` are not supported yet, and `time_zone` is not applied (the
  session's time zone is UTC unless the PostgreSQL protocol's `TimeZone` sets it).
- A value rounded to fit a `DECIMAL(p,s)`'s scale gives no note 1265.
- `EXTRACT` gives integers and accepts no `WEEK` mode but 0; `EXTRACT` of a negative compound
  value and of years before 1000 are not checked against MySQL.
- `AVG` of integers and `SUM`/`AVG` of `DECIMAL` use PostgreSQL's result scale (`AVG(2, 3)` is
  `2.5000000000000000`) where MySQL adds 4 digits to the argument's scale (`2.5000`).
- Indexes: a key on a column prefix (`KEY k (name(10))`) indexes the whole value, and a
  `UNIQUE` one answers 1235; `FULLTEXT` and `SPATIAL` keys answer 1235. `SHOW INDEX` gives
  `Cardinality` and `Sub_part` as `NULL`, and `EXPLAIN` gives `key_len`, `ref` and `rows` as
  `NULL`. `ALTER TABLE ... ADD INDEX` answers 1235 (use `CREATE INDEX`), and `FIRST`/`AFTER`
  in `ALTER TABLE` too. Foreign keys are checked when the statement ends, not row by row, and
  creating one makes no index on the child's columns (InnoDB makes one).

## Known differences from SQLite
Views are read-only (SQLite's are too, without `INSTEAD OF` triggers), `CREATE TEMP VIEW` and
the `temp` schema are `0A000`, and `sqlite_schema` does not list views yet.

The SQLite suite (`crates/eggdb-sql/correctness/sqlite.test`, 234 cases from sqlite.org's
documentation of SQLite 3.46, never SQLite's source) marks these as differing until they are
fixed:
- Indexes: `EXPLAIN` without `QUERY PLAN` (the bytecode listing), partial indexes and indexes
  on expressions answer an error; `ON CONFLICT (columns) DO ...` applies to a conflict on any
  unique key, not only the one named; `sqlite_schema.sql` of an index is regenerated from the
  catalog.
- A column has one type: the affinity's. SQLite's columns may hold any storage class, so a
  `NUMERIC` column given `7` keeps the integer `7` where EggDB keeps the real `7.0`, and an
  `INTEGER` column given `'abc'` keeps the text where EggDB answers `datatype mismatch`.
- A column declared `BLOB` is refused (`0A000`) where SQLite stores blobs: EggDB's `bytea` type
  would refuse the other storage classes a SQLite column accepts (D113).
- Dates and times: `date()`, `time()`, `datetime()`, `julianday()`, `strftime()`,
  `unixepoch()` and `CURRENT_TIMESTAMP` are not supported yet; `DATE` and `DATETIME` columns
  keep their text, as SQLite's do.

And more, not in the suite yet:
- Columns without a declared type are text, so `typeof()` says `text` for numbers stored
  there; `BLOB` columns and `x'...'` literals wait for columns that hold any storage class. A real given to an
  `INTEGER` column is rounded where SQLite keeps it as a real, and `CAST('abc' AS INTEGER)` is
  an error where SQLite gives 0.
- Names fold to lower case: result columns and `PRAGMA table_info` show `id` for a column
  declared `Id`. `PRAGMA table_info` and `sqlite_schema.sql` show the affinity's type name
  (`INTEGER`, `TEXT`, `REAL`) and a `CREATE TABLE` regenerated from the catalog, not the text
  as written.
- An `INTEGER PRIMARY KEY` given `0` takes the next row id where SQLite stores 0, and
  `AUTOINCREMENT` reuses the row id of a deleted last row (SQLite's never does).
  `last_insert_rowid()` is not changed by inserts into tables whose key is not an `INTEGER
  PRIMARY KEY`.
- `changes()` counts 2 for a row that `REPLACE` or an upsert replaced, as MySQL does, where
  SQLite counts 1.
- A statement that fails after it wrote rows inside a transaction rolls the whole transaction
  back, where SQLite rolls back the statement (no statement savepoints yet); `INSERT OR FAIL`
  behaves as `ABORT`, and the `OR` forms cover the primary key only (there are no `UNIQUE`
  constraints yet).
- `PRAGMA user_version`, `foreign_keys`, `journal_mode` and `synchronous` are per session, not
  kept in the file; foreign keys are never enforced.
- Not supported yet: `CREATE INDEX`, `UNIQUE` and `CHECK` constraints, `COLLATE NOCASE`,
  generated columns, views, triggers, `ATTACH`, `VACUUM`, `UPDATE OR ...`, `VALUES` as a query,
  window functions, `group_concat` and `total`, the JSON and math functions, `GLOB` character
  classes and bare columns in aggregate queries.

## Known differences from Oracle
The Oracle suite (`crates/eggdb-sql/correctness/oracle.test`, 254 cases from Oracle's SQL
Language Reference, never Oracle's software) marks these as differing until they are fixed:
- `EXPLAIN PLAN FOR` and `DBMS_XPLAN`, bitmap, function-based and reverse-key indexes, and the
  primary key's index in `USER_INDEXES` (EggDB keeps the primary key as the table's own order).
- `ROWNUM` in the select list, `ORDER BY` or anywhere but `WHERE` conditions joined by `AND`,
  and `FETCH ... PERCENT` and `WITH TIES`.
- `LISTAGG` and `MERGE`.
- An identity column that is not the first column of the primary key.

And more, not in the suite yet:
- Sequences hold 64-bit values: `NOMAXVALUE` is 9223372036854775807 (Oracle's is 10^28 - 1) and
  a larger `MAXVALUE` counts as `NOMAXVALUE`. Two `NEXTVAL` references in one row give two values
  (Oracle gives the row one), and the column of `s.NEXTVAL` is headed `S.NEXTVAL`.
- `NUMBER(p)` with `p` up to 18 is enforced by its storage type's range, not by `p` (no
  `ORA-01438` for 1000 in a `NUMBER(3)`); decimal constants, `/` and the numeric functions
  compute in double precision (`1/3` has 15 significant digits where Oracle has 38); `NUMBER`
  holds up to 1,000 digits (Oracle: 38 significant digits).
- `EXTRACT(HOUR ...)` is `ORA-30076` for a `TIMESTAMP(0)` column too, which EggDB stores like a
  `DATE`; `TIMEZONE_REGION` and `TIMEZONE_ABBR` are `ORA-00907`.
- `DATE` and `TIMESTAMP` show as `YYYY-MM-DD HH24:MI:SS[.FF]` whatever `NLS_DATE_FORMAT` says;
  `TO_DATE`, `TO_TIMESTAMP`, `TO_CHAR` of a date, `ADD_MONTHS`, `MONTHS_BETWEEN`, `LAST_DAY`,
  `NEXT_DAY` and date arithmetic with numbers are not supported yet.
- `VARCHAR2(n BYTE)` counts characters, and `ORA-12899` gives the maximum but not the column's
  name or the actual length. A `CHAR` compared with a `VARCHAR2` value ignores the `CHAR`'s
  trailing blanks (PostgreSQL's rule) where Oracle pads the shorter value only between two
  `CHAR`s. A string given to a `RAW` or `BLOB` is read as hexadecimal digits, as `HEXTORAW`
  does (an odd count gets a leading zero, another character is `ORA-01465`); the functions
  `HEXTORAW`, `RAWTOHEX`, `UTL_RAW` and `DBMS_LOB` are missing.
  `INTERVAL YEAR TO MONTH` and
  `INTERVAL DAY TO SECOND` answer `0A000` (D113).
- A statement that fails after it wrote rows rolls back the whole transaction, where Oracle
  rolls back only the statement (no statement savepoints yet); a failing single-row `INSERT`,
  `UPDATE` or `DELETE` that wrote nothing leaves the transaction as it was, as in Oracle.
  `SAVEPOINT` is not supported yet.
- `GENERATED ALWAYS` accepts a given value, and an identity column reuses the number of a
  deleted last row (Oracle's identity is a sequence that never goes back).
- Result columns of expressions are named after the expression in upper case with spaces
  removed outside quotes, close to SQL*Plus's headings; `SELECT *` from `USER_TABLES` shows the
  three columns above, not Oracle's fifty.
- Not supported yet: views, `CONNECT BY`, outer joins with `(+)`, `ALTER TABLE`, `CHECK` and foreign keys, PL/SQL (`BEGIN ...
  END`, procedures, packages), `SAVEPOINT`, `INSERT ALL`, `RETURNING ... INTO`, synonyms,
  database links, `ROWID`, the regular-expression, analytic and XML functions, `NLS_`
  settings, and schemas other than the session user's.

## Known differences from SQL Server
The SQL Server suite (`crates/eggdb-sql/correctness/sqlserver.test`, from Microsoft's
Transact-SQL reference, never SQL Server's software) and D122 list these:
- Names and aliases come back in lower case; a mixed-case table made in PostgreSQL with quotes
  (`"Users"`) cannot be named from Transact-SQL.
- Comparisons are binary (the collation reported is Latin1_General_100_BIN2): `'a' = 'A'` is
  false, and trailing spaces count.
- `+` with a string operand is not supported yet (40517), and nor are the types `bit`,
  `tinyint`, `money`, `datetime`, `datetimeoffset`, `uniqueidentifier`, `varbinary` and `time`
  (item 39), `alias = expression`, `ISNULL`, `CONVERT`, `OFFSET ... FETCH`, `TOP ... PERCENT`
  and `WITH TIES`, `OUTPUT`, `SELECT ... INTO` and `#temporary` tables.
- An `IDENTITY` seed or increment other than (1,1), and an identity column that is not the
  first column of the primary key; the next number is one more than the largest in the table,
  so a deleted largest value is used again.
- A statement that fails after it wrote rows rolls back the whole transaction (no statement
  savepoints yet) and ends the batch; one that wrote nothing leaves the transaction open, as in
  SQL Server. An unknown column of an existing table is found when the statement runs, after
  the statements before it in the batch ran (SQL Server finds it when the batch compiles).
- Views (`CREATE VIEW` and `ALTER VIEW`) and `CHECK` constraints: each keeps its dialect in the
  file, and a fifth dialect there is a file-format change of its own (D114, D116, D35); views
  and constraints made in another dialect work from Transact-SQL. A `DEFAULT` is a constant or
  `GETDATE()` and its kin.
- `DECLARE`, `IF`, `WHILE`, `BEGIN ... END`, `TRY ... CATCH`, `PRINT`, `EXEC`, stored procedures
  and triggers; `SAVE TRANSACTION`; `XACT_ABORT`, `QUOTED_IDENTIFIER OFF`, `ANSI_PADDING OFF`
  and `IMPLICIT_TRANSACTIONS ON`; three-part names and other databases.

## Next parts (roadmap)
- The rest of the JSON functions (item 36).
- Point `SELECT`s as fast as SQLite's everywhere: an AST that borrows the SQL text and a plan
  cache; table statistics for choosing between key lookups and hash joins.
- `LATERAL`, `FULL JOIN ... USING`, correlated subqueries in grouped queries, and the other
  aggregates.
- The rest of PostgreSQL's string functions (`substr`, `trim`, `replace`, `concat`, `position`,
  `left`, `right`, ...).
- Then the other dialects (item 23), each with its own correctness suite; for the PostgreSQL protocol,
  TLS, the `pg_catalog` queries of psql's `\d` and GUI tools, binary `COPY` and query
  cancellation (M6a).
