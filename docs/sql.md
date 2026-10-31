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
  `version()`, `current_user`, `current_database()`, `current_setting()` and the `pg_catalog`
relations psql's `\d` family reads.
- **`COPY`** (D117): `COPY ... FROM STDIN` takes `CopyData` messages in pieces of any size,
  ending with `CopyDone`; `CopyFail` fails it with `57014`, and `Flush` and `Sync` are ignored
  until it ends, as libpq expects. `COPY ... TO STDOUT` sends one `CopyData` per row, then
  `CopyDone`. Both work from the simple and the extended protocol, so psql's `\copy` and
  drivers' copy calls work; see [COPY](#copy).
- **Several connections**, one thread and one session each, up to `--max-connections` (64),
  at most 16 of them still logging in; readers run side by side, writers one at a time (a
  second waits 5 s, then `55P03`: retry once the other session's transaction ends).
- **Implicit transactions, as PostgreSQL runs them** (D135): outside `BEGIN`, the statements of
  one `Query` message, and of an extended-protocol pipeline up to its `Sync`, commit together or
  not at all: `INSERT ...; INSERT ...; SELECT 1/0` leaves nothing, and so does a batch of
  prepared statements whose second one fails. A `BEGIN` among them opens a block that includes
  the statements before it; a `COMMIT` or `ROLLBACK` among them ends what came before with the
  warning "there is no transaction in progress". Portals end with their transaction: after a
  `Sync` outside a block they are gone (`34000`), inside a block they live to its end. A
  pipeline that only reads never takes the database's one writer.
- **Cancelling a running statement** (D135): each connection gets a random secret key in its
  `BackendKeyData`; a `CancelRequest` with that process ID and key (psql's Ctrl-C,
  `PQcancel`, pgjdbc's `Statement.cancel()`, Npgsql's `NpgsqlCommand.Cancel()`) stops the
  statement the connection is running at its next row with `57014` "canceling statement due to
  user request", and rolls back what it wrote. A request with a wrong key changes nothing and is
  logged. `pg_sleep(seconds)` waits and can be cancelled the same way.
- **`DISCARD ALL`**, which connection pools send before reusing a connection, forgets the
  connection's prepared statements and portals and returns every setting to the value the client
  sent when it connected (`RESET` and `RESET ALL` do the same); `DISCARD PLANS`, `TEMP` and
  `SEQUENCES` are accepted too. Inside a block `DISCARD ALL` is `25001`.

psql's `\d` family (`\d`, `\dt`, `\di`, `\dv`, `\ds`, `\dn`, `\l`, `\du`) reads EggDB's
catalog: see [The PostgreSQL catalog](#the-postgresql-catalog).

Not yet (each fails readably or is ignored harmlessly): TLS, `\d+`, `\df` and the other psql
commands beyond the `\d` family, the catalog queries of GUI tools beyond it, `LISTEN`/`NOTIFY`, stored
users and roles, and non-ASCII passwords.

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
  `TIME(fsp)` is `time(fsp)` (with `TIME '...'` literals; `fsp` defaults to 0) holding MySQL's
  range, `-838:59:59` to `838:59:59`, read in MySQL's forms (`'100:00:00'`, `'-01:30'`, `'2
  03:04:05'` with days, `'123456'`) and printed as MySQL prints it (D153), and `BLOB`,
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
  SQLite application writes it). `DECIMAL(p,s)` is a real, by the same rule. A column declared
  `BLOB` holds every storage class as it is given, row by row (an integer, a real, text or a
  blob, D153): `typeof()` tells which, `quote()` writes a blob as `X'0A0B'`, `length()` counts a
  blob's bytes, and values of different classes compare and sort as SQLite orders them (numbers,
  then text, then blobs). Blob literals are `x'0A0B'` (an even number of hexadecimal digits;
  anything else is `unrecognized token`).
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
  `total_changes`, `unicode`, `char`, `sign`, and the aggregates `count`, `sum`, `avg`, `min`
  and `max`.
- **Dates and times** as SQLite's documented date and time functions: `date`, `time`,
  `datetime`, `julianday`, `unixepoch`, `strftime` (every documented `%` field) and
  `timediff`, on time values in the documented text formats (with a `Z` or `±HH:MM` zone),
  `'now'` (the statement's time) and numbers, with every modifier: `±N days|hours|minutes|
  seconds|months|years`, `±HH:MM[:SS.SSS]`, `±YYYY-MM-DD[ HH:MM:SS]`, `start of
  day|month|year`, `weekday N`, `unixepoch`, `julianday`, `auto`, `localtime` and `utc` (in
  the session's time zone), `subsec`, `ceiling` and `floor`. An invalid value or modifier
  gives `NULL`, as in SQLite.
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
protocol exists (D10). Oracle's SQL runs here, through the CLI and the embedded API, and through
PostgreSQL and TDS drivers on a connection that chooses the dialect
([Oracle SQL over PostgreSQL and TDS connections](#oracle-sql-over-postgresql-and-tds-connections),
D134).

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
  `OFFSET n ROWS` and `FETCH FIRST|NEXT n [PERCENT] ROW|ROWS ONLY | WITH TIES` (negative
  counts as 0; a percent of the rows the query finds, rounded up; `WITH TIES` adds the rows
  that sort equal to the last one, D149), `ROWNUM` in
  `WHERE` with Oracle's meaning (`ROWNUM <= n` keeps the first n rows found, before `GROUP BY`
  and `ORDER BY`; `ROWNUM = 1`; `ROWNUM > 1` and `ROWNUM = 2` find nothing), `CASE`, joins,
  `GROUP BY` and `HAVING`, `LIKE` (case-sensitive), `!=`, `^=` and `~=` for `<>`, `q'[...]'`
  and `N'...'` literals, `:name` and `:1` bind variables, and `/` that divides exactly where
  it can (`7/2` is `3.5`).
- **Types**: `VARCHAR2(n [BYTE|CHAR])`, `NVARCHAR2(n)` and `VARCHAR(n)` as `varchar(n)`
  (the length counts characters), `NUMBER(p)` and `NUMBER(p,0)` as `smallint` (p up to 4), `integer` (up to 9) or `bigint` (up
  to 18), `NUMBER(p,s)` with a scale and `NUMBER(19..38)` as exact `numeric(p,s)` (a value
  that does not fit is `ORA-01438`), `NUMBER` without precision as `numeric`, `FLOAT`, `REAL`,
  `DOUBLE PRECISION` and `BINARY_DOUBLE` as `double precision`, `BINARY_FLOAT` as `real`,
  `INTEGER`/`INT`/`SMALLINT` (Oracle's `NUMBER(38)`) as `bigint`, `CLOB`, `NCLOB` and `LONG` as
  `text`, `BOOLEAN` (23ai), `DATE` as `timestamp(0)` (a date with a time of day, to the second,
  as Oracle's), `TIMESTAMP(p)` as `timestamp(p)` (`p` defaults to 6; 7 to 9 keep 6),
  `TIMESTAMP(p) WITH [LOCAL] TIME ZONE` as `timestamptz(p)`, `INTERVAL YEAR [(p)] TO MONTH` and
  `INTERVAL DAY [(p)] TO SECOND [(s)]` as `interval year to month` and `interval day to
  second(s)` (D149), and `JSON` as `jsonb`. `DATE
  '...'` and `TIMESTAMP '...'` literals (strict ISO 8601, never the NLS format; a `TIMESTAMP`
  literal with an offset such as `-8:00` is a time zone value), `SYSDATE` and `CURRENT_DATE`
  (the session's local time to the second), `SYSTIMESTAMP`, `CURRENT_TIMESTAMP` and
  `LOCALTIMESTAMP` work; dates show as `YYYY-MM-DD HH24:MI:SS` in result sets, and as the NLS
  format in `TO_CHAR`, `||` and character columns. `EXTRACT(field FROM d)` takes `YEAR`, `MONTH`, `DAY`, `HOUR`,
  `MINUTE`, `SECOND`, `TIMEZONE_HOUR` and `TIMEZONE_MINUTE`; `HOUR`, `MINUTE` and `SECOND` of a
  `DATE` and a zone field of a time stamp without a zone are `ORA-30076`, another field
  `ORA-00907`; of an interval it takes the fields of its kind (`YEAR` and `MONTH`, or `DAY` to
  `SECOND`, another `ORA-30076`). `CHAR(n)` and `NCHAR(n)` are blank-padded `character(n)` (compared without
  their trailing blanks; `LENGTH` counts them, as Oracle's does), and `RAW(n)`, `LONG RAW` and
  `BLOB` are `bytea` (D113).
- **Exact numbers** (D126): decimal constants, `/`, `ROUND`, `TRUNC`, `CEIL`, `FLOOR`, `MOD`,
  `ABS`, `SIGN`, `GREATEST`, `LEAST`, `TO_CHAR` and `TO_NUMBER` compute in decimal, so `0.1 +
  0.2` is `.3`, `0.3 / 0.1` is `3` and `ROUND(1.005, 2)` is `1.01`. Values the `NUMBER` page's
  "Storage of Scale and Precision" table lists are stored as it lists them.
- **Datetime format models** (D126, D149): `TO_DATE(char [, fmt [, nlsparam]])`,
  `TO_TIMESTAMP`, `TO_TIMESTAMP_TZ` and `TO_CHAR(datetime [, fmt [, nlsparam]])` with `DD`,
  `DDD`, `D`, `DAY`, `DY`, `J`, `MM`, `MON`, `MONTH`, `RM`, `Y` to `YYYY`, `SYYYY`, `Y,YYY`,
  `RR`, `RRRR`, `AD`/`BC`/`A.D.`/`B.C.`, `HH`, `HH12`, `HH24`, `MI`, `SS`, `SSSSS`, `FF`,
  `FF1`-`FF9`, `AM`/`PM`/`A.M.`/`P.M.`, `TZH`, `TZM`, `TZR`, `TZD`, `X`, punctuation, `"quoted
  text"`, `FM`, `FX` and the short forms `DS` (`MM/DD/RRRR`), `DL` (`fmDay, Month dd, yyyy`)
  and `TS` (`HH:MI:SS AM`), alone or together; `TO_CHAR` also writes `CC`, `SCC`, `YEAR`,
  `SYEAR`, `IYYY` to `I`, `IW`, `WW`, `W`, `Q` and the suffixes `TH`, `SP`, `SPTH`/`THSP`
  (`DDTH` is `01ST`, `Ddspth` is `First`), which a reading model refuses with `ORA-01820`. A
  reading model checks its elements as Oracle does: a field twice (`ORA-01810`, `01812` year,
  `01813` hour, `01814`, `01815`, `01816` month, `01817` day of week), `J` with `DDD`
  (`ORA-01811`), `HH24` with a meridian (`ORA-01818`), `SYYYY` with BC (`ORA-01819`); and the
  value: a Julian day against the year, month and day given (`ORA-01831`, `01833`, `01834`),
  `SSSSS` against the hour, minute and second (`ORA-01836` to `01838`), a day name against the
  date (`ORA-01835`). The String-to-Date Conversion Rules (blanks ignored, punctuation left out
  or swapped, trailing time and zone fields left out, `MM`→`MON`→`MONTH`, `YY`→`YYYY`,
  `RR`→`RRRR`) and `FX`'s exact matching; `RR`'s century rule; years 4713 BC to 9999 AD. Zones
  read as `TZH:TZM`, a `TZR` offset (`-08:00`), `UTC`/`GMT` or a region of the time zone
  database (`US/Pacific`, with its daylight saving time, D153; an unknown one is `ORA-01882`); a
  value without one takes the session's time zone. The `nlsparam` accepts
  `NLS_DATE_LANGUAGE = American`.
- **Number format models** (D126, D149): `TO_CHAR(n, fmt)` and `TO_NUMBER(char, fmt
  [, nlsparam])` with `9`, `0`, `.`, `D`, `,`, `G`, `$`, `L`, `C`, `U`, `B`, `S` (leading or
  trailing), `MI`, `PR`, `V`, `EEEE`, `RN`/`rn`, `X`/`x` (with leading `0`s), `TM`, `TM9`, `TME`
  and `FM`, as the Format Models page's "Results of Number Conversions" table shows them
  (`TO_CHAR(123.456, '9.9EEEE')` is `  1.2E+02`, `FM999.009` keeps the zeros of its `0`s);
  `TO_NUMBER` reads `$`, `L`, `C`, `U`, `PR`, trailing `S`, `EEEE`, `RN` and `X`, and refuses
  `V` with `ORA-01481`. The currencies are America's: `$` local and dual, `USD` ISO.
- **Session settings** (D126): `ALTER SESSION SET` `NLS_DATE_FORMAT`, `NLS_TIMESTAMP_FORMAT`,
  `NLS_TIMESTAMP_TZ_FORMAT`, `NLS_LANGUAGE`/`NLS_DATE_LANGUAGE` (American),
  `NLS_TERRITORY` (America, which resets the formats it implies), `NLS_NUMERIC_CHARACTERS='.,'`,
  `NLS_SORT=BINARY`, `NLS_COMP=BINARY`, `TIME_ZONE` (an offset such as `'-08:00'`, `'UTC'`, a
  region such as `'Europe/Paris'`, `LOCAL` or `DBTIMEZONE`, both `+00:00`, as the session's
  `TimeZone`; `FROM_TZ` and `TZ_OFFSET` take regions too; `SESSIONTIMEZONE` and
  `DBTIMEZONE` show them, D149) and `CURRENT_SCHEMA` (as `search_path`; the Oracle
  dialect has no `CREATE SCHEMA` or `CREATE USER`, so it reaches schemas made in another dialect
  or by a restore, and `EGGDB.T`, the session user's own table, keeps naming it; `HR.T` names
  schema `hr`, D127). Defaults are `DD-MON-RR`, `DD-MON-RR HH.MI.SSXFF AM` and
  `DD-MON-RR HH.MI.SSXFF AM TZR`.
- **DDL as Oracle's tools write it** (D127): physical properties after `CREATE TABLE`, `CREATE
  INDEX` and `USING INDEX` (`SEGMENT CREATION`, `PCTFREE`, `PCTUSED`, `INITRANS`, `MAXTRANS`,
  `STORAGE (...)`, `TABLESPACE`, `[NO]LOGGING`, `[NO]COMPRESS`, `[NO]CACHE`, `[NO]PARALLEL`,
  `ROW MOVEMENT`, `INMEMORY`, `ORGANIZATION HEAP | INDEX` with `PCTTHRESHOLD`, `INCLUDING` and
  `OVERFLOW`, `LOB (...) STORE AS`, `COMPUTE STATISTICS`, `REVERSE`, `VISIBLE`, `ONLINE`),
  constraint states (`ENABLE`, `VALIDATE`, `NOVALIDATE`, `RELY`, `NORELY`, `USING INDEX`),
  `DEFAULT COLLATION`, `BEQUEATH` and `COLLATE USING_NLS_COMP | BINARY | USING_NLS_SORT`, and
  `SCALE`, `EXTEND` and `NOSHARD` on sequences are accepted with no effect; `PARTITION BY ...`
  makes one unpartitioned table. `NOT NULL DISABLE` leaves the column nullable; other disabled
  constraints are `ORA-03001`. `ALTER TABLE t ADD (column, CONSTRAINT ..., ...)` applies each
  element in order, and `MODIFY (c CONSTRAINT n NOT NULL ENABLE)` works. A primary key named
  like an index of its table on the same columns takes that index over (the separate index is
  gone, and dropping the key later drops it), a unique constraint named like such a unique index
  is that index, and a non-unique index with the name is `ORA-00955`. `ADD CONSTRAINT ... PRIMARY
  KEY | UNIQUE (...) USING INDEX [schema.]name` names the index to use (D150): one on exactly the
  constraint's columns is taken over the same way, one with more columns stays beside the
  constraint's own, a missing one is `ORA-01418` and one whose leading columns are not the
  constraint's is `ORA-14196`. `CREATE INDEX "HR"."X" ON
  "HR"."T"` works; an index in another schema than its table's is `ORA-03001`. `ORGANIZATION
  EXTERNAL`, clusters, nested-table storage and `SHARD` are `ORA-03001`. `COPY` loads and
  unloads tables in the Oracle dialect too, an empty field loading as NULL. SQL*Plus commands
  (`SET`, `DEFINE`, `@`, `/`) are read by `eggdb restore --dialect oracle`, not by `eggdb sql`.
- **Implicit conversions** (D126): text compared with or assigned to a `DATE` or `TIMESTAMP`,
  and `CAST(text AS DATE)`, is read by the NLS format, then as strict ISO 8601; text meets an
  interval as `TO_YMINTERVAL` or `TO_DSINTERVAL` reads it (`'2-6'`, `'1 00:00:00'`); text meets a
  number as `TO_NUMBER` reads it (`'20.5'` into `NUMBER(4)` stores 21, `'5.5' + 1` is 6.5);
  numbers and dates assigned to character columns are written as Oracle writes them (`.5`,
  `17-JUN-03`).
- **Date arithmetic** (D126, D149): `date + n`, `date - n` and `n + date` add days, cutting the
  date to the second and rounding to the second (`d + 1/24` is an hour later); `date - date` is
  the difference in days, and a difference with a `TIMESTAMP` on either side is an `INTERVAL DAY
  TO SECOND`. `datetime ± interval` moves by the interval, and a month that lacks the day is
  `ORA-01839` (`DATE '2024-01-31' + INTERVAL '1' MONTH`), as Oracle's documentation says;
  `interval ± interval` of one kind (two kinds are `ORA-30081`), `interval * n` and `interval
  / n`. `ADD_MONTHS`, `LAST_DAY`, `MONTHS_BETWEEN` (whole months when the days agree or both are
  last days, otherwise with 31-day months), `NEXT_DAY(d, day)` (a full or abbreviated American
  day name, `ORA-01846` otherwise), and `ROUND(date [, fmt])` and `TRUNC(date [, fmt])` with
  every unit of the "ROUND and TRUNC Date Functions" table (`CC`, `YYYY`, `IYYY`, `Q`, `MM`,
  `WW`, `IW`, `W`, `DD`, `DAY`, `HH`, `MI` and their synonyms; a year rounds up on July 1, a
  month on the 16th, a day at noon). `FROM_TZ`, `SYS_EXTRACT_UTC` and `TZ_OFFSET`.
- **Intervals** (D149): literals `INTERVAL '123-2' YEAR(3) TO MONTH`, `INTERVAL '4 5:12:10.222'
  DAY TO SECOND(3)` and every field range of the Interval Literals section, with the leading
  precision checked (`ORA-01873`, default 2) and the other fields' ranges (`ORA-01843`,
  `01850` to `01852`); `NUMTODSINTERVAL`, `NUMTOYMINTERVAL` (`ORA-01760` for another unit),
  `TO_DSINTERVAL` and `TO_YMINTERVAL` with Oracle's and ISO 8601's forms (`'P1DT5H'`); intervals
  print as `+01-02` and `+04 05:12:10.222`.
- **MERGE** (D126): `MERGE INTO t [alias] USING {table | (subquery)} [alias] ON (condition)`
  with `WHEN MATCHED THEN UPDATE SET ... [WHERE ...] [DELETE WHERE ...]` and `WHEN NOT MATCHED
  THEN INSERT [(columns)] VALUES (...) [WHERE ...]`, in either order; `ORA-30926` when a target
  row matches more than one source row, `ORA-38104` for an updated `ON` column, `ORA-00918`
  for an ambiguous name; the result is `MERGE n`.
- **Indexes**: `CREATE [UNIQUE] INDEX name ON t (...)` with `TABLESPACE`, `ONLINE`, `LOGGING`
  and `COMPUTE STATISTICS` accepted, `DROP INDEX name`, `UNIQUE` columns and `CONSTRAINT name
  UNIQUE (...)` (an unnamed one is `SYS_C` and seven digits), `USER_INDEXES`/`ALL_INDEXES`
  (`INDEX_NAME`, `INDEX_TYPE`, `TABLE_NAME`, `UNIQUENESS`) and
  `USER_IND_COLUMNS`/`ALL_IND_COLUMNS`; `ORA-00001` for a duplicate, `ORA-01452` when `CREATE
  UNIQUE INDEX` finds one, `ORA-00955` for a name in use and `ORA-01418` for a missing index.
- **Identity columns**: `GENERATED [ALWAYS | BY DEFAULT [ON NULL [FOR INSERT ONLY | FOR INSERT
  AND UPDATE]]] AS IDENTITY [(options)]` on the first column of the primary key (D106, D150).
  `ALWAYS` is kept and enforced: a value given in `INSERT` is `ORA-32795` and an `UPDATE` of the
  column `ORA-32796` (`DEFAULT` is allowed); `eggdb restore` and `COPY` keep the values they
  bring. Options, in parentheses or bare as SQL Developer writes them: `START WITH` (default
  `MINVALUE`, or `MAXVALUE` when descending) and `INCREMENT BY` set the first value and the step;
  `CACHE`, `ORDER`, `KEEP`, `SCALE`, their `NO` forms, `NOCYCLE` and a `MINVALUE`/`MAXVALUE` that
  does not end the identity inside its type's range have no effect. `INCREMENT BY 0` is
  `ORA-04002`, `MINVALUE` not below `MAXVALUE` `ORA-04004`, a start outside them
  `ORA-04006`/`ORA-04008`; a limit inside the type's range, `CYCLE` and `START WITH LIMIT VALUE`
  are `ORA-03001`. New rows take the next value after the furthest one stored (D147).
  `USER_TAB_IDENTITY_COLS` (with `ALL_` and `DBA_`) lists them with `GENERATION_TYPE`,
  `SEQUENCE_NAME` (`ISEQ$$_` and the table's id) and `IDENTITY_OPTIONS`.
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
  fit) and for dates, `TO_NUMBER`, `TO_DATE`, `TO_TIMESTAMP`, `ADD_MONTHS`, `LAST_DAY`, the aggregates `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, and `USER`. Other
  dialects' functions are not there (`CHAR_LENGTH` is `ORA-00904`), and Oracle's are not in the
  other dialects. `LISTAGG([ALL | DISTINCT] expr [, delimiter] [ON OVERFLOW ERROR | TRUNCATE
  ['indicator'] [WITH | WITHOUT COUNT]]) [WITHIN GROUP (ORDER BY ...)]` joins a group's values
  (NULLs left out, NULL for none); past 32,767 bytes it is `ORA-01489`, or truncated at the last
  whole value with the indicator (`...`) and the count of values left out (D149).
- **Length limit**: `LPAD`, `RPAD`, `REPLACE`, `||`, `UPPER`, `LOWER`, `INITCAP` and `TO_CHAR` refuse a result longer than
  1,000,000,000 bytes with `ORA-01489` before they build it.
- **The data dictionary** (D134): `USER_`, `ALL_` and `DBA_` `TABLES`, `TAB_COLUMNS`,
  `TAB_COLS`, `CONSTRAINTS`, `CONS_COLUMNS`, `INDEXES`, `IND_COLUMNS`, `SEQUENCES`, `VIEWS`,
  `OBJECTS`, `TAB_COMMENTS`, `COL_COMMENTS`, `USERS`, `SOURCE`, `PROCEDURES`, `ARGUMENTS`,
  `TRIGGERS` and `ERRORS`, the synonyms `TABS`, `COLS`, `IND`,
  `SEQ` and `OBJ`, `V$VERSION`, `NLS_SESSION_PARAMETERS` and `NLS_DATABASE_PARAMETERS`, with
  Oracle's column names and the rest of `SELECT` on them: joins (`USER_CONSTRAINTS` with
  `USER_CONS_COLUMNS`), grouping, subqueries. They are built from the catalog once per
  statement, as PostgreSQL's catalog is (D129), and are read-only. `SYS.USER_TABLES` and the
  session user's own prefix name them too; a table of the user's own named like one
  (`TABS`) comes first, as a private object hides Oracle's public synonym. The owner of a
  schema's objects is its name in upper case; the session user's own schema is the schema named
  after it when one exists (`HR` for the user `hr` after HR's installer restored), otherwise the
  default schema, shown with the session user's name (`EGGDB` by default) and as `PUBLIC` to a
  user with a schema of its own. Unqualified names and `HR.EMPLOYEES` resolve in that schema.
- **`SYS_CONTEXT('USERENV', parameter)`** (D134): `CURRENT_USER`, `SESSION_USER`,
  `CURRENT_SCHEMA`, `DB_NAME`, `DB_UNIQUE_NAME`, `CON_NAME`, `INSTANCE_NAME`, `SERVICE_NAME`,
  `SID`, `SESSIONID`, `ISDBA`, `LANGUAGE`, `NLS_DATE_FORMAT`, `NLS_TERRITORY` and the other
  documented parameters; NULL for what EggDB does not know (`HOST`, `OS_USER`, `MODULE`, ...)
  and for another namespace; `ORA-02003` for an undocumented parameter.
- **Errors** with Oracle's numbers and wording: `ORA-00942: table or view does not exist`,
  `ORA-00904: "X": invalid identifier`, `ORA-00001: unique constraint (EGGDB.EMP_PKEY)
  violated`, `ORA-01400: cannot insert NULL into ("EGGDB"."EMP"."ENAME")`, `ORA-01407`,
  `ORA-00955`, `ORA-00923`, `ORA-00933`, `ORA-00936`, `ORA-00900`, `ORA-01722: invalid
  number`, `ORA-01476: divisor is equal to zero`, `ORA-12899`, `ORA-01426`, `ORA-01428`,
  `ORA-00938`, `ORA-00939`, `ORA-00906`, `ORA-01741`, `ORA-01756`, `ORA-00972`, the
  datetime format errors (`ORA-01801`, `01810`, `01820`, `01821`, `01830`, `01839`, `01840`,
  `01841`, `01843`, `01847`, `01849`, `01850`, `01851`, `01852`, `01855`, `01858`, `01861`,
  `01862` and, for item 42b, `01811` to `01819`, `01831` to `01838`, `01846`, `01848`, `01853`,
  `01854`, `01856`, `01857`, `01867`, `01873`, `01874`, `01875`, `01882`), `ORA-01481`,
  `ORA-01760`, `ORA-30076`, `ORA-30081`, `ORA-02248`, `ORA-01435`, `ORA-12702`, `ORA-12705`, `ORA-30926`, `ORA-38104`, and
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
  before sending; the server reads `GO` as SQL Server does, as an ordinary word (`SELECT 1 GO`
  names its column `go`; `GO` alone is a syntax error). Scripts with `GO` for `eggdb restore`
  come with item 40.
- `SELECT TOP n` and `TOP (expression)`; `N'...'` strings; `--` and nested `/* */` comments;
  the operators `+ - * / % = <> != < > <= >= !< !>`.
- **Types** (item 39, D123), written bare or bracketed in any case (`[decimal](18, 2)`, `AS
  DateTime2`), each kept as one of the engine's types, so PostgreSQL SQL reads the same columns:

  | SQL Server | Kept as | Notes |
  | --- | --- | --- |
  | `int`, `bigint`, `smallint`, `tinyint` | `integer`, `bigint`, `smallint`, `smallint` | `CAST` to `tinyint` checks 0-255 (error 220) |
  | `bit` | `boolean` | 1, 0 and NULL; `'TRUE'`/`'FALSE'` and any non-zero number convert; printed `1`/`0` |
  | `decimal`/`numeric(p,s)`, `money`, `smallmoney` | `numeric(p,s)`, `numeric(19,4)`, `numeric(10,4)` | 18,0 without a precision, at most 38 digits; money rounds to 4 places |
  | `float(n)`, `real` | `double precision`, `real` | `float(1..24)` is `real` |
  | `date`, `time(n)` | `date`, `time(n)` | |
  | `datetime`, `smalldatetime`, `datetime2(n)` | `timestamp(3)`, `timestamp(0)`, `timestamp(n)` | `CAST` rounds `datetime` to 1/300 s and `smalldatetime` to the minute |
  | `datetimeoffset(n)` | `timestamptz(n)` | the same instant, shown in UTC (`+00:00`) |
  | `uniqueidentifier` | `uuid` | printed in upper case |
  | `char`/`nchar`/`varchar`/`nvarchar(n \| max)`, `text`, `ntext`, `sysname` | `character(n)`, `varchar(n)`, `text`, `varchar(128)` | 1 when no length is given (30 in `CAST`) |
  | `binary(n)`, `varbinary(n \| max)`, `image` | `bytea` | `0x0102FF` constants; printed as `0x...` |

  `rowversion`/`timestamp`, `sql_variant`, `xml`, `hierarchyid` and the spatial types are error
  40517. Seven fractional digits are read and the seventh is dropped (`.9999999` stays in its
  second).
- **Conversions follow SQL Server's rules** for `CAST`, `CONVERT`, assignment to a column, `+`
  and comparisons: the operand of lower data type precedence converts to the other's type
  (`1 + '1'` is 2, `'a' + 1` is error 245, `'1' + '1'` is `11`), a fraction stored into an
  integer is truncated (`10.6` stores 10), dates are read as SQL Server reads them (ISO 8601,
  `yyyymmdd`, `mm/dd/yyyy`, month names, two-digit years 00-49 as 20xx), and conversions SQL
  Server's chart forbids are errors 529 (explicit) and 206 (implicit). `CONVERT` writes and
  reads every date and time style of the reference (0 to 14, 20 to 25, 100 to 114, 120, 121,
  126 and 127; the Hijri styles 130 and 131 and the styles 140 and 141 are error 40517), the `float` styles 0 to 2 (a
  `float` turned into text without a style shows at most six digits, `1.23457e+006`, as SQL
  Server does), the `money` styles 0, 1 (`4,235.98`), 2 and 126, and the binary styles 0 to 2
  both ways (`CONVERT(varchar, 0x4E61, 1)` is `0x4E61`, `CONVERT(varbinary, '4E61', 2)` is
  `0x4E61`). Reading text, milliseconds may follow a colon (`13:05:09:120`) and six digits read
  as `yymmdd`. `TRY_CAST` and `TRY_CONVERT` return NULL where the conversion fails, and keep
  error 529 for the conversions the chart forbids.
- **`IDENTITY` and `IDENTITY(seed, increment)`** number an integer column that is the first
  column of the primary key, inline or as a table-level `CONSTRAINT ... PRIMARY KEY`: the first
  row gets the seed and each next one the largest number so far plus the increment (the
  smallest so far for a negative increment), so `IDENTITY(1000, 10)` gives 1000, 1010, 1020 and,
  after an explicit 1055 under `IDENTITY_INSERT`, 1065 (D147). The seed and increment are kept
  in the table's definition, shown by `sys.identity_columns` and `sp_help`, and must be whole
  numbers in the column type's range, the increment other than 0 (error 50000 otherwise). An
  `INSERT` without a column list leaves the column out, and an explicit value is error 544, as
  with `IDENTITY_INSERT OFF`.
- `GETDATE()` and `CURRENT_TIMESTAMP` (milliseconds), `SYSDATETIME()`, `GETUTCDATE()`,
  `SYSUTCDATETIME()`, `SYSDATETIMEOFFSET()` (the session's time zone is UTC), `CAST`,
  `CONVERT`, `TRY_CAST`, `TRY_CONVERT`, `ISNULL`, `DATEADD`, `DATEDIFF` and `DATEDIFF_BIG`
  (counting boundaries crossed), `DATEPART` and `DATENAME` (every part, `ISO_WEEK` and
  `TZOFFSET` included, weeks starting on Sunday), `YEAR`, `MONTH`, `DAY`, `EOMONTH`,
  `CHARINDEX`, `LEN`, `SUBSTRING` (characters, or bytes of a binary value), `LEFT`, `RIGHT`,
  `REPLACE`, `STUFF`, `REPLICATE` and `SPACE` (cut at 8,000 bytes, as SQL Server cuts them,
  unless the value is `varchar(max)` or `nvarchar(max)`), `REVERSE`, `TRIM` (with `characters
  FROM` and `LEADING`, `TRAILING` or `BOTH`), `LTRIM`, `RTRIM`, `CONCAT` and `CONCAT_WS`
  (skipping NULLs), `DATALENGTH`, `ASCII`, `CHAR`, `UNICODE`, `NCHAR`, `IIF`, `CHOOSE`,
  `FORMAT` (.NET's standard and custom number, date and time formats, `time` as a time span
  whose separators are escaped, cultures `en-US` and invariant), `ROUND` (keeping the type and
  scale, with truncation as its third argument), `CEILING`, `FLOOR`, `NEWID()`,
  `SCOPE_IDENTITY()`, `UPPER`, `LOWER`, `ABS`, `COALESCE`, `NULLIF` and the aggregates:
  `COUNT` returning `int` and `COUNT_BIG` `bigint`, `SUM` and `AVG` with SQL Server's result
  types (`int` for `tinyint`, `smallint` and `int`, `bigint`, `decimal(38, s)` and for `AVG`
  `decimal(38, max(s, 6))`, `float`; error 8115 past the type), `STRING_AGG` with `WITHIN GROUP
  (ORDER BY ...)`. `STRING_SPLIT(text, separator [, enable_ordinal])` is a table with a `value`
  column (and `ordinal`), and a one-character separator (error 214 otherwise). Dividing
  decimals gives SQL Server's precision and scale (`2 / 3.0` is `0.666666`, `10.0 / 4` is
  `2.500000000000`). `@@VERSION`, `@@SPID`, `@@SERVERNAME`, `@@TRANCOUNT`, `@@LANGUAGE`,
  `@@ROWCOUNT`, `@@IDENTITY`, `@@ERROR`. Another function is error 195, another `@@` name error
  137, and so is an `@variable` unless the batch declares it or it is a parameter of a
  parameterized batch (`sp_executesql` and the prepared statements of SqlClient and mssql-jdbc,
  item 23b).
- `ORDER BY ... OFFSET n ROWS [FETCH {FIRST | NEXT} m ROWS ONLY]`, `OUTPUT inserted.*` on
  `INSERT` and `UPDATE` and `OUTPUT deleted.*` on `DELETE` (columns, expressions and constants
  such as EF Core's `OUTPUT 1`), `INSERT ... [OUTPUT ...] SELECT ...`, `(VALUES (...), ...) AS
  alias (columns)` as a table, and `SELECT total = qty * price`.
- **`MERGE`** (D138): `MERGE [INTO] target [[AS] alias] USING source [[AS] alias [(columns)]] ON
  condition`, then `WHEN MATCHED [AND condition] THEN UPDATE SET ... | DELETE` and `WHEN NOT
  MATCHED [BY TARGET] [AND condition] THEN INSERT [(columns)] VALUES (...)`, and `OUTPUT` with
  `inserted.`, `deleted.` and the source's columns (rows of changed rows first, then inserted
  ones); the tag is `MERGE n`. `WHEN NOT MATCHED BY SOURCE`, a second `WHEN MATCHED`, `$action`,
  `TOP`, table hints and `OUTPUT ... INTO` are 40517, and a `MERGE` without `WHEN` is Msg 10713.
- **`OPENJSON(json [, path]) WITH (column type ['path'] [AS JSON], ...) [AS] alias`** in `FROM`
  (D138): one row per element of a JSON array (an object is one row), each column the value at
  its path (`$`, `$.key`, `$."key"`, `$[n]`, `lax`; without a path the key named as the column
  is written) converted to its type, NULL when missing or, without `AS JSON`, when it is an
  object or array; text that is not an array or object is Msg 13609, a bad path Msg 13607.
  `OPENJSON` without `WITH`, `strict` paths and `CROSS APPLY` are 40517.
- **Defaults** may be any Transact-SQL expression without column references, subqueries or
  variables, inline or through `ALTER TABLE ... ADD [CONSTRAINT name] DEFAULT ... FOR column`
  (D147). Constants are folded to the column's type (`DEFAULT ((0))` on a `bit` column is false)
  and `GETDATE()` and its kin, `NEWID()` and `NEWSEQUENTIALID()` are kept as PostgreSQL's
  equivalents, so every dialect reads them as before; any other expression, such as
  `DEFAULT (dateadd(day,(30),getdate()))`, is kept as written, without its outer parentheses, and
  runs as Transact-SQL, with SQL Server's assignment to the column, whichever dialect inserts
  the row. `sys.default_constraints` and `INFORMATION_SCHEMA.COLUMNS` show it in parentheses as
  SQL Server does; PostgreSQL's catalogs show it as written; `.schema`'s PostgreSQL `CREATE
  TABLE` leaves it out.
- **Views and `CHECK` constraints** (D124): `CREATE VIEW`, `ALTER VIEW` and `CREATE OR ALTER
  VIEW`, with a column list and `WITH SCHEMABINDING` or `VIEW_METADATA` (accepted without
  effect); the view must be the only statement of its batch (error 111), and `ORDER BY` needs
  `TOP` or `OFFSET` (error 1033). A table can be dropped under a view, which then fails with
  error 208 until the table is back, as SQL Server's views without `SCHEMABINDING` do. `CHECK`
  constraints inline, at table level and through `ALTER TABLE ... ADD CONSTRAINT`, violated with
  error 547 (a subquery in one is error 1046, an aggregate error 175); an unnamed one is called `ck__<table>__<column>`. Both keep their Transact-SQL text
  in the file and work from every dialect.
- **Transactions**: each statement commits on its own unless `BEGIN TRAN[SACTION]` opened a
  transaction; `BEGIN TRAN` nests (`@@TRANCOUNT`), the outermost `COMMIT [TRAN | WORK]`
  commits, `ROLLBACK [TRAN | WORK]` rolls everything back, and without a transaction they are
  errors 3902 and 3903. DDL is transactional. With `SET IMPLICIT_TRANSACTIONS ON` (what
  mssql-jdbc's `setAutoCommit(false)` sends) the first statement that changes data or the
  schema starts a transaction that lasts until `COMMIT` or `ROLLBACK`.
- **`SET XACT_ABORT ON`** (D148): a run-time error, a constraint violation included, rolls the
  open transaction back and ends the batch; `OFF` (the default) ends only the statement for the
  errors SQL Server ends only the statement for. Syntax errors and `RAISERROR` are not affected,
  as in SQL Server; `THROW` is. Inside `TRY` the error moves to `CATCH` and leaves the
  transaction open but uncommittable: `XACT_STATE()` is -1, `ROLLBACK` ends it, and `COMMIT` or
  any write is Msg 3930; a batch that ends with it still open rolls it back with Msg 3998.
- **Sequences** (D148): `CREATE SEQUENCE name [AS tinyint | smallint | int | bigint |
  decimal(p, 0) | numeric(p, 0)] [START WITH n] [INCREMENT BY n] [MINVALUE [n] | NO MINVALUE]
  [MAXVALUE [n] | NO MAXVALUE] [CYCLE | NO CYCLE] [CACHE [n] | NO CACHE]` with SQL Server's
  defaults (`bigint`, the type's whole range, starting at its minimum for an ascending
  sequence), `ALTER SEQUENCE name [RESTART [WITH n]]` with the same options, `DROP SEQUENCE [IF
  EXISTS] name [, ...]`, and `NEXT VALUE FOR name` in queries and defaults (`DEFAULT (NEXT VALUE
  FOR [dbo].[Orders])`). They are the sequences of [Sequences, constraints and ALTER
  TABLE](#sequences-constraints-and-alter-table), shown in `sys.sequences`. `decimal` and
  `numeric` sequences hold up to 18 digits (wider ones are Msg 11702), `NEXT VALUE FOR` returns
  a `bigint` whatever the sequence's type, its `OVER (ORDER BY ...)` form is 40517, and
  `ALTER SEQUENCE ... NO MINVALUE` takes PostgreSQL's bound rather than the type's.
- **Descriptions** (D148): `sp_addextendedproperty`, `sp_updateextendedproperty` and
  `sp_dropextendedproperty` of `MS_Description` on a schema, table or view, and on a table's
  column, index or constraint, set and remove the object's comment, which every dialect sees
  (`COMMENT ON`, `obj_description`) and `sys.extended_properties` lists for schemas, tables, views
  and columns. Other properties and other object types are 40517.
- **Batch logic**: variables, `IF ... ELSE`, `WHILE`, `BEGIN ... END`, `TRY ... CATCH`,
  `THROW`, `RAISERROR`, `PRINT` and dynamic SQL; see
  [Transact-SQL batch logic](#transact-sql-batch-logic).
- `USE` the served database (another is error 911), and `SET NOCOUNT`, `ANSI_NULLS`,
  `QUOTED_IDENTIFIER`, `ANSI_PADDING`, `ANSI_WARNINGS`, `ARITHABORT`, `CONCAT_NULL_YIELDS_NULL`
  and `ANSI_NULL_DFLT_ON` `ON`, `IMPLICIT_TRANSACTIONS ON` and `OFF`, `TEXTSIZE`, `LANGUAGE
  us_english`, `DATEFORMAT mdy`, `LOCK_TIMEOUT` and `TRANSACTION ISOLATION LEVEL` (all run
  serializable).
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

Not yet: TLS, MARS, cancelling a running batch (attention is answered when the batch ends), and SSMS's and Azure Data Studio's own catalog queries (the `sys`
views and catalog procedures drivers use are in [SQL Server metadata for
tools](#sql-server-metadata-for-tools)).

## .NET and Java apps over TDS

.NET apps through Microsoft.Data.SqlClient and Java apps through Microsoft's JDBC driver
(mssql-jdbc) run against `eggdb serve --tds-listen` unchanged, apart from turning encryption
off (roadmap item 23b, D132):

```csharp
using var connection = new SqlConnection(
    "Server=127.0.0.1,1433;Database=shop;User ID=eggdb;Password=choose-one;Encrypt=false");
connection.Open();
using var insert = new SqlCommand("INSERT INTO bakes (id, name) VALUES (@id, @name)", connection);
insert.Parameters.Add("@id", SqlDbType.Int).Value = 1;
insert.Parameters.Add("@name", SqlDbType.NVarChar, 40).Value = "bread";
insert.ExecuteNonQuery();
```

```java
Connection connection = DriverManager.getConnection(
    "jdbc:sqlserver://127.0.0.1:1433;databaseName=shop;encrypt=false", "eggdb", "choose-one");
PreparedStatement select = connection.prepareStatement("SELECT name FROM bakes WHERE id = ?");
select.setInt(1, 1);
```

What the server does:
- **Parameterized statements** arrive as remote procedure calls (RPC requests), by name or by
  the ids the drivers use: `sp_executesql` runs a batch once with its parameters;
  `sp_prepare` and `sp_prepexec` prepare one and return its handle (an `int` output parameter,
  numbered from 1 on each connection), `sp_execute` runs it again by its handle and
  `sp_unprepare` forgets it (a handle it does not know is error 8179). Several calls may come in
  one request. Each `@name` in the text takes the value of its parameter, given by name or by
  position in the declaration list (`@p1 int, @p2 nvarchar(40) OUTPUT`), converted to its
  declared type as `CAST` would; a declared parameter without a value is error 8178, a value
  for an undeclared one 8145, one too many 8144. The statements run with the batch rules above
  and the results come back as SQL Server sends them (DONEINPROC per statement, then the
  return status 0, the output parameters and DONEPROC). A connection keeps at most 65,536
  prepared statements and 64 MiB of their text.
- **Parameter types** are those the drivers send: `tinyint`, `smallint`, `int` and `bigint`,
  `bit`, `real` and `float`, `decimal`/`numeric`, `money` and `smallmoney`, `datetime` and
  `smalldatetime`, `date`, `time`, `datetime2`, `datetimeoffset` (its instant), `uniqueidentifier`,
  `nvarchar`/`nchar`/`ntext` (with `nvarchar(max)` in chunks), `varchar`/`char`/`text` (read as
  Windows-1252, or UTF-8 under a UTF-8 collation) and `varbinary`/`binary`/`image`. `sql_variant`,
  `xml`, user-defined and table-valued parameters and Always Encrypted are error 40517.
- **Transactions** from `SqlConnection.BeginTransaction`, `Commit` and `Rollback` arrive as
  transaction manager requests, which begin, commit and roll back as `BEGIN TRAN`, `COMMIT` and
  `ROLLBACK` do (a commit or rollback that begins the next transaction too included) and report
  the transaction descriptor the driver sends back. A save point (`SqlTransaction.Save`, `SAVE
  TRANSACTION name`) is recorded, and rolling back to it is error 40517 with the transaction kept
  (D138); distributed transactions are error 40517. mssql-jdbc's `setAutoCommit(false)`, `commit()` and
  `rollback()` send `SET IMPLICIT_TRANSACTIONS ON` and `IF @@TRANCOUNT > 0 COMMIT TRAN` batches,
  which run as above.
- **Pooled connections**: when SqlClient hands out a pooled connection again it asks for a reset
  with its next request; the server rolls back the open transaction (unless asked to keep it),
  puts every `SET` option back as after login and acknowledges the reset first. Prepared handles
  stay.

CI's `tds-apps` job runs a C# program through SqlClient and a Java program through mssql-jdbc
(parameterized CRUD, prepared statements, a committed and a rolled-back transaction, an error
and, for SqlClient, a reused pooled connection) and compares what each prints with
`scripts/tds-apps/expected-*.out`.

## Bulk loads with bcp and SqlBulkCopy

`SqlBulkCopy` and `bcp in` load rows into an EggDB table over TDS as they load them into SQL
Server (roadmap item 46, D136), and `eggdb import --dialect sqlserver` loads the files `bcp
... out -c` writes without a server:

```csharp
using var copier = new SqlBulkCopy(connection) { DestinationTableName = "dbo.items", BatchSize = 5000 };
copier.WriteToServer(table);
```

```sh
bcp shop.dbo.items in items.dat -c -S 127.0.0.1,1433 -U eggdb -P choose-one -d shop
eggdb import --dialect sqlserver shop.db --csv items.dat --table dbo.items
```

What the server does:
- **`SqlBulkCopy`'s description of the table**: it first sends `select @@trancount; SET FMTONLY
  ON select * from <table> SET FMTONLY OFF exec ..sp_tablecollations_100 N'<table>'`. With
  `SET FMTONLY ON` a query returns its columns and no rows, each described by its declared type
  (unbounded text as `nvarchar(max)`, `bytea` as `varbinary(max)`, an unconstrained `numeric` as
  text), and other statements but `SET` do not run. `sp_tablecollations_100` lists each column
  with its collation (Latin1_General_100_BIN2 for the character ones, NULL for the others).
- **`INSERT BULK table [(column type [COLLATE name], ...)] [WITH (option, ...)]`** checks the
  table and its columns and waits for the bulk load message that must come next; any other
  request first ends the wait. The column types are read and ignored: the values' types come
  with the data. Without a column list the data holds every column. `KEEP_NULLS` keeps NULLs;
  without it a NULL takes its column's default. `TABLOCK`, `CHECK_CONSTRAINTS`,
  `FIRE_TRIGGERS`, `KEEPIDENTITY`, `ORDER (...)`, `ROWS_PER_BATCH` and `KILOBYTES_PER_BATCH` are
  accepted; constraints are always checked, and there are no triggers yet.
- **The bulk load message** (packet type 0x07) holds column metadata, a row token per row (or
  a row with a null bitmap) and DONE. It is read as it arrives, a row at a time, so a load of
  any size takes no more memory than its largest row (at most 64 MiB). The values have the
  types of RPC parameters (above) plus `text`, `ntext` and `image`, and convert to their columns
  as SQL Server's `INSERT` converts them. The rows load in one statement through the `INSERT`
  path (defaults, identity, constraints, indexes, foreign keys): all of them, or none and an
  error (a duplicate key is 2627, a missing value 515). Each `SqlBulkCopy` batch (`BatchSize`)
  is its own `INSERT BULK` and message, so its own transaction unless
  `SqlBulkCopyOptions.UseInternalTransaction` or an open transaction holds them. The answer is
  a DONE with the row count. An identity column the statement names keeps the values sent
  (`SqlBulkCopyOptions.KeepIdentity`); one it leaves out is numbered.
- **Bad data**: bulk data that is not valid TDS is error 4804, a column type EggDB does not read
  (`sql_variant`, `xml`, user-defined types) 4816, data with another number of columns than
  `INSERT BULK` named, or a row over 64 MiB, 4815, Always Encrypted columns 40517, and bulk data
  without an `INSERT BULK` before it 4002. The rest of the message is read and dropped, and the
  connection goes on.

`eggdb import --dialect sqlserver <file> --csv <data file> --table <name>` reads bcp's character
format (`-c`): every field but the last of a row ends with the field terminator (`--delimiter`,
bcp's `-t`, a tab by default) and the last with the row terminator (`--row-terminator`, bcp's
`-r`, a newline by default, `\r\n` accepted with it), with bcp's escapes `\t`, `\n`, `\r`, `\0`
and `\\`. An empty field is NULL (the column's default unless `--keep-nulls`, bcp's `-k`), a
field that is one NUL byte is an empty string, and `--header` skips the first row (bcp's
`-F 2`). The text must be UTF-8 (`bcp ... -c -C 65001`); invalid bytes are error 4864, a row
cut short 4832. The rows go through `INSERT BULK` and the same load as above, in one
transaction; values convert as SQL Server converts text (`1`/`0` for `bit`, `2024-05-06
07:08:09.123` for `datetime2`). `bcp`'s native (`-n`) and Unicode (`-w`) formats and format
files are not read yet.

CI's `tds-apps` job loads the committed bcp file `crates/eggdb/tests/data/sqlserver/bcp/items.dat`
with `eggdb import --dialect sqlserver` and with `SqlBulkCopy` (in batches of two, with
`KeepNulls`, `UseInternalTransaction`, `TableLock`, and a load that fails on a duplicate key) and
compares both tables' rows with the file and what the program prints with
`scripts/tds-apps/expected-bulk-dotnet.out`. `bcp` itself is not run in CI: its license does not
allow it there.

## Transact-SQL batch logic

A batch (what a client sends at once; what sqlcmd sends between `GO` lines) runs Transact-SQL's
control-of-flow language (roadmap item 47, D137) in `eggdb sql --dialect sqlserver`, `eggdb
restore`, MCP and over TDS:

```sql
DECLARE @i int = 0, @odd int = 0
WHILE @i < 10
BEGIN
    SET @i += 1
    IF @i % 2 = 0 CONTINUE
    SET @odd += @i
END
BEGIN TRY
    INSERT INTO bakes (name) VALUES (NULL)
END TRY
BEGIN CATCH
    PRINT N'not inserted: ' + ERROR_MESSAGE()
END CATCH
SELECT @odd AS odd_sum
```

- **The whole batch is compiled first**, as SQL Server does: a syntax error, a variable used
  before its `DECLARE` (Msg 137) or declared twice (134), `BREAK` or `CONTINUE` outside a loop
  (135, 136), `RETURN` with a value outside a procedure or function (178), `THROW;` outside a `CATCH` block (10704) and a `SELECT`
  mixing assignments with columns (141) run nothing.
- **Variables**: `DECLARE @name type [= value], ...` with SQL Server's types. A variable exists
  from its `DECLARE` to the end of the batch, also when that `DECLARE` sits in a branch that does
  not run, starts NULL (a `DECLARE` without a value in a loop does not reset it), and keeps its
  type: assigning converts as SQL Server does (`varchar(3)` keeps three characters, `tinyint`
  checks its range). `SET @v = expression`, `SET @v += expression` (and `-=`, `*=`, `/=`, `%=`),
  and `SELECT @a = expression, @b = ... [FROM ... WHERE ... ORDER BY ...]`, which takes the last
  row's values, leaves the variables unchanged when there is no row, and with `+=` accumulates
  row by row (`SELECT @names += name + N',' FROM t`).
- **Control of flow**: `IF condition statement [ELSE statement]` with any condition (subqueries,
  `[NOT] EXISTS (...)`, `OBJECT_ID(...) IS NULL`, `@@ROWCOUNT`), `BEGIN ... END`, `WHILE
  condition statement` with `BREAK` and `CONTINUE`, and `RETURN`. A condition leaves
  `@@ROWCOUNT` as the statement before it set it.
- **Errors**: inside `BEGIN TRY ... END TRY BEGIN CATCH ... END CATCH` an error moves to the
  `CATCH` block instead of reaching the client; there `ERROR_NUMBER()`, `ERROR_SEVERITY()`,
  `ERROR_STATE()`, `ERROR_LINE()`, `ERROR_MESSAGE()` and `ERROR_PROCEDURE()` (NULL) describe it,
  and they are NULL outside. As in SQL Server, an unknown table or column (Msg 207, 208) in the
  `TRY` block's own statements is not caught, while one inside dynamic SQL is. `THROW number,
  message, state` (numbers 50000 to 2147483647, severity 16) ends the batch unless caught, and
  `THROW;` in a `CATCH` block raises the caught error again. `RAISERROR (message, severity,
  state [, argument, ...]) [WITH LOG | NOWAIT | SETERROR]` fills `%d %i %u %o %x %X %s` (with
  flags, width and precision, `*` from the arguments) and sends severity 0-10 as an
  informational message; 11-18 is error 50000, which ends only its statement; above 18 is Msg
  2754, and a message number instead of a text is Msg 18054 (there is no `sp_addmessage`).
  `@@ERROR` is the last statement's error number (0 after one that succeeded) and
  `XACT_STATE()` is 1 inside a transaction, -1 inside one that `XACT_ABORT` made uncommittable,
  and 0 outside.
- **`PRINT expression`** sends its text, up to 8,000 characters, as an informational message: a
  TDS `INFO` token (as `RAISERROR` of severity 10 or less), a notice in `eggdb sql`.
- **Dynamic SQL**: `EXEC (string [+ string ...])` and `EXEC sp_executesql @statement [,
  @params [, @p = value [OUTPUT], ...]]` run the text as a batch of its own: it does not see the
  caller's variables or table variables, binds its parameters by position or name (Msg 8144,
  8145, 8162, 8178), copies `OUTPUT` parameters back, and nests up to 32 levels (Msg 217).
  `EXEC sp_executesql N'CREATE VIEW ...'` is how SSMS creates a view inside an existence guard.
- **Table variables**: `DECLARE @t TABLE (column type [NULL | NOT NULL], ...)`, `INSERT [INTO]
  @t [(columns)] VALUES (...), ... | SELECT ... | EXEC procedure`, `UPDATE @t SET column =
  expression, ... [WHERE ...]`, `DELETE [FROM] @t [WHERE ...]` and `SELECT ... FROM @t
  [alias]`, for the batch.
- Statements that only assign variables (`DECLARE`, `SET @v`, `SELECT @v = ...`) send no done
  message or row count of their own; `@@ROWCOUNT` is 1 after `SET @v` and the number of rows
  after `SELECT @v = ...`.
- Over TDS, a batch run by `sp_executesql`, `sp_prepexec` or `sp_execute` may assign its
  parameters (`SET @p = ...`), and its `OUTPUT` parameters return the values it assigned, in
  their declared types. `EXEC sp_unprepare handle` inside a batch, as mssql-jdbc sends it,
  releases the connection's prepared statement. A server that stops cancels the batches its
  connections run.

CI's `tds-apps` job runs `scripts/tds-apps/batch-logic.sql` batch by batch through
Microsoft.Data.SqlClient, the way sqlcmd sends a script, and compares what it prints (result
sets, `INFO` messages and errors with their numbers, severities, states and lines) with
`scripts/tds-apps/expected-logic-dotnet.out`.

## Transact-SQL procedures and functions

In the SQL Server dialect (roadmap 3.15, D159), users create stored procedures and user-defined
functions in Transact-SQL and call them from batches, over RPC and from other routines and
triggers:

```sql
CREATE PROCEDURE dbo.stock_of @name nvarchar(20), @qty int OUTPUT
AS
    SET NOCOUNT ON;
    SELECT @qty = qty FROM items WHERE name = @name;
    IF @qty IS NULL RETURN 1;
    SELECT id, name FROM items WHERE name = @name;
    RETURN 0
GO
DECLARE @q int, @rc int
EXEC @rc = dbo.stock_of N'oat', @q OUTPUT
SELECT @rc AS status, @q AS qty
GO
CREATE FUNCTION dbo.doubled (@n int) RETURNS int AS BEGIN RETURN @n * 2 END
GO
CREATE FUNCTION dbo.in_stock (@min int) RETURNS TABLE
AS RETURN (SELECT id, name FROM items WHERE qty >= @min)
GO
SELECT name, dbo.doubled(qty) AS doubled FROM items
SELECT id, name FROM dbo.in_stock(5)
```

- **Statements**: `CREATE [OR ALTER] {PROCEDURE | PROC} name [@parameter type [= default]
  [OUT | OUTPUT] [READONLY], ...] [WITH option, ...] [FOR REPLICATION] AS body` and `CREATE [OR
  ALTER] FUNCTION name ([@parameter type [= default], ...]) RETURNS type [WITH option, ...] [AS]
  BEGIN ... RETURN expression END` (scalar), `RETURNS TABLE [AS] RETURN [(] query [)]` (inline
  table-valued) and `RETURNS @variable TABLE (column type, ...) [AS] BEGIN ... RETURN END`
  (multi-statement table-valued); `ALTER PROCEDURE` and `ALTER FUNCTION` (Msg 208 when it does
  not exist, 2010 for the other kind), and `DROP {PROCEDURE | PROC | FUNCTION} [IF EXISTS] name,
  ...` (Msg 3701, 3705). `CREATE` must start its batch (Msg 111); a procedure's body runs to the
  end of the batch. `WITH RETURNS NULL ON NULL INPUT` skips a scalar function's body for a NULL
  argument; `EXECUTE AS`, `RECOMPILE`, `ENCRYPTION`, `SCHEMABINDING` and `CALLED ON NULL INPUT`
  are accepted and change nothing. Procedures, functions, tables and views share one name space
  per schema (Msg 2714), and a name has one routine: Transact-SQL has no overloads.
- **Bodies are compiled at `CREATE`** with the batch compiler, so the errors a batch reports
  before it runs (syntax, undeclared variables, Msg 137) refuse the `CREATE`. A function body
  that would change the database or talk to the client is refused as SQL Server refuses it:
  `INSERT`, `UPDATE`, `DELETE`, `MERGE`, `PRINT`, `RAISERROR`, `THROW`, dynamic SQL and `SET`
  options are Msg 443, a `SELECT` that returns rows is Msg 444, and `EXEC` of a procedure is Msg
  557; table variables the function declares, and its return table, may change.
- **`EXEC [@status =] name [value | @variable [OUTPUT] | DEFAULT], ... | @parameter = ...`**
  binds arguments by position and then by name, fills defaults, converts each value to the
  parameter's declared type, and reports SQL Server's errors (Msg 201 missing, 8144 too many,
  8145 unknown name, 8143 given twice, 8162 `OUTPUT` for an input parameter, 2812 unknown
  procedure, 2809 a function run with `EXEC`). The procedure runs inside the caller's batch: its
  result sets, `PRINT` and `RAISERROR` messages and row counts reach the client in order, its
  errors reach the caller's `TRY ... CATCH`, `OUTPUT` parameters are copied back, and `RETURN n`
  sets the status (0 without a value). Its `SET NOCOUNT`, `XACT_ABORT` and `IMPLICIT_TRANSACTIONS`
  return to the caller's values, a procedure that changes `@@TRANCOUNT` ends with Msg 266, and
  `@@NESTLEVEL` counts the levels. `INSERT INTO table | @variable EXEC procedure ...` keeps its
  rows, and a trigger body may `EXEC` a procedure.
- **Functions** are called with their schema in expressions (`dbo.doubled(qty)`, once per row
  where the row matters) and table-valued ones as a `FROM` item (`FROM dbo.in_stock(5) [AS] x`).
  An argument may be `DEFAULT`; the count must match (Msg 313, 8144), and a procedure called as
  a function is Msg 4121. Functions read the database as their statement sees it.
- **Over TDS**, an RPC request naming a procedure (`CommandType.StoredProcedure` in SqlClient,
  `{call ...}` and `CallableStatement` in mssql-jdbc) runs it with its parameters by name or
  position, sends `RETURNSTATUS` with the status and `RETURNVALUE` tokens with the `OUTPUT`
  parameters, and `SqlCommandBuilder.DeriveParameters` lists the parameters through
  `sp_procedure_params_100_managed`.
- **Catalogs**: procedures and functions are in `sys.objects` (types `P`, `FN`, `IF`, `TF`),
  `sys.procedures`, `sys.parameters`, `sys.sql_modules`, `INFORMATION_SCHEMA.ROUTINES` and
  `PARAMETERS`, `OBJECT_ID`, `OBJECT_DEFINITION`, `OBJECTPROPERTY` and `sp_helptext`, and in
  `sp_stored_procedures`, `sp_sproc_columns` and `sp_help`. `ALTER` keeps the text as a `CREATE`
  statement, as SQL Server shows it.
- **Limits**: procedures and functions nest at most 32 levels (Msg 217), with the dynamic SQL
  they run counted in.

Not yet: a table-valued function after the first `FROM` item (joins and `CROSS APPLY`), a
procedure called by its name alone as the first statement of a batch (write `EXEC`), `EXEC
@result = dbo.scalar_function`, `@@PROCID` (0) and `ERROR_PROCEDURE()` (NULL), numbered and
temporary (`#name`) procedures, CLR routines (`AS EXTERNAL NAME`), `SELECT INTO` in a body, and
`COMMIT` or `ROLLBACK` of the caller's transaction inside a function.

## SQL Server metadata for tools

Metadata tools and drivers browse an EggDB database over TDS as they browse SQL Server (roadmap
item 41, D133): mssql-jdbc's `DatabaseMetaData` (`getTables`, `getColumns`, `getPrimaryKeys`,
`getImportedKeys`, `getIndexInfo`, `getTypeInfo`, `getCatalogs`, `getUserName`) and SqlClient's
`GetSchema("Tables" | "Columns" | "Indexes" | "IndexColumns" | "ForeignKeys")` list its schemas,
tables, views, columns, keys and indexes, and so do queries written against `sys` and
`INFORMATION_SCHEMA`:

```sql
SELECT s.name, t.name, c.name, c.system_type_id, c.max_length, c.is_nullable
FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id
JOIN sys.columns c ON c.object_id = t.object_id ORDER BY s.name, t.name, c.column_id
SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, DATA_TYPE FROM INFORMATION_SCHEMA.COLUMNS
EXEC sp_columns @table_name = N'orders', @table_owner = N'sales'
EXEC sp_help 'sales.orders'
SELECT OBJECT_ID('sales.orders'), SERVERPROPERTY('ProductVersion'), DB_NAME(), SCHEMA_NAME()
```

- **`sys` views**: `sys.objects`, `sys.tables`, `sys.views`, `sys.columns`, `sys.types`,
  `sys.schemas`, `sys.indexes`, `sys.index_columns`, `sys.foreign_keys`,
  `sys.foreign_key_columns`, `sys.key_constraints`, `sys.default_constraints`,
  `sys.check_constraints`, `sys.identity_columns`, `sys.sequences`, `sys.databases` and
  `sys.sql_modules` with SQL Server 2022's columns, and `sys.extended_properties` with the
  `MS_Description` of schemas, tables, views and columns, `sys.triggers` with every trigger (also
  in `sys.objects` as type `TR` and in `sys.sql_modules` with its text), and `sys.procedures` and
  `sys.parameters` with every Transact-SQL procedure and function (also in `sys.objects` as
  types `P`, `FN`, `IF` and `TF`). They are built for each statement that
  names them from the stored tables, views and sequences, read-only, and reached only with their
  schema (`sys.tables`, not `tables`).
- **`INFORMATION_SCHEMA`**: `SCHEMATA`, `TABLES`, `COLUMNS`, `VIEWS`, `VIEW_TABLE_USAGE`,
  `SEQUENCES`, `TABLE_CONSTRAINTS`, `KEY_COLUMN_USAGE`, `REFERENTIAL_CONSTRAINTS`,
  `CONSTRAINT_COLUMN_USAGE`, `CONSTRAINT_TABLE_USAGE` and `CHECK_CONSTRAINTS` with SQL Server's
  columns and values (schema `dbo`, `nvarchar`, `BASE TABLE`), and `ROUTINES` and `PARAMETERS`
  with the Transact-SQL procedures and functions.
- **Ids**: a table, view, sequence or constraint has an `object_id` made from a hash of its
  kind, table and name, the same in every statement and after a restart; it changes when the
  object is renamed. `dbo` is schema 1, `sys` 4, other schemas count from 5 in name order; the
  database is `database_id` 5. A primary key is the table's clustered index 1, a table without
  one is a heap (index 0), other indexes count from 2.
- **Functions**: `SERVERPROPERTY` (`ProductVersion` `16.0.1000.0`, `EngineEdition` 3, `Edition`
  `EggDB (64-bit)`, `Collation`, ...), `DATABASEPROPERTYEX`, `DB_NAME`, `DB_ID`, `OBJECT_ID`
  (with a type: `OBJECT_ID('t', 'U')`), `OBJECT_NAME`, `OBJECT_SCHEMA_NAME`, `SCHEMA_NAME`,
  `SCHEMA_ID`, `TYPE_NAME`, `TYPE_ID`, `COLUMNPROPERTY`, `OBJECTPROPERTY`, `OBJECTPROPERTYEX`,
  `COL_NAME`, `COL_LENGTH`, `IS_SRVROLEMEMBER`, `IS_MEMBER`, `HAS_DBACCESS`, `SUSER_SNAME()`,
  `ORIGINAL_LOGIN()`, `USER_NAME()`, `SYSTEM_USER` (the login), `CURRENT_USER`, `SESSION_USER`
  and `USER` (`dbo`), and `@@MICROSOFTVERSION`, `@@SERVICENAME`, `@@MAX_PRECISION`,
  `@@DATEFIRST`, `@@LANGID`, `@@LOCK_TIMEOUT`, `@@TEXTSIZE`, `@@MAX_CONNECTIONS`, `@@NESTLEVEL`
  and `@@PROCID`.
- **Catalog procedures**, by `EXEC` (arguments by position or as `@name = value`, `DEFAULT`), as
  the first statement of a batch without `EXEC`, and by RPC: `sp_tables`, `sp_columns`,
  `sp_columns_100`, `sp_pkeys`, `sp_fkeys`, `sp_statistics`, `sp_statistics_100`,
  `sp_special_columns`, `sp_datatype_info`, `sp_datatype_info_100`, `sp_stored_procedures`,
  `sp_sproc_columns`, `sp_help` (its several result sets), `sp_helpindex`, `sp_databases`, and
  `sp_columns_managed`, `sp_indexes_managed` and `sp_indexcolumns_managed`, which SqlClient's
  `GetSchema` calls. Name arguments take SQL Server's `LIKE` patterns where the procedure does
  (`%`, `_`, `[...]`), compared without regard to case. Another procedure is Msg 2812.
- **Table variables** (item 47 widened them): `DECLARE @t TABLE (column type [NULL | NOT NULL],
  ...)`, `INSERT [INTO] @t [(columns)] EXEC procedure ...` as mssql-jdbc's `getColumns` and
  `getImportedKeys` send it, and `SELECT ... FROM @t [alias]` within one batch; `COLLATE` in an
  expression is accepted without effect.

CI's `tds-apps` job restores the sqlpackage fixture
(`crates/eggdb/tests/data/sqlserver/shop-sqlpackage.sql`) and compares what mssql-jdbc's
`DatabaseMetaData` and SqlClient's `GetSchema` list for it with
`scripts/tds-apps/expected-metadata-*.out`.

## Oracle SQL over PostgreSQL and TDS connections

Apps reach Oracle's SQL through the drivers EggDB already speaks (roadmap item 44, D134): a
connection chooses the dialect its statements are read in with the setting `eggdb.dialect`,
whose value is the connection's own dialect or `oracle`. On a PostgreSQL connection it comes in
the startup packet's `options` (`-c eggdb.dialect=oracle`), which psql takes from `PGOPTIONS`,
pgjdbc from its `options` property and libpq-based drivers from `options` in the connection
string; or later with `SET eggdb.dialect = oracle` (or `TO`), from either dialect. On a TDS
connection a batch sends `SET eggdb.dialect oracle` (or `= oracle`).

```sh
eggdb restore --dialect oracle --create hr.db hr_install.sql   # Oracle's HR sample schema
EGGDB_PASSWORD=secret eggdb serve hr.db --user hr
PGPASSWORD=secret PGOPTIONS='-c eggdb.dialect=oracle' psql -h 127.0.0.1 -U hr hr
hr=> SELECT table_name FROM user_tables;
hr=> SELECT last_name FROM employees ORDER BY employee_id FETCH FIRST 3 ROWS ONLY;
```

```java
Properties properties = new Properties();
properties.setProperty("user", "hr");
properties.setProperty("password", "secret");
properties.setProperty("options", "-c eggdb.dialect=oracle");
Connection connection = DriverManager.getConnection("jdbc:postgresql://127.0.0.1:5432/hr", properties);
```

- **What the choice changes**: the statements after it are read and answered as in
  [the Oracle dialect](#the-oracle-dialect): Oracle's names, functions, `''` as NULL, `DUAL`,
  `ROWNUM`, `ORA-` errors (sent with their SQLSTATE), the data dictionary and `SYS_CONTEXT`.
  `SET eggdb.dialect = postgresql` (or `= DEFAULT`) goes back; `RESET eggdb.dialect` and `SHOW
  eggdb.dialect` work in PostgreSQL's dialect. Another dialect is `0A000`, an unknown name
  `22023` (and ends a connection that sent it at startup), and a change inside a transaction
  `25001`.
- **Where it applies**: in a query string with several statements (psql `-c`, a simple query),
  the statements after the `SET` run in the new dialect. In a Transact-SQL batch the statements
  after `SET eggdb.dialect oracle` are read in Oracle's dialect, checked before any of them
  runs.
- **Transactions follow the connection**: drivers control them as their protocol does, so a
  PostgreSQL connection commits each statement outside `BEGIN` ... `COMMIT` (pgjdbc's
  `setAutoCommit(false)` sends `BEGIN`, which Oracle's dialect takes as the start of a
  transaction when it stands alone), and a TDS connection keeps SQL Server's rules
  (`IMPLICIT_TRANSACTIONS`, transaction manager requests, which run as Transact-SQL whichever
  dialect the connection chose). Oracle's DDL still commits what is open.
- **Parameters and values**: pgjdbc's and libpq's `$1` placeholders work in Oracle's dialect
  next to Oracle's `:name` and `:1`. Values go out with the protocol's closest types (`NUMBER`
  as `numeric`, `DATE` as `timestamp(0)`, `VARCHAR2` as `text` or `varchar`, over TDS as SQL
  Server's), so the drivers read them unchanged. A pooled TDS connection's reset returns to
  Transact-SQL.

Known differences: a query string that the previous dialect cannot split into tokens (an
Oracle `q'[...]'` literal sent in one psql `-c` with the `SET` before it) fails before anything
runs, so send the `SET` on its own; psql's backslash commands and pgjdbc's `DatabaseMetaData`
read PostgreSQL's catalog, which Oracle's dialect does not answer (use the data dictionary); over
TDS, `sp_executesql` parameters (`@name`) are read only in Transact-SQL, and mssql-jdbc's
`setAutoCommit(false)` sends Transact-SQL (`set implicit_transactions on`) that Oracle's dialect
refuses, so use plain batches there.

CI's `oracle-apps` job restores HR's installer unchanged, serves it as the user `hr` and runs
`scripts/oracle-apps/commands.sql` through psql and `crates/eggdb/tests/clients/java/OracleAppCheck.java`
through pgjdbc with `-c eggdb.dialect=oracle`, comparing their output with
`scripts/oracle-apps/expected-*.out`.

## PL/SQL blocks, procedures, functions and packages

In the Oracle dialect (roadmap 33e, D160), PL/SQL runs as Oracle's PL/SQL Language Reference
documents it: anonymous blocks, standalone procedures and functions, packages with a
specification and a body, and triggers ([Triggers](#triggers)). It runs through `eggdb sql
--dialect oracle` (a script's PL/SQL units end at a `/` line, as in SQL*Plus), `eggdb shell
--dialect oracle`, the embedded API, a PostgreSQL or TDS connection that chose Oracle's dialect
and `eggdb restore --dialect oracle`; there is no Oracle wire protocol (D108).

```sql
CREATE OR REPLACE PROCEDURE raise_salary (p_empno IN NUMBER, p_amount IN NUMBER, p_new_sal OUT NUMBER) IS
BEGIN
  UPDATE emp SET sal = sal + p_amount WHERE empno = p_empno;
  SELECT sal INTO p_new_sal FROM emp WHERE empno = p_empno;
END raise_salary;
/
SET SERVEROUTPUT ON
DECLARE
  v_sal emp.sal%TYPE;
BEGIN
  raise_salary(7839, 100, v_sal);
  DBMS_OUTPUT.PUT_LINE('new salary ' || v_sal);
EXCEPTION
  WHEN NO_DATA_FOUND THEN
    RAISE_APPLICATION_ERROR(-20001, 'no such employee');
END;
/
```

- **Statements**: `CREATE [OR REPLACE] [EDITIONABLE | NONEDITIONABLE] PROCEDURE name [(parameter
  [IN | OUT | IN OUT] [NOCOPY] type [{:= | DEFAULT} expression], ...)] [AUTHID ...] {IS | AS}
  declarations BEGIN ... [EXCEPTION ...] END [name];`, the same for `FUNCTION ... RETURN type`
  with `DETERMINISTIC`, `PIPELINED`, `PARALLEL_ENABLE`, `RESULT_CACHE`, `ACCESSIBLE BY`,
  `SHARING` and `DEFAULT COLLATION` accepted; `CREATE [OR REPLACE] PACKAGE name {IS | AS} ... END
  [name];` and `CREATE [OR REPLACE] PACKAGE BODY name {IS | AS} ... [BEGIN initialization] END
  [name];`; `DROP PROCEDURE`, `DROP FUNCTION`, `DROP PACKAGE [BODY]`; `ALTER PROCEDURE |
  FUNCTION | PACKAGE ... COMPILE` (compiles the unit again and reports its errors); `CALL name(arguments)` and
  SQL*Plus's `EXECUTE name(arguments)`; anonymous blocks `[DECLARE ...] BEGIN ... END;`, also
  labelled (`<<name>>`). Oracle's DDL rules hold: a name is used by one table, view, procedure,
  function or package (`ORA-00955`), a missing one is `ORA-04043`, a body needs its
  specification, and DDL commits.
- **Declarations**: variables and `CONSTANT`s with `NOT NULL` and defaults, every Oracle type plus
  `PLS_INTEGER`, `BINARY_INTEGER`, `NATURAL`, `POSITIVE`, `SIMPLE_INTEGER`, `SIGNTYPE` and
  `BOOLEAN`; `%TYPE` and `%ROWTYPE` on columns, tables, cursors and variables (a `%TYPE` parameter
  follows its column when the column changes); `TYPE ... IS RECORD` with field defaults; `SUBTYPE`;
  cursors with parameters and defaults; `REF CURSOR` and `SYS_REFCURSOR`; exceptions with
  `PRAGMA EXCEPTION_INIT`; and nested procedures and functions, which see the variables of the
  block that declares them.
- **Statements in a block**: assignment, `IF ... ELSIF ... ELSE`, simple and searched `CASE`
  (`CASE_NOT_FOUND`), `LOOP`, `WHILE`, numeric `FOR` (`REVERSE`), cursor `FOR` loops over a cursor
  or a query, `EXIT [WHEN]`, `CONTINUE [WHEN]`, labels on blocks and loops (`outer.x` names a
  hidden variable), `NULL`, `RETURN`, procedure calls with positional and named (`=>`)
  arguments and `OUT`/`IN OUT` results, embedded `SELECT ... INTO` (`NO_DATA_FOUND`,
  `TOO_MANY_ROWS`), `INSERT`, `UPDATE`, `DELETE` and `MERGE`, `OPEN`,
  `FETCH ... INTO`, `CLOSE`, cursor attributes `%FOUND`, `%NOTFOUND`, `%ROWCOUNT` and `%ISOPEN`
  on cursors and on `SQL`, `EXECUTE IMMEDIATE ... [INTO] [USING]` (also of PL/SQL and DDL),
  `OPEN ... FOR` a query or a string, `COMMIT` and `ROLLBACK` (the transaction ends that way when
  the statement that ran the block finishes), and DDL through `EXECUTE IMMEDIATE`.
- **Collections**: associative arrays (`INDEX BY PLS_INTEGER | VARCHAR2(n)`), nested tables and
  `VARRAY`s with constructors, `v(i)`, `v(i).field`, the methods `COUNT`, `FIRST`, `LAST`,
  `NEXT`, `PRIOR`, `EXISTS`, `LIMIT`, `EXTEND`, `TRIM` and `DELETE`, Oracle's errors
  (`COLLECTION_IS_NULL`, `SUBSCRIPT_BEYOND_COUNT`, `SUBSCRIPT_OUTSIDE_LIMIT`, `NO_DATA_FOUND`
  for a missing element), `SELECT` and `FETCH ... BULK COLLECT INTO [LIMIT n]`, and `FORALL` over
  a range, `INDICES OF` and `VALUES OF` with `SAVE EXCEPTIONS`, `SQL%BULK_ROWCOUNT` and
  `SQL%BULK_EXCEPTIONS`.
- **Exceptions**: the predefined exceptions by name with their Oracle numbers, user exceptions,
  `RAISE`, `RAISE;` in a handler, `WHEN a OR b`, `WHEN OTHERS`, `SQLCODE` and `SQLERRM [(n)]`, and
  `RAISE_APPLICATION_ERROR(-20000 .. -20999, message [, TRUE])`. A handled exception undoes only
  the statement that failed, as in Oracle; an unhandled one reaches the client with Oracle's
  number and one `ORA-06512: at "SCHEMA.UNIT", line N` (or `at line N` for a block) per unit it
  left. Line numbers count from the unit's `PROCEDURE`, `FUNCTION`, `PACKAGE` or `TRIGGER`
  keyword, as `USER_SOURCE` shows the unit.
- **Functions in SQL**: a standalone or package function is called in any expression (`SELECT
  annual(sal) FROM emp`, `SELECT pkg.total() FROM dual`, named arguments with `=>`); it reads what
  the calling statement sees and cannot write, commit or roll back (`ORA-14552`). Calls nest at
  most 100 deep.
- **Packages**: a package's state (variables, open cursors) belongs to the session; the body's
  initialization runs once, at the first use in the session, and again after `CREATE OR
  REPLACE`. Public items are `pkg.item` outside the package and plain names inside it;
  subprograms can be overloaded; private items stay private; a package exception with
  `EXCEPTION_INIT` is caught by `WHEN pkg.e` or by its number.
- **`DBMS_OUTPUT`**: `PUT_LINE`, `PUT`, `NEW_LINE`, `ENABLE [(size)]`, `DISABLE` and `GET_LINE`. The
  lines are kept for the session and printed by `eggdb sql` after `SET SERVEROUTPUT ON` and by
  the Oracle shell; the embedded API reads them with `Session::take_server_output()` after
  `enable_server_output`. `ENABLE(n)` limits the buffer to `n` bytes (`ORU-10027` beyond), a line
  holds at most 32,767 bytes (`ORU-10028`).
- **Compile errors**: a unit is compiled at `CREATE`; one that does not compile is refused with
  `ORA-06550: line L, column C:` and the `PLS-` message (`PLS-00201: identifier 'X' must be
  declared`, `PLS-00103: Encountered the symbol ...`, `PLS-00363` for assigning an `IN`
  parameter), where Oracle would create it invalid. Wrong arguments in a call are `ORA-06553
  PLS-306`.
- **Dictionary**: `USER_SOURCE`, `USER_PROCEDURES`, `USER_ARGUMENTS`, `USER_TRIGGERS`,
  `USER_ERRORS` (always empty) and the PL/SQL objects in `USER_OBJECTS`, also as `ALL_` and
  `DBA_` views ([Known differences from Oracle](#known-differences-from-oracle)).
- **Limits** (D45): blocks nest at most 64 deep, a unit holds at most 1 MiB of text, a collection
  4,194,304 elements, a session 1,000 package states, a unit 1,000 open cursors; past a limit is
  an error, never a crash.

Not supported yet (`0A000`, or `ORA-06550` with the reason at `CREATE`): `GOTO`, `PRAGMA
AUTONOMOUS_TRANSACTION`, call specifications (`LANGUAGE`, `EXTERNAL`), user-defined aggregates,
`PIPE ROW` and pipelined functions in `TABLE(...)`, collections in SQL (`TABLE(v)`, `MEMBER OF`),
`OUT` binds in `EXECUTE IMMEDIATE ... USING`, `COMMIT` and `ROLLBACK` through `EXECUTE
IMMEDIATE`, object types (`CREATE TYPE`), compound triggers, and Oracle-supplied packages other
than `DBMS_OUTPUT` (`DBMS_SQL`, `UTL_FILE`, `DBMS_LOB`, ...).

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
  (the method and each column's order are kept and shown; a `hash` index is one column, not
  unique and without `ASC`, `DESC` or `NULLS`, as in PostgreSQL, `0A000` otherwise)
  and `DROP INDEX [IF EXISTS] name [, ...] [RESTRICT]`: secondary indexes, described under
  [Indexes](#indexes). An unnamed index is `table_columns_idx`.
- `EXPLAIN statement` for `SELECT`, `UPDATE` and `DELETE`: the plan as PostgreSQL prints it
  without costs (`Seq Scan on t`, `Index Scan using t_b on t`, `Index Scan Backward ...`,
  `Sort`, `Limit`, `Nested Loop`, `Hash Join`).
- `DROP TABLE [IF EXISTS] name [, ...] [CASCADE | RESTRICT]`: removes the definition and every
  row, in one transaction, and the sequences its columns own. A table another table's foreign
  key references, or whose owned sequence another table's default uses, is `2BP01` unless
  `CASCADE`, which drops those foreign keys or defaults (not the tables).
- `INSERT INTO name [(columns)] VALUES (...), (...) | DEFAULT VALUES | query [RETURNING items]`,
  with `DEFAULT` in a values list. `INSERT ... SELECT` (and `WITH ... SELECT`, `(SELECT ...)`,
  set operations, `VALUES` and `ORDER BY`) runs its query to the end first, so it reads the table
  as it was before the statement, then inserts the rows in the query's order through the path of
  `INSERT ... VALUES` (conversions, defaults, identity, constraints, `RETURNING`), in every
  dialect (D138).
- `VALUES (expr, ...), (expr, ...) [ORDER BY ...] [LIMIT ...] [OFFSET ...]` as a statement, an
  operand of `UNION`, `INTERSECT` and `EXCEPT`, a subquery (`x IN (VALUES (1), (2))`), a `WITH`
  query and a `FROM` item (`(VALUES (1, 'a')) AS v(n, s)`): its columns are `column1`,
  `column2`, ... and each takes one type by the `UNION` rules below; rows of different lengths
  are `42601` (D128).
- `SELECT [ALL | DISTINCT] items [FROM from_item [join ...]] [WHERE expr] [GROUP BY expr, ...] [HAVING expr] [ORDER BY expr [ASC | DESC] [NULLS FIRST | LAST], ...] [LIMIT n | ALL] [OFFSET n [ROW | ROWS]]`.
  `ORDER BY` takes an output column's number or alias, or any expression over the tables (with
  `DISTINCT`, only expressions in the select list, else `42P10`); a `SELECT` without `FROM`
  returns one row.
- **Functions in `FROM`** (D128): `generate_series(start, stop [, step])` over `smallint`,
  `integer` and `bigint` (a step of 0 is `22023`; `numeric` and time stamps answer `0A000`) and
  `unnest(array)`, also written `pg_catalog.generate_series` and `pg_catalog.unnest`, with
  `WITH ORDINALITY` (a `bigint` column `ordinality`) and `[AS] alias [(column, ...)]`; without
  a column list a one-column function's column takes the alias (`FROM generate_series(1, 3) s`
  makes `s` a column). The arguments are constants, parameters, the outer query's columns in a
  correlated subquery (the rows are made again for each outer value), or the columns of the
  items before it in the same `FROM` (PostgreSQL's implicit `LATERAL`, below); naming an item
  after it is `0A000`. A `VALUES` list in `FROM` may name the outer query's columns and the items
  before it too. Set-returning functions may also stand as items of a select list, several in
  lock-step (D131), each input row giving as many rows as the longest one; one inside an
  expression and one in `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY` or an aggregate answer `0A000`,
  and the `ORDER BY` of such a select takes output columns. The rows a function or `VALUES` list
  makes count against the statement's 1 GiB, so `generate_series(1, 10^12)` ends with `53200`.
- **`LATERAL`** (D151): `FROM a, LATERAL (SELECT ... a.x ...) s`, `a [LEFT] JOIN LATERAL (...) s
  ON cond`, `a CROSS JOIN LATERAL f(a.x)` and a function or `VALUES` list that names an earlier
  item without the word. The lateral item runs once for each distinct set of the earlier items'
  values it names (the earlier items, joined as written, give those sets) and its rows are joined
  back on those values, NULLs matching NULLs, so a `LEFT JOIN LATERAL` keeps the left rows it gave
  nothing to. A lateral item may hold subqueries of its own, correlated with it or with the items
  before it. `RIGHT` and `FULL JOIN LATERAL` naming an earlier item are `42P10`, as in
  PostgreSQL. Transact-SQL's `CROSS APPLY` and `OUTER APPLY` are lateral joins too.
- **Joins**: `a [INNER] JOIN b ON cond`, `a LEFT [OUTER] JOIN b ON cond`, `a RIGHT [OUTER] JOIN
  b ON cond`, `a FULL [OUTER] JOIN b ON cond`, `JOIN ... USING (columns)`,
  `a CROSS JOIN b` and `FROM a, b`, chained left to right, each table with an optional alias.
  A right join keeps every row of its right table, a full join every row of both; the rows of
  the right table that matched nothing come after the others, extended with NULLs. With
  `RIGHT JOIN ... USING`, the `USING` column is the right table's; with `FULL JOIN ... USING`
  it is `coalesce(left.c, right.c)`, so `*`, `GROUP BY c` and `ORDER BY c` see the value of
  whichever side had the row, as in PostgreSQL, and `left.c` and `right.c` stay apart.
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
  A subquery may name the columns of any query around it, however many levels out (a correlated
  subquery), in its own select list, `WHERE`, `HAVING` and `JOIN ... ON`, and stand in that
  query's select list, `WHERE`, `HAVING` or `ORDER BY`; in a grouped query it may name the
  `GROUP BY` columns and the columns they determine (otherwise `42803` "subquery uses ungrouped
  column"). It runs once for each distinct set of the outer values it names, read by key or index
  where its conditions allow, and the others run once per statement; an `EXISTS` whose outer
  references are only `inner = outer` conditions in `WHERE` on the query directly around it runs
  once, into a hash set the outer rows look up. `UPDATE` and `DELETE` take subqueries that name
  the changed row in `SET`, `WHERE` and `RETURNING`, all read before the first row changes;
  `INSERT ... VALUES` takes subqueries that stand alone.
- **`WITH` queries**: `WITH name [(columns)] AS [[NOT] MATERIALIZED] (query), ...` before a
  query, a subquery, an `INSERT`, an `UPDATE` or a `DELETE`, each name visible to the ones after
  it and to the statement; every `WITH` query is run once and its rows kept for the statement,
  however often it is named. At the top of a statement a `WITH` query may also be an `INSERT`,
  `UPDATE` or `DELETE` with `RETURNING` (`WITH moved AS (DELETE FROM a RETURNING *) INSERT INTO
  b SELECT * FROM moved`, D151): each runs once, in order, before the statement, and the
  statement reads what it returned; naming one without `RETURNING` and one inside a subquery
  are `0A000`. They run one after the other on the same data, so the statement and later `WITH`
  queries see their changes, where PostgreSQL runs them all on the same snapshot.
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
- **Aggregates**: `count(*)`, `count(expr)`, `sum`, `avg`, `min`, `max`,
  `string_agg(text, delimiter)` (also over `bytea`; a NULL delimiter joins with nothing),
  `json_agg`, `jsonb_agg`, `json_object_agg(key, value)`, `jsonb_object_agg(key, value)`,
  `array_agg` and, in the PostgreSQL dialect, `bool_and`, `bool_or` and `every` (D131), each
  with `DISTINCT` or `ALL` and an `ORDER BY` inside the call (`array_agg(x ORDER BY y DESC
  NULLS LAST)`, `string_agg(s, ',' ORDER BY s)`) that orders the values it combines. They skip
  NULLs, except the JSON aggregates, which keep them as JSON `null` (a NULL key is `22004`), and
  `array_agg`, which keeps them as NULL elements; `count` of nothing is 0, the others are NULL.
  `array_agg` makes an array of its argument's type (`integer[]`, `text[]`, ...) for the
  element types arrays take; `bool_and`, `bool_or` and `every` take `boolean` only (`42883`). Result types are
  PostgreSQL's: `count` is `bigint`; `sum` of `smallint` or `integer` is `bigint`, of `bigint`
  or `numeric` is an exact `numeric`, of `real` is `real`, of `double precision` is `double
  precision`; `avg` of an integer or `numeric` is an exact `numeric` (divided at PostgreSQL's
  scale, `avg` of 1 and 2 is `1.5000000000000000`), of a float `double precision`; `min` and
  `max` keep their argument's type and order NaN above every number, as `ORDER BY` does. `sum`
  and `avg` of text, and `min` and `max` of booleans and `json`, fail with `42883`, as there.
  Every aggregate takes `FILTER (WHERE condition)`, which feeds it only the rows where the
  condition is true (`count(*) FILTER (WHERE x > 1)`, D151).
- **Ordered-set aggregates** (D151): `percentile_cont(fraction) WITHIN GROUP (ORDER BY expr)`
  (`double precision`, interpolated between the two nearest values), `percentile_disc(fraction)
  WITHIN GROUP (ORDER BY expr)` (the first value whose position reaches the fraction, in the
  argument's type) and `mode() WITHIN GROUP (ORDER BY expr)` (the most frequent value, the first
  in that order on a tie), skipping NULLs; a fraction outside 0 to 1 is `22003`. Also in Oracle
  and Transact-SQL as aggregates.
- **Window functions** (D151): `function(...) OVER (window)` and `OVER name` with `WINDOW name AS
  (window), ...` after `HAVING`, where a window is `[existing name] [PARTITION BY expr, ...]
  [ORDER BY ...] [frame]` and the frame is `{ROWS | RANGE | GROUPS} {start | BETWEEN start AND
  end} [EXCLUDE {CURRENT ROW | GROUP | TIES | NO OTHERS}]` with `UNBOUNDED PRECEDING`, `n
  PRECEDING`, `CURRENT ROW`, `n FOLLOWING` and `UNBOUNDED FOLLOWING`. The functions are
  `row_number`, `rank`, `dense_rank` (`bigint`), `percent_rank`, `cume_dist` (`double
  precision`), `ntile(n)` (`integer`), `lag` and `lead(value [, offset [, default]])`,
  `first_value`, `last_value`, `nth_value(value, n)`, and every aggregate above with `FILTER`.
  Without `ORDER BY` the frame is the whole partition; with it, the default frame runs from the
  partition's start to the current row's last peer, as in PostgreSQL. `RANGE` with an offset
  needs exactly one `ORDER BY` column (numbers, or dates and time stamps with an `interval`);
  `GROUPS` needs an `ORDER BY`; offsets are constants or parameters, never negative or NULL
  (`22013`, `22004`). Window calls stand anywhere in the select list, inside expressions
  (`amount * 100 / sum(amount) OVER (PARTITION BY region)`), and in `ORDER BY`, and run after
  `WHERE`, `GROUP BY` and `HAVING` (so `rank() OVER (ORDER BY sum(x))` ranks groups) and before
  `DISTINCT`, `ORDER BY` and `LIMIT`; beside set-returning functions they are computed first. In
  `WHERE`, `GROUP BY`, `HAVING`, `JOIN ... ON` or `LIMIT` they are `42P20`, nested ones are
  `42P20`, and one inside an aggregate is `42803`, as PostgreSQL reports them. MySQL, SQLite,
  Oracle and Transact-SQL take the same syntax.
- `UPDATE name [[AS] alias] SET column = expr | DEFAULT, ... [WHERE expr] [RETURNING items]`.
  Changing a primary key moves the row; the uniqueness check runs after every row moved, so
  `UPDATE t SET id = id + 1` succeeds (PostgreSQL may refuse it, depending on the order it
  visits rows).
- `DELETE FROM name [[AS] alias] [WHERE expr] [RETURNING items]`.
- `BEGIN [WORK | TRANSACTION] [READ ONLY | READ WRITE] [ISOLATION LEVEL ...]`,
  `START TRANSACTION`, `COMMIT`/`END`, `ROLLBACK`/`ABORT`. `SET SESSION CHARACTERISTICS AS
  TRANSACTION ...` and `SET TRANSACTION ...` take every isolation level, `[NOT] DEFERRABLE` and
  `READ WRITE` (D131): every transaction runs serializable, so the level changes nothing, as
  `SHOW transaction_isolation` says; `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY` sets
  `default_transaction_read_only`. `SET TRANSACTION READ ONLY` and `SET TRANSACTION SNAPSHOT`
  are `0A000`.
- `SET [SESSION] name {TO | =} value | DEFAULT`, `SET TIME ZONE`, `SET NAMES`, `RESET name | ALL`,
  `SHOW name | ALL`: PostgreSQL's run-time parameters (D104). `client_encoding` takes `UTF8`
  (and `SQL_ASCII`, which passes bytes through) only; settings EggDB does not apply yet, such as `statement_timeout` and `search_path`, are
  remembered and shown; `SET LOCAL` is not supported yet. `max_identifier_length`,
  `max_index_keys` and `max_function_args` are fixed, as PostgreSQL's server limits.
- `$1`, `$2`, …: parameters, given through the PostgreSQL protocol's extended query messages
  (D104); outside it, `$1` is `42P02` (there is no parameter `$1`).
- `version()`, `current_user`, `session_user`, `user`, `current_database()`, `current_schema`
  and `current_setting('name')`.
- `serial`, `bigserial` and `smallserial` (`serial4`, `serial8`, `serial2`) columns, and
  `GENERATED ALWAYS AS IDENTITY` and `GENERATED BY DEFAULT AS IDENTITY` on a `smallint`,
  `integer` or `bigint` column: the column takes the next number when an `INSERT` leaves it out
  (D111). A `GENERATED ALWAYS` column refuses a value: `INSERT` with one is `428C9 cannot insert
  a non-DEFAULT value into column "id"` unless it says `OVERRIDING SYSTEM VALUE`, and `UPDATE`
  may only set it to `DEFAULT` (`428C9`); `COPY` and bulk loads keep the values they bring, as in
  PostgreSQL (D146). The column must be the first column of the primary key; a named sequence and
  `DEFAULT nextval('s')` have no such limit. Identity options in `CREATE TABLE` (`GENERATED ...
  AS IDENTITY (START WITH 100 INCREMENT BY 5)`) are kept; `MINVALUE`, `MAXVALUE` and `CYCLE`
  that end the identity before its type's range answer `0A000`. `ALTER COLUMN c SET GENERATED
  {ALWAYS | BY DEFAULT}`, `SET INCREMENT BY`, `SET START WITH`, `RESTART [WITH n]` (on an identity
  backed by a sequence) and `DROP IDENTITY [IF EXISTS]` change it later (`55000` on a column that
  is not an identity). Generated columns (`GENERATED ALWAYS AS (expr) STORED`) answer `0A000`. `ALTER TABLE t
  ALTER COLUMN c ADD GENERATED {ALWAYS | BY DEFAULT} AS IDENTITY [(SEQUENCE NAME s START WITH
  ...)]`, as `pg_dump` writes identity columns, makes a sequence owned by the column (`s`, or
  `t_c_seq`) and the default `nextval('s'::regclass)`, so `setval('s', ...)` works and the column
  has none of the auto-increment column's limits, and the column stays an identity column of its
  kind (`ALWAYS` or `BY DEFAULT`), so `ALTER COLUMN ... SET DEFAULT` and `DROP DEFAULT` on it are
  `42601` as in PostgreSQL. `INSERT ... OVERRIDING SYSTEM VALUE` keeps the given values, and
  `OVERRIDING USER VALUE` ignores the values given for identity columns and numbers them (D118,
  D146).
- `CREATE UNLOGGED TABLE` makes an ordinary, logged table (D118).
- **Temporary tables and views** (D151): `CREATE TEMP | TEMPORARY TABLE name (...) [ON COMMIT
  PRESERVE ROWS | DELETE ROWS | DROP]` and `CREATE TEMP VIEW`, in schema `pg_temp`, seen only by
  the session that made them and gone when it closes; an unqualified name finds a temporary
  relation before the search path, so one may hide an ordinary table of the same name, and a
  view that reads a temporary relation becomes temporary itself (`NOTICE: view "v" will be a
  temporary view`). They have keys, indexes, constraints and sequences like any table, take part
  in the session's transactions (a `ROLLBACK` undoes their changes too), and `DISCARD TEMP` or
  `DISCARD ALL` drops them all. They are held in the session's memory, at most 1 GiB per session
  (`53200`), never written to the database file, so they cost no disk writes; changing one still
  takes the database's write transaction, so a read-only file cannot have them. A temporary table
  cannot reference an ordinary one with a foreign key, nor the other way round (`42P16`), nor
  move to another schema (`0A000`). `pg_class` and the other catalogs do not list them yet.
  MySQL's `CREATE TEMPORARY TABLE` and `DROP TEMPORARY TABLE`, SQLite's `CREATE TEMP TABLE` and
  `temp.name`, and Transact-SQL's `#name` tables are the same session tables.
- `x = ANY (ARRAY[a, b])`, `x <> ALL (ARRAY[a, b])` and the other comparisons with `ANY`, `SOME`
  or `ALL` over a constant `ARRAY[...]` list, also in parentheses and with a cast such as
  `::text[]` (how `pg_dump` writes the `IN` lists of `CHECK` constraints): `= ANY` is `IN`, `<> ALL`
  is `NOT IN`, and the others compare with each item in three-valued logic (D118). Any other
  array works too (`x = ANY ('{1,2}'::int[])`, `x <> ALL (ARRAY(SELECT ...))`, an array value of
  a derived table): `ANY` over no elements is false, `ALL` true, a NULL array gives NULL, and a
  value that is not an array is `42809` (D128).

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
`microseconds` to `millennium`. `TimeZone` takes `UTC` (and its aliases such as `Etc/UTC`),
fixed offsets written as POSIX does (`UTC+3`, `EST5`, `+05:30`: the sign means west of
Greenwich, as in PostgreSQL), a plain number of hours east (`SET TIME ZONE -7`), and every region
and alias of the IANA time zone database in any letter case (`Europe/Paris`, `us/eastern`,
`Asia/Kolkata`; shown as the database spells it), and refuses anything else with `22023` (D153).
A region's offset follows its daylight saving time and its history (`1900-01-01 00:00:00+00:09:21`
in Paris): `timestamptz` values print with the offset in force at that instant, a local time
that a spring-forward gap skips takes the offset from before the change (`2026-03-29 02:30` in
Paris is `03:30:00+02`) and one that a fall-back change repeats the offset from after it, as
PostgreSQL documents; `timestamptz ± interval` adds months and days on the local calendar and
the time part as elapsed time (`+ '1 day'` keeps the clock time across a change, `+ '24
hours'` does not). Input may name a region (`2026-07-01 12:00:00 Europe/Paris`), and
`pg_timezone_names` lists every region with its abbreviation, offset and daylight saving flag
now. The database is IANA's 2026e release, compiled from its public-domain source when a region
is first named.

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
or a time work. `make_date` (a negative year is BC), `make_time`, `make_timestamp`,
`make_timestamptz`, `make_interval` (also with PostgreSQL's named arguments, `make_interval(days
=> 10)`), `date_bin`, `isfinite` and `value AT TIME ZONE zone` (a `timestamp` read in the zone
becomes a `timestamptz`, a `timestamptz` becomes the zone's local `timestamp`, a `time` or
`timetz` becomes a `timetz`; the zone is `UTC`, a fixed offset written as POSIX does, a region
such as `America/New_York`, or an `interval`) work as in PostgreSQL; an unknown zone is `22023`.
`to_char` takes PostgreSQL's template patterns for dates, time stamps and
intervals (`YYYY`, `MM`, `Month`, `Mon`, `DD`, `Day`, `HH24`, `MI`, `SS`, `MS`, `US`, `AM`,
`IYYY`, `IW`, `J`, `Q`, `RM`, `TZ`, `OF`, `FM`, `TH` and the rest, with quoted text) and numbers
(`9`, `0`, `.`, `,`, `D`, `G`, `S`, `MI`, `PL`, `SG`, `PR`, `RN`, `TH`, `V`, `EEEE`, `FM`);
`to_date`, `to_timestamp(text, template)` and `to_number` read text by the same patterns, and
`to_timestamp(seconds)` reads Unix time.

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
`json_array_length`/`jsonb_array_length` work as in PostgreSQL. So do `jsonb`'s `?`, `?|`,
`?&`, `-` (a key, an array index or a `text[]` of keys), `#-` (a path) and `||` (objects merged,
arrays joined, scalars wrapped); the builders `to_json`/`to_jsonb`, `row_to_json` (of a `FROM`
item, `row_to_json(t)`, or of `row(...)`), `json_build_object`/`jsonb_build_object` and
`json_build_array`/`jsonb_build_array`, written with PostgreSQL's spacing (`{"a" : 1}` for
`json`, normalized for `jsonb`; dates and time stamps in ISO 8601 with a `T`); the aggregates
`json_agg`/`jsonb_agg` (NULLs kept, a `FROM` item as an object, `json_agg(t)`) and
`json_object_agg`/`jsonb_object_agg`; `jsonb_set`, `jsonb_insert`, `jsonb_strip_nulls`,
`json_strip_nulls`, `jsonb_pretty` and `json(b)_extract_path(_text)`; and the set-returning
`json(b)_each(_text)`, `json(b)_array_elements(_text)` and `json(b)_object_keys` in `FROM`
(columns `key` and `value`) or alone in a select list (each row then as its record text,
`(a,1)`). `jsonb` values compare, group
and index by their normalized text with every number written without trailing zeros, so
`'{"a": 1.0}'::jsonb = '{"a": 1}'::jsonb` is true and a unique index holds them once, while each
value keeps the number as written (`1.0`); `json` has no equality, as in PostgreSQL (`42883`),
and cannot be indexed or be a key (`42704`). Stored `json` and `jsonb` are parsed again when a
row is read, so a damaged one is `XX001`.

**Arrays** (D128) are values of expressions, of `WITH` queries, derived tables and `VALUES` lists,
and of results, with one dimension and elements of `smallint`, `integer`, `bigint`, `real`,
`double precision`, `numeric`, `boolean`, `text`, `varchar` or `character`: `ARRAY[1, 2]`
(elements of the common type; `ARRAY[]` needs a cast, `42P18`), `ARRAY(SELECT ...)` (one
column, `42601` otherwise), PostgreSQL's literals (`'{1,2,NULL}'::int[]`, `'{"a b",c}'::text[]`,
with quotes, backslashes and `NULL`; a malformed one is `22P02` with PostgreSQL's detail), casts
between array types element by element (an element type's length or scale applies to each
element: `ARRAY['abc']::varchar(1)[]` is `{a}`, `ARRAY[1]::numeric(5,2)[]` is `{1.00}`) and to
and from text, arrays of different element types in `UNION` and `VALUES` (of the element
types' common type, so `ARRAY[1]` and `ARRAY[1.5]` meet as `numeric[]`), `a[i]` (NULL outside the array;
a subscript outside the `integer` range is `22003`), `||` between two arrays or an array and an
element, of the element types' common type as `UNION` finds it (`ARRAY[1, 2] || 3.5` is
`{1,2,3.5}`, a `numeric[]`; text types keep the first operand's type; `integer[] || boolean` is
`42883`), where a NULL array, typed or not, counts as empty (`NULL::int[] || 1` is `{1}`, two NULL
arrays give NULL),
`= <> < <= > >=` element by element (NULL elements equal each other and sort last; two arrays of
different element types are `42883`), `IS [NULL]` and the functions `array_to_string(a, sep [,
null_text])`, `array_length(a, 1)`, `array_upper`, `array_lower`, `array_ndims` and
`cardinality`. They print as PostgreSQL prints them (`{1,2}`, `{a,"b c",NULL,"\"q\""}`) and travel
with PostgreSQL's array type OIDs in its text and binary forms. Array columns (`text[]`, `integer
ARRAY`, in `CREATE TABLE`, `ALTER TABLE ... ADD COLUMN` and views) store one-dimensional arrays
of every element type above; arrays of `name`, `oid`, `"char"`, `aclitem`, `int2vector` and
`oidvector` stay catalog-only (`0A000` as a column). An array given to a text column is stored as
its text. Multidimensional arrays, slices (`a[1:2]`), bounds in literals
(`[1:2]={...}`), arrays of other element types and array parameters answer `0A000`. An array may
hold at most 2^27 elements (`54000`) and counts against the session's value limit.

**Expressions**
- Arithmetic `+ - * / % ^`, unary `-` and `+`, string concatenation `||`.
- Regular expressions (D128, D130): `text ~ pattern`, `~*` (ignoring case), `!~` and `!~*`, with
  PostgreSQL's advanced syntax: `.`, bracket expressions (`[a-z]`, `[^0-9]`, `[[:alpha:]]` and the
  other classes, `\d \w \s \D \W \S` inside brackets too, collating elements such as
  `[[.hyphen.]]` and `[[=a=]]` by POSIX's names for the portable characters), `\d \w \s \D \W \S`,
  character escapes (`\n`, `\t`, `\x41`, `\u00e9`), `^`
  and `$`, groups `(...)` and `(?:...)`, `|`, and `* + ? {m} {m,} {m,n}` with their lazy forms;
  back references (`(a)\1`), lookahead and lookbehind (`(?=re)`, `(?!re)`, `(?<=re)`,
  `(?<!re)`), the constraint escapes `\m \M \y \Y \A \Z`, embedded options at the start
  (`(?i)`, `(?c)`, `(?n)`, `(?m)`, `(?p)`, `(?w)`, `(?s)`, `(?x)`, `(?t)`, `(?q)`) and the
  `***=` and `***:` directors. A pattern without back references or lookaround is matched in
  time linear in the text; one with them runs a backtracking search bounded by the same step
  budget. A malformed pattern is `2201B` with PostgreSQL's reason (`invalid regular expression:
  parentheses () not balanced`), the basic and extended flavours (options `b` and `e`) are
  `0A000`, and one match may take at most 10^9 steps (`54000`); a statement may run any number
  of matches. Classes and `~*` follow the `C` locale, as text comparison does: only ASCII
  letters change case.
- The regular expression functions (D130): `regexp_match`, `regexp_matches` (a set-returning
  function, every match with the `g` flag), `regexp_replace(s, re, replacement [, start [, n]]
  [, flags])` with `\1`..`\9`, `\&` and `\\` in the replacement, `regexp_split_to_table`,
  `regexp_split_to_array`, `regexp_like`, `regexp_count`, `regexp_instr` and `regexp_substr`, with
  PostgreSQL's flags (`g`, `i`, `c`, `n`, `m`, `p`, `w`, `s`, `x`, `t`, `q`); a match starts as
  far left as it can and is the longest or the shortest there by the pattern's own preference, as
  PostgreSQL's rules describe (`regexp_match('abcd', 'a|ab')` is `{ab}`). `substring(s FROM re)`
  gives the first parenthesized part, `substring(s SIMILAR re ESCAPE e)` and `substring(s FROM re
  FOR e)` the part between the `e"` separators, and `s [NOT] SIMILAR TO re [ESCAPE e]` matches
  the SQL standard's patterns (`%`, `_`, `|`, `*`, `+`, `?`, `{m,n}`, `()`, brackets), through
  `similar_to_escape`.
- `OPERATOR(op)` and `OPERATOR(pg_catalog.op)` for every binary operator, as psql writes them,
  with PostgreSQL's precedence; `expr COLLATE "C"`, `"POSIX"`, `"default"` or `ucs_basic`
  (optionally `pg_catalog.`), which changes nothing since text compares bytewise as in `C` (any
  other collation is `42704`, a non-text operand `42804`); and type names written
  `pg_catalog.int4` (another schema is `42704`).
- Comparisons `= <> != < <= > >=`, `BETWEEN`, `IN (list)`, `LIKE` and `ILIKE` with `%`, `_`
  and `\` escapes, `IS [NOT] NULL`, `ISNULL`, `NOTNULL`, `IS [NOT] TRUE | FALSE | UNKNOWN`,
  `IS [NOT] DISTINCT FROM`, each with its `NOT` form.
- `AND`, `OR`, `NOT` with SQL's three-valued logic: `NULL AND false` is false, `NULL OR true`
  is true, anything else with NULL is NULL, and `WHERE` keeps only rows where the condition is
  true.
- `CAST(expr AS type)`, `expr::type` and `type 'literal'`.
- `CASE [operand] WHEN ... THEN ... [ELSE ...] END`.
- Functions `coalesce`, `nullif`, `abs`, `length` (`char_length`, `character_length`),
  `octet_length`, `lower`, `upper`, `substring(s FROM start FOR count)` (or with commas),
  `trim([LEADING | TRAILING | BOTH] [characters] FROM s)` (and `trim(s [, characters])`),
  `ltrim`, `rtrim`, `btrim` and `quote_ident` (quoting what PostgreSQL 16 quotes, by its key word
  list).
- String functions (item 36): `split_part` (a negative field counts from the end), `concat` and
  `concat_ws` (skipping NULLs), `replace`, `position(sub IN s)` and `strpos`, `left` and `right`
  (a negative count drops from the other end), `lpad` and `rpad`, `repeat`, `initcap`,
  `translate`, `reverse`, `chr` and `ascii`, `md5` (RFC 1321, of text or `bytea`),
  `starts_with`, `overlay(s PLACING r FROM start [FOR count])`, `substr`, `quote_literal`,
  `quote_nullable` and `format` with `%s`, `%I`, `%L`, `%%`, argument positions (`%2$s`) and
  widths (`%-10s`, `%*s`). Every result that can grow (`repeat`, `lpad`, `rpad`, `replace`,
  `format`, `concat`, `regexp_replace`) checks the session's value limit (D119) before it is
  built, and one past 1 GiB is `54000` (`requested length too large`). Every session has
  EggDB's own limits of 1 GiB for one value a statement builds and 2 GiB for one row (D156); a
  session may set lower ones (MCP calls: 16 MiB and 64 MiB) but never higher ones.
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

## Transactions, savepoints and locks
- **Savepoints** work in every dialect: `SAVEPOINT name`, `ROLLBACK TO [SAVEPOINT] name` and
  `RELEASE [SAVEPOINT] name` (PostgreSQL, MySQL, SQLite, Oracle), and `SAVE TRANSACTION name` with
  `ROLLBACK TRANSACTION name` (SQL Server). Rolling back to a savepoint undoes the changes made after
  it and keeps it. In PostgreSQL's dialect a failed statement after a savepoint leaves the block
  aborted until `ROLLBACK TO` brings it back; without savepoints the block is rolled back as
  before. In SQLite's, a `SAVEPOINT` outside a transaction begins one, and releasing it commits.
  A name that was never set fails with each dialect's error (`3B001`, MySQL 1305, ORA-01086,
  SQL Server 6401).
- **Row locks.** `SELECT ... FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE`, `FOR KEY SHARE` with
  `OF`, `NOWAIT`, `SKIP LOCKED` (and Oracle's `WAIT n`, MySQL's `LOCK IN SHARE MODE`) and
  `LOCK TABLE ... IN ... MODE [NOWAIT]` are accepted. A transaction block holds the database's
  one writer from `BEGIN` on, so no other session changes the rows it read, and the lock
  clauses need nothing more; they will take part when SQL sessions run on concurrent
  transactions (D172).
- **Isolation levels.** `SET TRANSACTION`, `BEGIN ISOLATION LEVEL` and each dialect's forms are
  accepted. Every block runs serializably today, which meets every level's guarantees.

## LISTEN and NOTIFY

Sessions of `eggdb serve` can signal each other as PostgreSQL's do: `LISTEN channel`,
`UNLISTEN channel` (or `*`), `NOTIFY channel, 'payload'` and `pg_notify(channel, payload)`.
Notifications are delivered when the sending transaction commits (and dropped when it rolls
back), once per distinct channel and payload in a transaction, to every session listening on the
channel, the sender included. A listener receives them before its next `ReadyForQuery`, and while
it sits idle within a fifth of a second, so psql prints them and drivers' notification APIs see
them. Channels are up to 63 bytes and payloads under 8,000 bytes, as in PostgreSQL.

## Roles and privileges

Superusers (embedded use and the server's own user) can create roles and grant them table
privileges; a role that logs in through `eggdb serve` then reads and writes only what it holds
(D183):

```sql
CREATE ROLE readers;
CREATE USER clerk WITH PASSWORD 'secret' IN ROLE readers;
GRANT SELECT ON accounts TO readers;
GRANT INSERT, UPDATE ON accounts TO clerk;
REVOKE readers FROM clerk;
```

`psql -U clerk` then logs in with SCRAM-SHA-256 against the stored verifier, and a statement on a
table the role holds no privilege for fails with `42501 permission denied for table audit`. Roles
are listed in `pg_roles`. Not yet: logins of stored roles over MySQL's and SQL Server's protocols,
column and schema privileges, object owners and `SET ROLE`.

Roles can also log in through a directory or an identity provider (D185). Give `eggdb serve
--auth-rules auth.conf` a file such as:

```
# user   method  settings
alice    ldap    server=ldap.example.org:389 prefix=uid= suffix=,ou=people,dc=example,dc=org
all      jwt     jwks=provider-keys.json issuer=https://login.example.org audience=eggdb user_claim=preferred_username
mallory  reject
```

The first line naming a user (or `all`) decides; users no line names use their stored password.
LDAP checks the password with a simple bind; `jwt` takes an OpenID Connect token as the password
and checks its signature against the key set file, its issuer, audience and expiry, and that it
names the user. Both need an encrypted connection (`sslmode=require`) or the local socket, and the
user must exist as a role with `LOGIN`.

## Statistics: `ANALYZE` and approximate counts
- `ANALYZE [table, ...]` (PostgreSQL and SQLite), `ANALYZE TABLE t, ...` (MySQL, which answers
  with its `Table | Op | Msg_type | Msg_text` rows) and Oracle's `ANALYZE TABLE t COMPUTE
  STATISTICS` count each table's rows. The count is kept in the catalog, shown as
  `pg_class.reltuples`, and dropped or moved with the table.
- `approx_count_distinct(expr)`, in every dialect, estimates the number of distinct non-NULL
  values with a HyperLogLog sketch of 4,096 registers: within a few percent, in 4 KiB per group,
  however many rows there are.
- Next: column statistics (`pg_stats`), joins ordered by cost from them, and `TABLESAMPLE`.

## Partitioned tables
A table can be split into partitions by range, list or hash, as PostgreSQL does:

```sql
CREATE TABLE measurements (id integer NOT NULL, at date NOT NULL, v integer,
                           PRIMARY KEY (id, at)) PARTITION BY RANGE (at);
CREATE TABLE measurements_2025 PARTITION OF measurements
    FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
CREATE TABLE measurements_rest PARTITION OF measurements DEFAULT;
ALTER TABLE measurements ATTACH PARTITION old_rows FOR VALUES FROM (MINVALUE) TO ('2025-01-01');
ALTER TABLE measurements DETACH PARTITION old_rows;
```

- **Strategies.** `RANGE` over one or more columns (`FROM` inclusive, `TO` exclusive, `MINVALUE`
  and `MAXVALUE`), `LIST` over one column (`NULL` may be listed), `HASH` over one or more columns
  (`MODULUS` and `REMAINDER`), and a `DEFAULT` partition for rows no other partition takes. A
  partition may itself be partitioned. Partition keys are columns; expressions in a key are
  refused for now.
- **Rows go where they belong.** `INSERT`, `COPY` and `INSERT ... SELECT` into the partitioned
  table route each row to its partition. A row no partition takes fails with `23514`, `no
  partition of relation ... found for row`, and a row written straight into a partition must
  fit its bound (`violates partition constraint`). `UPDATE`, `DELETE` and `TRUNCATE` on the
  partitioned table reach every partition. An `UPDATE` that would move a row to another
  partition is refused for now.
- **Pruning.** A query reads only the partitions its `WHERE` can match: comparisons, `BETWEEN`,
  `IN` and `IS NULL` on the key against constants. `EXPLAIN` lists the partitions read; when one
  is left, the query reads it directly and uses its indexes.
- **Constraints.** A primary key or unique constraint of a partitioned table must include every
  key column, and each partition gets its own. `PARTITION OF` copies the parent's columns,
  defaults, primary key, unique constraints and checks. `AUTO_INCREMENT` and identity columns are
  refused in partitioned tables for now; `serial` columns share their sequence.
- **Catalogs and restores.** `pg_class` shows `relkind` `p` and `relispartition`, `pg_inherits`
  links partitions to their parent, `pg_partitioned_table`, `pg_get_partkeydef` and
  `pg_get_expr(relpartbound, oid)` describe them, and `pg_dump` files restore with their
  partitions (`ATTACH PARTITION`).
- **MySQL and Oracle.** `PARTITION BY RANGE|LIST|HASH|KEY (...)` with named partitions inside
  `CREATE TABLE` (`VALUES LESS THAN`, `VALUES IN`, `PARTITIONS n`, Oracle's `VALUES (DEFAULT)`)
  makes one partition per name, stored as a table named `<table>#P#<partition>`.
- **Hash values.** Hash partitions assign rows by EggDB's own hash of the key, not PostgreSQL's,
  so a `COPY` of a hash partition's rows from a `pg_dump` is routed through the partitioned
  table, as `pg_dump --load-via-partition-root` would.

## User-defined types and domains

In the PostgreSQL dialect (D152), as PostgreSQL's manual describes them:

- **Enums**: `CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')`. Values compare and sort in the
  order the labels were listed; a label not in the list is `22P02 invalid input value for enum
  mood: "x"`. `ALTER TYPE mood ADD VALUE [IF NOT EXISTS] 'meh' [BEFORE | AFTER 'ok']` places a
  label without moving the others (stored rows keep their values), `RENAME VALUE 'a' TO 'b'`
  renames one; `enum_first`, `enum_last` and `enum_range` answer as PostgreSQL's. At most 100,000
  labels of at most 63 bytes.
- **Composite types**: `CREATE TYPE complex AS (r double precision, i double precision)`. Values
  are built with `ROW(1.5, -2)` or `(1.5, -2)` with more than one field, read from text as
  `'(1.5,-2)'`, print as PostgreSQL prints them (`(1,"a b",)`), and give a field with
  `(value).field` (`42703` for a field the type does not have). A `record` value (an anonymous
  row) can be selected and compared, not stored (`42P16 column cannot be of type record`).
- **Range types**: the built-in `int4range`, `int8range`, `numrange`, `daterange`, `tsrange` and
  `tstzrange`, and `CREATE TYPE r AS RANGE (subtype = float8)` (`subtype_diff`,
  `multirange_type_name`, `collation` and `subtype_opclass` accepted; `canonical` is `0A000`).
  Constructors `int4range(1, 5 [, '[]'])`, literals `'[1,5)'`, `'empty'`, unbounded sides; discrete
  ranges take PostgreSQL's canonical `[)` form; a lower bound above the upper is `22000`. Operators
  `@>`, `<@`, `&&`, `<<`, `>>`, `&<`, `&>`, `-|-`, `+`, `*`, `-`, and `lower`, `upper`,
  `isempty`, `lower_inc`, `upper_inc`, `lower_inf`, `upper_inf`.
- **Domains**: `CREATE DOMAIN posint AS integer [DEFAULT expr] [NOT NULL] [CONSTRAINT name] CHECK
  (VALUE > 0) ...`. A column of a domain stores its base type and checks the domain's `NOT NULL`
  (`23502 domain posint does not allow null values`) and checks (`23514 value for domain posint
  violates check constraint "posint_check"`) on every insert, update, `COPY` and cast; the
  domain's default fills a column that has none of its own. `ALTER DOMAIN` takes `SET DEFAULT`,
  `DROP DEFAULT`, `SET NOT NULL`, `DROP NOT NULL`, `ADD CONSTRAINT ... [NOT VALID]` (existing
  columns are checked unless `NOT VALID`), `VALIDATE CONSTRAINT`, `DROP CONSTRAINT [IF EXISTS]`,
  `RENAME CONSTRAINT`, `RENAME TO` and `SET SCHEMA`. A domain may be over another domain.
- `DROP TYPE` / `DROP DOMAIN [IF EXISTS] name, ... [CASCADE]`: a type used by a column or by
  another type is `2BP01`; `CASCADE` drops the columns that use it. `ALTER TYPE ... RENAME TO`,
  `SET SCHEMA` and `OWNER TO` work on every kind.
- Types are schema objects: they resolve through `search_path`, may be written `schema.name`,
  and share their names with tables (`42710 type "t" already exists`). `pg_type` lists them
  (`typtype` `e`, `c`, `r`, `d`, the domain's base type, `NOT NULL` and default), `pg_enum` lists
  the labels with their sort order, `pg_range` the ranges, `format_type` and
  `pg_type_is_visible` know them, and `information_schema.columns` names a column's domain
  (`domain_name`) and its type (`udt_name`).

Not yet: arrays of enums, composites and ranges, composite types as attributes of a domain or
another composite type's array, multiranges, a table's own row type as a value (`SELECT t FROM
t`), typed tables (`CREATE TABLE ... OF type`), `ALTER TYPE ... ADD/DROP/ALTER ATTRIBUTE`, and
keys or indexes on array, composite or range columns (`0A000`; enum columns may be keys and
indexed, in label order).

## Indexes

A secondary index keeps one entry per row: the indexed columns' values in the same
order-preserving encoding as primary keys, then the row's primary key (or hidden row number),
so equal values sort in primary key order (D109). Entries are written in the same transaction
as their rows by every `INSERT` (with `IGNORE`, `REPLACE` and upserts), `UPDATE`, `DELETE` and
`TRUNCATE`, so a crash or a rollback never leaves an index that disagrees with its table.
`CREATE INDEX` on a table that has rows fills the index in the same transaction, a thousand rows
at a time within the statement's 1 GiB work memory; `DROP INDEX` and `DROP TABLE` remove the
entries.

**Method and order.** An index remembers what made it: its access method (`USING hash` in the
PostgreSQL dialect), each column's `ASC` or `DESC` and `NULLS FIRST` or `NULLS LAST`, and
whether a unique index backs a `UNIQUE` constraint or was made by `CREATE UNIQUE INDEX`. The
catalogs show them (psql's `\d`, MySQL's `SHOW INDEX` collation `D`, Oracle's `DESCEND`, SQL
Server's `is_descending_key`), `.schema` writes them back, and in PostgreSQL's dialect
`ALTER TABLE ... DROP CONSTRAINT` finds only constraints, as there. The entries are stored the
same way whatever the method or order: every index keeps its values in ascending order with
NULLs last, and a query reads it forward or backward as the plan needs, so results never depend
on them; a `hash` index is used for ranges and order as a `btree` one would be (D146).

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
`'name'::regclass` a `regclass` naming the relation (`42P01` when there is none, see
[The PostgreSQL catalog](#the-postgresql-catalog)); `pg_catalog.nextval(...)` and the other `pg_catalog.`
spellings work. Past its limit a sequence fails with `2200H` (`nextval: reached maximum value of
sequence "s" (9223372036854775807)`) or, with `CYCLE`, starts again; arithmetic never wraps.
`nextval` is not undone by a rollback, as in PostgreSQL, and is durable with the commit of any
transaction that used the value. Oracle's sequences are the same objects: `CREATE SEQUENCE s
START WITH 1 INCREMENT BY 1 NOCACHE NOCYCLE` (with `NOMAXVALUE`, `NOMINVALUE`, `ORDER` and the
rest accepted), `s.NEXTVAL` and `s.CURRVAL`, with ORA-02289, ORA-08002 and ORA-08004. SQL
Server's are too: `CREATE SEQUENCE` with SQL Server's defaults and `NEXT VALUE FOR s` (see the
Transact-SQL section). MySQL and SQLite have no sequences, and `nextval` there is an unknown function. A column `DEFAULT
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
checked when the statement ends, so a multi-row `INSERT` may insert a child before its parent.
A foreign key declared `DEFERRABLE INITIALLY DEFERRED`, or `DEFERRABLE` and deferred with `SET
CONSTRAINTS {name, ... | ALL} DEFERRED`, is checked at `COMMIT` instead (`23503` there rolls the
transaction back), or when `SET CONSTRAINTS ... IMMEDIATE` names it; in autocommit it is checked
when the statement ends. `SET CONSTRAINTS` naming a missing constraint is `42704`, one that is
not deferrable `42809`, and outside a transaction block it warns and does nothing. A transaction
holds at most 4,194,304 deferred checks (`54000`). `DEFERRABLE` unique keys and primary keys are
recorded and checked immediately. A child is found
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
- A check or foreign key added with `NOT VALID` skips the existing rows and holds for new and
  changed ones; `pg_constraint.convalidated` is false and `pg_get_constraintdef` ends with `NOT
  VALID`. `VALIDATE CONSTRAINT name` checks the existing rows (the same `23514` or `23503` as
  adding it) and marks it valid. `RENAME CONSTRAINT a TO b` renames a check, foreign key, unique
  constraint or primary key (and the index of a key), `42704` when there is none, `42710` when
  the new name is taken.
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

## Triggers

Triggers run in the dialect they were written in, on the same tables every dialect sees (D157).
A trigger's changes are part of the statement that fired it: they commit with it, roll back with
it, and a statement that fails in a trigger leaves nothing behind.

- **SQLite:** `CREATE TRIGGER [IF NOT EXISTS] name [BEFORE | AFTER | INSTEAD OF] INSERT | DELETE |
  UPDATE [OF columns] ON table [FOR EACH ROW] [WHEN condition] BEGIN ... END`, with `NEW.column`
  and `OLD.column`, `RAISE(IGNORE)` (skips the row), `RAISE(ABORT | FAIL | ROLLBACK, message)`,
  and `INSTEAD OF` triggers that make a view writable. Triggers do not fire themselves again
  (SQLite's `recursive_triggers` is off).
- **MySQL:** `CREATE TRIGGER [IF NOT EXISTS] name BEFORE | AFTER INSERT | UPDATE | DELETE ON
  table FOR EACH ROW [FOLLOWS | PRECEDES other] body`, where the body is one statement or a
  `BEGIN ... END` block with `DECLARE`, `SET` (also `SET NEW.column = ...` in a `BEFORE`
  trigger), `IF ... ELSEIF ... ELSE ... END IF`, `WHILE ... DO ... END WHILE` and
  `SIGNAL SQLSTATE ... SET MESSAGE_TEXT = ...`. `INSERT ... ON DUPLICATE KEY UPDATE` fires the
  `UPDATE` triggers for the rows it updates, and `REPLACE` the `INSERT` triggers. A trigger
  that changes the table its statement is changing is error 1442, as in MySQL. `SHOW TRIGGERS
  [LIKE 'table']` and `SHOW CREATE TRIGGER` list them.
- **SQL Server:** `CREATE [OR ALTER] TRIGGER name ON table | view {FOR | AFTER | INSTEAD OF}
  {INSERT, UPDATE, DELETE} AS batch`, `ALTER TRIGGER`, `DROP TRIGGER [IF EXISTS]`,
  `ENABLE | DISABLE TRIGGER {name | ALL} ON table` and `ALTER TABLE ... {ENABLE | DISABLE}
  TRIGGER`. The body is a Transact-SQL batch on the batch interpreter (D137) with the `inserted`
  and `deleted` tables and `UPDATE(column)`; triggers fire once per statement, after it, and an
  `INSTEAD OF` trigger replaces the statement. `CREATE TRIGGER` must start its batch (error 111).
  `sys.triggers` lists them.
- **PostgreSQL:** `CREATE [OR REPLACE] TRIGGER name {BEFORE | AFTER | INSTEAD OF} event [OR
  ...] ON table [REFERENCING {OLD | NEW} TABLE AS name] [FOR EACH {ROW | STATEMENT}] [WHEN
  (condition)] EXECUTE {FUNCTION | PROCEDURE} name(arguments)`, `DROP TRIGGER [IF EXISTS] name ON
  table` and `ALTER TABLE ... {ENABLE | DISABLE} TRIGGER`. A trigger function is a PL/pgSQL
  function (`RETURNS trigger`; PostgreSQL has no `LANGUAGE sql` trigger functions, see
  [Functions, procedures and DO blocks](#functions-procedures-and-do-blocks)) with `NEW`, `OLD`
  and the `TG_*` variables: a `BEFORE` row trigger returning `NULL` skips the row and one
  returning a record changes it, and `CREATE TRIGGER` checks that the function exists (`42883`)
  and returns `trigger` (`42P17`).
  `pg_trigger`, `pg_get_triggerdef` and `information_schema.triggers` list triggers of every
  dialect.
- **Oracle:** `CREATE [OR REPLACE] [EDITIONABLE | NONEDITIONABLE] TRIGGER name {BEFORE | AFTER |
  INSTEAD OF} {INSERT | UPDATE [OF columns] | DELETE} [OR ...] ON table | view [REFERENCING
  {NEW | OLD | PARENT} [AS] name ...] [FOR EACH ROW] [FOLLOWS | PRECEDES other] [ENABLE |
  DISABLE] [WHEN (condition)] {plsql_block | CALL routine(arguments)}`, `ALTER TRIGGER name
  {ENABLE | DISABLE | COMPILE | RENAME TO new}`, `ALTER TABLE name {ENABLE | DISABLE} ALL
  TRIGGERS` and `DROP TRIGGER [IF EXISTS] name`. The body is a PL/SQL block on the PL/SQL
  interpreter with `:NEW` and `:OLD` (all `NULL` fields where the event has no such row),
  `INSERTING`, `UPDATING`, `UPDATING('column')` and `DELETING`; a `BEFORE` row trigger changes
  the row by assigning `:NEW.column`, an `INSTEAD OF` trigger makes a view writable, and a
  trigger without `FOR EACH ROW` fires once per statement. Oracle's errors are kept:
  `ORA-04081` for a second trigger of the same name without `OR REPLACE`, `ORA-04080` for a
  missing one, `ORA-04077` for `WHEN` without `FOR EACH ROW`, `ORA-04082` for `:NEW`/`:OLD`
  there, `ORA-04084` and `ORA-04085` for assigning `:NEW` after the row is written or `:OLD`,
  `ORA-25001`/`ORA-25002` for the wrong timing on a view or table, `PLS-00049` for a `:NEW` or
  `:OLD` column the table does not have, and `ORA-04092` for `COMMIT` or `ROLLBACK` in a
  trigger. An error in a trigger reaches the client with `ORA-06512: at "TRIGGER", line N` and
  `ORA-04088: error during execution of trigger 'TRIGGER'`; a trigger body that does not
  compile is refused at `CREATE` (`ORA-06550`) instead of being kept invalid. Line numbers count
  from the block's first line, as in Oracle. Compound, DDL, database event and crossedition
  triggers are `0A000`, and a trigger does not fire itself again.

Triggers that fire triggers nest at most 32 deep; deeper is an error, never a crash. Dropping
a table or view drops its triggers, and renaming one keeps them. Not yet: temporary and
constraint and event triggers, SQL Server DDL and logon triggers, Oracle compound and event
triggers, and transition tables (`REFERENCING`, `0A000` when the trigger fires). PostgreSQL statement triggers
fire on `TRUNCATE` too.

## Functions, procedures and DO blocks

In the PostgreSQL dialect (D158), users create and call their own functions and procedures in
`LANGUAGE sql` and `LANGUAGE plpgsql`:

- `CREATE [OR REPLACE] FUNCTION name([[IN | OUT | INOUT | VARIADIC] [name] type [DEFAULT
  expression], ...]) RETURNS type | SETOF type | TABLE(column type, ...) LANGUAGE {sql |
  plpgsql} [IMMUTABLE | STABLE | VOLATILE] [STRICT] [SECURITY DEFINER] [SET setting = value]
  AS 'body' | $$body$$`, also `RETURN expression` and `BEGIN ATOMIC ... END` bodies for `sql`;
  `CREATE [OR REPLACE] PROCEDURE` the same without `RETURNS`; overloads by input types;
  `DROP {FUNCTION | PROCEDURE | ROUTINE} [IF EXISTS] name[(types)], ... [CASCADE]` (`CASCADE`
  drops the triggers using a function); `ALTER {FUNCTION | PROCEDURE} ... RENAME TO | SET SCHEMA`
  (owner and attribute changes are accepted and change nothing).
- Calls: `SELECT f(...)` and `f(...)` anywhere in an expression, `SELECT * FROM f(...) [WITH
  ORDINALITY] [AS alias(columns)]` for set-returning functions, `CALL p(...)` (its result row is
  the `OUT` and `INOUT` values), `DO [LANGUAGE plpgsql] $$...$$`, named arguments (`a => 1`),
  defaults and `VARIADIC`. Overloads are chosen by argument types (`42725` when ambiguous,
  `42883` when none fits, `42809` for a procedure called as a function and back).
- PL/pgSQL: `DECLARE` (types, `%TYPE`, `%ROWTYPE`, `record`, `CONSTANT`, `NOT NULL`, defaults,
  `ALIAS FOR`, cursors), nested blocks with labels, `:=` and `=` assignments to variables, record
  fields and array elements, `SELECT ... INTO [STRICT]`, DML with `RETURNING ... INTO`,
  `PERFORM`, `EXECUTE ... [INTO] [USING]` dynamic SQL, `GET [CURRENT | STACKED] DIAGNOSTICS`,
  `FOUND`, `IF`/`ELSIF`, both `CASE` forms, `LOOP`, `WHILE`, `FOR` over integers, queries,
  cursors and `EXECUTE`, `FOREACH ... IN ARRAY [SLICE]`, `EXIT`/`CONTINUE [label] [WHEN]`,
  `RETURN`, `RETURN NEXT`, `RETURN QUERY [EXECUTE]`, `RAISE` with levels, condition names,
  `SQLSTATE` and `USING` options (notices reach the client as PostgreSQL's notices), `ASSERT`,
  `BEGIN ... EXCEPTION WHEN condition THEN ...` with `SQLSTATE`/`SQLERRM` (the block's writes are
  undone when a handler catches the error), `OPEN`/`FETCH`/`MOVE`/`CLOSE` with scrollable
  cursors, and `CALL` with `INOUT` variables assigned back. Errors name their place as PostgreSQL
  does (`PL/pgSQL function f(integer) line 3 at RAISE`).
- Bodies are checked at `CREATE` (unless `check_function_bodies` is off, as `pg_dump` sets it),
  with PostgreSQL's messages; the SQL statements inside run when they are reached.

Limits (D45): calls nest at most 100 deep (`54001`), every loop and statement can be cancelled,
a body nests at most 64 levels and holds at most 100,000 statements, a routine's text is at most
1 MiB, and a routine holds at most 1000 open cursors. Not yet: `COMMIT` and `ROLLBACK` in
procedures (`0A000`), refcursors returned to the caller, polymorphic types (`anyelement`),
`regprocedure`, functions that write when called once per row (`0A000`; call them on their
own), set-returning functions other than as a `FROM` item or in a `SELECT` without `FROM`, and
languages other than `sql` and `plpgsql`.

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
pg_catalog.set_config('search_path', '', false)` (constants or parameters, not `true` for local), `current_schema()`
and `current_schemas(bool)` (shown as `{pg_catalog,app,public}`). A name is looked up in each
schema of the path that exists, in order; `pg_catalog` is searched first for functions
(`pg_catalog.nextval`, `pg_catalog.set_config` and the other functions that exist). New objects
are created in the first schema of the path that exists (`3F000 no schema has been selected to
create in` when there is none). The other dialects have one schema: SQLite's `main.t` (`temp.t`
is the session's temporary schema), Oracle's `USER.t` for the session's user's own tables and
`hr.t` for schema `hr` (D127), MySQL's `db.t` for the connected database.

**Views**, in every dialect: `CREATE [OR REPLACE] VIEW name [(columns)] AS query`, `DROP VIEW
[IF EXISTS] name [, ...] [CASCADE | RESTRICT]`, and in PostgreSQL `ALTER VIEW [IF EXISTS] name
RENAME TO new | SET SCHEMA s | OWNER TO role`. A view may use joins, groups, subqueries, `WITH`,
set operations and other views. Its query is checked when it is created and expanded where it is
used, as a derived table.

A view that selects from one table or one such view, without `WITH`, `DISTINCT`, `GROUP BY`,
`HAVING`, `LIMIT`, `OFFSET`, set operations, aggregates, window or set-returning functions, is
automatically updatable, as PostgreSQL's rules say (D151): `INSERT`, `UPDATE` and `DELETE` on it
change its table, with `RETURNING` in the view's columns. Its columns that are plain columns of
the table can be given values (others are `0A000` "cannot update column "c" of view "v""); an
`UPDATE` or `DELETE` reaches only the rows the view shows; an `INSERT` gives the table's other
columns their defaults. Any other view is `55000` ("cannot update view "v"", with the reason in
the detail). `CREATE VIEW ... WITH [CASCADED | LOCAL] CHECK OPTION` (and `WITH CHECK OPTION` in
MySQL, Oracle and Transact-SQL, cascaded) refuses an inserted or updated row the view would not
show with `44000` ("new row violates check option for view "v""); `CASCADED` also checks the
views it reads, `LOCAL` only its own condition and the check options below it. The check option
is kept in the view's definition; a release before this one refuses such a view as damaged.
`TRUNCATE`, `COPY ... FROM` and `MERGE` into a view are `0A000`. `SELECT *` is expanded when the view is created, so columns added to the table
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
b ON ...) JOIN c ON ...)`) runs, `WITH CHECK OPTION` included. `CREATE MATERIALIZED VIEW` is
`0A000` with a message naming it; `CREATE TYPE` and `CREATE DOMAIN` are described in
[User-defined types and domains](#user-defined-types-and-domains).

## COPY

`COPY` loads a table from the client and unloads a table or a query to it, in PostgreSQL's
text, CSV and binary formats (D117, D154, PostgreSQL dialect only; the other dialects refuse it
with their own syntax error):

```sql
COPY table [(column, ...)] FROM STDIN [[WITH] (option, ...)] [WHERE condition]
COPY {table [(column, ...)] | (query)} TO STDOUT [[WITH] (option, ...)]
```

The older forms that `pg_dump` and psql write work too (`WITH CSV HEADER`, `DELIMITER [AS]
'x'`, `NULL [AS] 'x'`, `QUOTE`, `ESCAPE`, `FORCE QUOTE`, `FORCE NOT NULL`, `BINARY`), and
`STDIN` and `STDOUT` may be written either way round, as in PostgreSQL. Options:

| Option | Meaning |
|--------|---------|
| `FORMAT text \| csv \| binary` | text (the default), CSV, or PostgreSQL's binary format (below) |
| `DELIMITER 'c'` | one one-byte character between columns: tab in text, comma in CSV |
| `NULL 's'` | the text for NULL: `\N` in text, an unquoted empty field in CSV |
| `DEFAULT 's'` | `COPY FROM` only: the text that stands for the column's default |
| `HEADER [true \| false \| match]` | a first line of column names: written by `TO`, skipped by `FROM`, or with `match` checked against the columns (`22P04` on a mismatch) |
| `QUOTE 'c'`, `ESCAPE 'c'` | CSV only: the quote (`"`) and the character that escapes it inside quotes (the quote itself) |
| `FORCE_QUOTE (cols) \| *` | CSV `TO` only: quote these columns' non-NULL values |
| `FORCE_NOT_NULL (cols) \| *`, `FORCE_NULL (cols) \| *` | CSV `FROM` only: an unquoted null string is an empty string, or a quoted one is NULL |
| `ENCODING 'name'` | the encoding of the data the client sends or receives: `UTF8` (the default), `LATIN1`, `LATIN9`, `WIN1252`, and `UTF16` (`UTF16LE`, `UTF16BE`) beyond PostgreSQL; PostgreSQL's other encodings are `0A000`, an unknown name `22023` |
| `ON_ERROR stop \| ignore` | `COPY FROM` only: `stop` (the default) fails the copy at the first bad value; `ignore` skips each row with a value its column's type does not accept and reports how many (text and CSV only) |
| `LOG_VERBOSITY default \| verbose \| silent` | with `ON_ERROR ignore`: `verbose` adds a notice per skipped row, `silent` drops the count |
| `FREEZE` | accepted and changes nothing |

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

**`WHERE`.** `COPY ... FROM STDIN ... WHERE condition` loads only the rows the condition is
true for, as PostgreSQL does: each row is built first (columns the copy leaves out take their
defaults, so a skipped row still uses up a `serial` value), then tested, then stored. The
condition may use every column of the table, qualified with its name or not, and functions;
aggregates (`42803`), window functions (`42P20`) and subqueries (`0A000`) are refused, and
`COPY ... TO` takes no `WHERE` (`0A000`; copy a query instead).

**`ON_ERROR ignore`.** A row with a value its column's type input refuses (a class `22` error
such as `22P02`, `22003`, `22007` or `22001`) is skipped instead of failing the copy; the
other rows load in the same single transaction, and `COPY n` counts only them. A notice says
`N rows were skipped due to data type incompatibility`; with `LOG_VERBOSITY verbose` each
skipped row also gets `skipping row due to data type incompatibility at line L for column "c":
"value"`, and `silent` leaves out the count. Rows that break the format (`22P04`), bytes that
are not valid in the encoding (`22021`) and constraint violations (`23505`, `23502`, ...) still
fail the whole copy, as in PostgreSQL. `ON_ERROR ignore` is refused in the binary format.

**Encodings.** `ENCODING` names the encoding of the data on the client's side; EggDB stores
UTF-8, so `COPY FROM` converts the data to UTF-8 before reading it and `COPY TO` converts each
row from UTF-8. `LATIN1` (ISO 8859-1), `LATIN9` (ISO 8859-15) and `WIN1252` (Windows code page
1252) are converted by table, with PostgreSQL's names and aliases (`ISO_8859_1`, `Windows-1252`,
`utf-8`, ...); a byte with no character (`WIN1252`'s `0x81`) or a character with no byte (`€` in
`LATIN1`) is `22P05`, a NUL byte `22021`. `UTF16` reads a byte-order mark (little-endian without
one, as Windows tools write it) and writes one; `UTF16LE` and `UTF16BE` are fixed and write none.
The data of a `COPY` inside a script (`eggdb sql file.sql`, `eggdb restore`) is part of the
script's text, which is already decoded, so `ENCODING` does not apply to it; the binary format
carries text as UTF-8 and ignores `ENCODING`.

**The binary format.** `FORMAT binary` (or the older `COPY BINARY table ...`) reads and writes
the layout the PostgreSQL manual describes: the 11-byte signature `PGCOPY\n\377\r\n\0`, a
flags word and a header extension (skipped when reading), then per row a 16-bit field count and
each field as a 32-bit length (`-1` for NULL) and the value in its type's binary form (the same
forms as binary results over the protocol: integers, `real` and `double precision`, `numeric`,
`boolean`, text types, `bytea`, `uuid`, dates, times and time stamps, `interval`, `json` and
`jsonb`, enums and one-dimensional arrays of these), and a 16-bit `-1` trailer. Each value read
goes through its type's input as a text value would, so constraints, typmods and defaults
behave as in the other formats. Errors follow PostgreSQL's: `COPY file signature not
recognized`, `invalid COPY file header (WITH OIDS)`, `row field count is 1, expected 2`,
`unexpected EOF in COPY data`, `received copy data after EOF marker` (`22P04`), and `incorrect
binary data format` (`22P03`) with `COPY t, line 1, column a` as the context. `DELIMITER`,
`NULL`, `DEFAULT` and `HEADER` are refused in this format (`42601`), a script cannot carry
binary data inline (`0A000`; send it from psql's `\copy`, a driver or `Session::execute_with_copy`),
and columns of composite, range and some catalog types are `0A000`. Drivers' binary copy works
unchanged: psycopg's `copy.write_row`, Npgsql's `BeginBinaryImport`, pgx's `CopyFrom`.

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
`GRANT`, `CREATE EXTENSION plpgsql`, enum, composite and range types, domains, array columns,
deferred foreign keys and `NOT VALID` constraints. Functions, triggers, materialized views,
partitioned tables and other extensions are not created yet: `eggdb restore` lists them in its
report, and over the protocol they fail with an error that psql prints before going on.

## The PostgreSQL catalog

psql's `\d`, `\dt`, `\di`, `\dv`, `\ds`, `\dn`, `\d name` for tables, views, sequences and
indexes (with patterns such as `\d cust*` and `\dt shop.*`), `\l` and `\du` work against
`eggdb serve` as against PostgreSQL (roadmap item 37, D129). CI's `pg-catalog` job restores a
fixture's `pg_dump` into EggDB, runs psql 17 against it and against PostgreSQL 16 holding the
same fixture, and requires the same bytes; `scripts/pg-catalog/check.sh` does the same locally.

**Relations.** In the PostgreSQL dialect `pg_catalog.X`, and `X` alone, name the catalog
relation X (`pg_catalog` is searched first unless `search_path` lists it after another schema):
`pg_class`, `pg_attribute`, `pg_type`, `pg_namespace`, `pg_index`, `pg_constraint`,
`pg_attrdef`, `pg_description`, `pg_database`, `pg_roles`, `pg_am`, `pg_collation`,
`pg_sequence`, `pg_depend`, `pg_tablespace`, `pg_settings` (the session's settings, one row
each), `pg_timezone_names` (every region of the time zone database, D153) and `pg_trigger`
(every trigger, of any dialect, with `tgtype`, `tgenabled`, `tgattr` and `tgargs`), and the empty `pg_inherits`, `pg_policy`, `pg_statistic_ext`, `pg_publication`,
`pg_publication_rel`, `pg_publication_namespace`, `pg_rewrite`,
`pg_auth_members`, `pg_opclass`, `pg_enum` and `pg_range`, `pg_proc` with the array
functions `array_in`, `array_out`, `array_recv` and `array_send` (D135) and every user function
and procedure (D158), and `pg_language` (`internal`, `c`, `sql`, `plpgsql`), each with PostgreSQL
16's columns and column types. `pg_get_functiondef`, `pg_get_function_arguments`,
`pg_get_function_identity_arguments`, `pg_get_function_result`, `pg_function_is_visible` and
`regproc` name user routines, `pg_trigger.tgfoid` is the trigger's function, and
`information_schema.routines` and `parameters` list them, so psql's `\df` shows them. `information_schema`'s views are catalog relations too: see
[Java and Python apps](#java-and-python-apps-and-information_schema). A statement that names them builds their rows from the stored tables, views, sequences,
schemas and comments once, however often it reads them (correlated subqueries included); a
statement that names none costs nothing; a stored default, `CHECK` constraint or view that uses
a catalog function or a `reg*` cast reads the catalog when a statement uses it. Equalities in
`WHERE` between two tables of a comma (or `CROSS JOIN`) list join them by hash as an `ON`
condition does, so psql's index query stays fast with thousands of tables. They are read-only:
`INSERT`, `UPDATE`, `DELETE` and `COPY ... FROM` are `42501 permission denied for table
pg_class`, and `DROP`, `ALTER`, `COMMENT ON` and `CREATE INDEX` are `42501 permission denied:
"pg_class" is a system catalog`; `COPY pg_class TO` reads them. Other dialects do not see them,
and `public.pg_class` still names a table of your own.

**What the rows say.** An index shows its access method (`btree`, or `hash` for `USING hash`,
whose column is an `integer` hash code as in PostgreSQL) and each column's order (`DESC`,
`NULLS FIRST`, `NULLS LAST`) in `pg_index.indoption` and `pg_get_indexdef`, and psql's `\d`
prints them as PostgreSQL does. A unique index made by `CREATE UNIQUE INDEX` is a unique index,
not a constraint (`"i" UNIQUE, btree (a)`), and only `UNIQUE` constraints are in
`pg_constraint`. A `GENERATED ALWAYS AS IDENTITY` column is `generated always as identity`
(`attidentity` `a`) and a `GENERATED BY DEFAULT AS IDENTITY` one `generated by default as
identity` (`d`), whether `CREATE TABLE` made it or a dump's `ALTER TABLE ... ADD GENERATED`
restored it; a restored identity column has no `pg_attrdef` row, its sequence depends on it as
an identity sequence (`pg_depend.deptype` `i`, so psql prints `Sequence for identity column:`),
and `information_schema.columns` gives its generation and its sequence's start, increment and
bounds. Any other EggDB auto-increment column (`serial`, MySQL's `AUTO_INCREMENT`, SQL Server's
`IDENTITY`) still shows as `generated by default as identity` (D146).
`pg_database` has one row, the served database (`eggdb serve --database`), and `pg_roles` the
session's user, with every attribute, and PostgreSQL 16's predefined roles. `CHECK` constraints
and defaults are shown in PostgreSQL's display form, worked out from the stored text and the
column types: `CHECK (price >= 0::numeric)`, `CHECK (level::text = ANY (ARRAY['debug'::character
varying]::text[]))`, `nextval('customers_id_seq'::regclass)`, `'x'::text`. An expression the
display rules do not cover yet (`CASE`, for one) is shown as it is stored.

**OIDs.** The catalog's own relations, the built-in types, the schemas `pg_catalog` (11),
`public` (2200) and `information_schema`, the roles (the session's user is 10), the access
methods, collations and tablespaces have PostgreSQL's OIDs. Every other object's OID is a hash of
its names (FNV-1a over its kind, its table for an index, constraint or default, and its name,
folded into 16385 to 4294967295, a collision taking the next free number in a fixed order), so
it is the same in every statement and after a restart, and changes when the object is renamed.

**Types and functions.** `oid`, `xid`, `name`, `pg_node_tree`, `aclitem` (always NULL),
`int2vector` and `oidvector` (subscripts from 0, written separated by spaces), and `regclass`,
`regtype`, `regnamespace`, `regrole` and `regproc` work in expressions and casts but are not
column types of tables; `"char"` is the type of the catalog's kind columns only, and `"char"`
written by you is still `char(1)`. A `reg*` cast (`x::regclass`, `CAST(x AS regclass)` or
`regclass(x)`) takes a name or a number and shows the object's name, qualified with its schema
only when it is not visible on the search path; OID 0 and `'-'` show as `-`; a type name may
carry a modifier and `[]` (`'character varying(10)'::regtype` is `character varying`); an
unknown name is `42P01` (relation), `42704` (type, role) or `3F000` (schema), and an unknown
number shows as the number. Integers and quoted constants compare with `oid` columns, and
`nextval('s'::regclass)` (or `CAST('s' AS regclass)`) stays a plain name lookup that never
builds the catalog; naming a table there is `42809 "t" is not a sequence`. `pg_get_expr` takes
only the catalog's `pg_node_tree` columns, as PostgreSQL does. The functions:
`format_type`, `pg_get_userbyid`, `pg_table_is_visible`, `pg_type_is_visible`, `pg_get_expr`,
`pg_get_constraintdef`, `pg_get_indexdef`, `pg_get_viewdef` (the view's stored text),
`pg_relation_is_publishable`, `pg_partition_ancestors` (no rows: there are no partitions),
`pg_encoding_to_char`, `obj_description`, `col_description`, `pg_get_serial_sequence` (the
sequence a `serial` column owns, schema-qualified, or NULL), `to_regclass` and `to_regtype`
(NULL for a name that does not exist), `generate_subscripts`, `pg_get_triggerdef` (the
trigger's `CREATE TRIGGER` as written), and `pg_get_ruledef`, `pg_get_statisticsobjdef_columns`
and `shobj_description`, which return NULL.
`regproc` takes and shows the name of the built-in functions EggDB names: `array_in`,
`array_out`, `array_recv` and `array_send`, the `typinput`, `typoutput`, `typreceive` and
`typsend` of every array type in `pg_type`; another name is `42883`.

**Not yet:** `\d+` and `\dt+ \dv+ \ds+ \di+ \dn+` (sizes, storage, a table's access
method, `public`'s access privileges and description; `pg_table_size` and the rest are
missing), `\df`, `\dT`, `\dp`, `\dx`, `\dd`, `\db` and `\dC` (they read `pg_proc`'s other
functions, `pg_extension`, `pg_tablespace_location` and `pg_cast`, which are not there yet, and
fail readably or list nothing), the composite row type of each table, and a view column of a
catalog type (`CREATE VIEW v AS SELECT relname FROM pg_class` is `0A000`; `relname::text`
works). A `serial` column made by `CREATE TABLE` in EggDB (not restored from a dump) is an
auto-increment column and shows as an identity column, where PostgreSQL shows a sequence and a
`nextval(...)` default.
`\d` of a view shows the view's text as it was written (PostgreSQL shows its own pretty form;
only `\d+` prints it). `COMMENT ON` a catalog relation or its columns is refused (`42501`, as for
`DROP` and `ALTER`): nothing would keep the comment. The non-pretty forms of `pg_get_expr` and
`pg_get_constraintdef` are the pretty form (wrapped in one pair of parentheses for a `CHECK`),
without PostgreSQL's extra parentheses around nested casts. A `reg*` cast PostgreSQL rejects
while planning (`'t'::regclass::regtype`) fails here when the cast runs, so not on zero rows or
a NULL; `numeric` with a negative scale and `interval year(3)` are not taken by `regtype`, and
`timestamp(7)` gives no warning.

## .NET apps through Npgsql

.NET apps through Npgsql run against `eggdb serve` unchanged (roadmap item 45, D135), with an
ordinary connection string such as
`Host=127.0.0.1;Port=5432;Username=eggdb;Password=...;Database=shop`:

- **Type loading.** When a data source first connects, Npgsql reads the server's types from
  `pg_type`, `pg_namespace`, `pg_class`, `pg_proc` (to find array types by their receive
  function `array_recv`), `pg_range`, `pg_attribute` and `pg_enum` in one message; EggDB answers
  with its built-in types and their arrays, so `integer[]` and the other array columns read as
  .NET arrays.
- **Commands and batches.** Commands with typed parameters (`int`, `long`, `short`, `decimal`,
  `double`, `float`, `bool`, `string`, `DateTime`, `TimeSpan`, `Guid`, `byte[]`), readers in
  binary form, `BeginTransaction` with `Commit` and `Rollback`, and `NpgsqlBatch`, whose
  commands commit together or not at all.
- **Pooled connections.** A connection taken from the pool again is reset with `DISCARD ALL`,
  which keeps the settings of the connection string.
- **Cancelling.** `NpgsqlCommand.Cancel()` and a cancelled `CancellationToken` stop the running
  command with `57014`; the connection stays usable.

CI's `pg-apps` job runs `crates/eggdb/tests/clients/dotnet/NpgsqlAppCheck` (Npgsql 8.0.5)
against PostgreSQL 17 and against EggDB and requires the same output: type loading, CRUD over
pooled connections, an `int[]` column, a rolled-back and a committed transaction, a batch, a
batch that fails half way and leaves nothing, and `SELECT pg_sleep(30)` cancelled after one
second.

Known differences: Npgsql resets a connection that holds prepared statements (after
`NpgsqlCommand.Prepare()`, or with `Max Auto Prepare` set) with `SET SESSION AUTHORIZATION
DEFAULT`, `CLOSE ALL`, `UNLISTEN *` and `pg_advisory_unlock_all()`, which EggDB does not run
yet: such apps add `No Reset On Close=true` to their connection string for now. Array
parameters are not supported yet (array results are). `pg_sleep`'s result column is `text`
where PostgreSQL's is `void`.

## ORMs and migration tools

Three tools that own an app's schema and data run against EggDB unchanged (roadmap item 48,
D138):

- **SQLAlchemy's ORM** (2.0, through psycopg 3): declarative models with relationships,
  `MetaData.create_all()` and `drop_all()` (their `has_table` checks), a `Session`'s inserts of
  several rows at once (`INSERT ... SELECT ... FROM (VALUES ...) ORDER BY ... RETURNING`, the
  keys matched to the objects by order), lazy and `selectinload` loading, `LIMIT`/`OFFSET` with
  parameters, updates of changed attributes, bulk `update()`, `rollback()`, a unique violation
  as `IntegrityError` (`23505`) and cascading deletes.
- **Flyway** (10, Community, through pgjdbc): `migrate`, `info` and `validate` with its schema
  history table, its schema-empty and table-exists queries, `set_config('search_path', $1,
  false)`, `SET ROLE` to the session's own user, and its advisory lock. EggDB has one writer
  (D86), and Flyway's default runs the migration and the history insert in two connections'
  transactions at once, which fails with `55P03`; set `flyway.postgresql.transactional.lock=false`
  (`-Dflyway.postgresql.transactional.lock=false`, `FLYWAY_POSTGRESQL_TRANSACTIONAL_LOCK=false`)
  so Flyway uses one connection and `pg_try_advisory_lock`, until concurrent writers come
  (roadmap 4.M3-1).
- **EF Core 8** (Microsoft.EntityFrameworkCore.SqlServer, over TDS with `Encrypt=false`):
  `Database.Migrate()` with hand-written or generated migrations (`__EFMigrationsHistory`, each
  migration in its own transaction), `SaveChanges` batches (`MERGE ... OUTPUT INSERTED.[Id],
  i._Position` for several inserts, `INSERT ... OUTPUT`, `UPDATE`/`DELETE ... OUTPUT 1`, `SET
  IMPLICIT_TRANSACTIONS OFF; SET NOCOUNT ON;`), LINQ queries (`OFFSET ... FETCH` in derived
  tables, joins, `TOP(1)`, `CASE WHEN EXISTS`, `GROUP BY`, `Contains` as `OPENJSON(@p) WITH
  (...)`), and `SaveChanges` inside `BeginTransaction()` (EF's `SAVE TRANSACTION
  [__EFSavePoint]`) rolled back whole. EF Core 9's migrations lock with `sp_getapplock` through
  `EXEC @result = sp_getapplock ...`, which is not there yet.

**Advisory locks.** `pg_advisory_lock`, `pg_try_advisory_lock`, `pg_advisory_unlock` and their
`_shared` forms take one `bigint` or two `integer`s, as do the transaction-level
`pg_advisory_xact_lock` and `pg_try_advisory_xact_lock` (and `_shared`); `pg_advisory_unlock_all()`
releases the session's session-level locks. They are held by the session in one table for every
session over the same database: session-level locks count re-entries and last until unlocked or
until the session ends, transaction-level locks until the transaction ends (the statement, outside
one). The functions that wait return an empty `text` where PostgreSQL returns `void`.

CI's `pg-apps` job runs the ORM section of `crates/eggdb/tests/clients/python/app_check.py` and
`crates/eggdb/tests/clients/java/FlywayCheck.java` (with the migrations in
`crates/eggdb/tests/clients/java/flyway/`) on PostgreSQL 17 and on EggDB and requires the same
bytes; the `tds-apps` job runs `crates/eggdb/tests/clients/dotnet/EfCoreAppCheck` and compares it
with `scripts/tds-apps/expected-efcore-dotnet.out`.

## Java and Python apps and information_schema

Java apps through pgjdbc, and Python apps through psycopg 3 and SQLAlchemy Core, run against
`eggdb serve` unchanged (roadmap item 38, D131): their connection setup, CRUD with prepared
statements, transactions, `DatabaseMetaData` (`getSchemas`, `getTables`, `getColumns`,
`getImportedKeys`) and SQLAlchemy's `MetaData.reflect()` and inspector answer as on PostgreSQL.
CI's `pg-apps` job restores the catalog fixture's `pg_dump`, runs the same pgjdbc program and the
same psycopg 3 and SQLAlchemy program against EggDB and against PostgreSQL 17 holding the same
fixture, and requires the same bytes; `scripts/pg-apps/check.sh` does the same locally.
pgjdbc's `getPrimaryKeys` and `getIndexInfo` call `information_schema._pg_expandarray(anyarray)`,
a set-returning function of records `(x, n)` (each element and its position), read as
`(information_schema._pg_expandarray(i.indkey)).n` in a select list and as `(result.KEYS).x` from
a derived table's column; EggDB answers both (D152).

**`information_schema`.** `information_schema.schemata`, `tables`, `columns`, `views`,
`sequences`, `table_constraints`, `key_column_usage`, `referential_constraints`,
`constraint_column_usage`, `check_constraints`, `routines` and `parameters` answer with
PostgreSQL 17's columns and rows, built from the stored definitions once per statement that names
them, like the catalog relations above. A name without a schema reaches them only when
`information_schema` is on `search_path` (after any table of that name in an earlier schema);
they are read-only (`42501`). What the rows say:

- `tables` and `columns` list your tables and views and the catalog's own relations (the
  `pg_catalog` relations as `BASE TABLE`, `pg_roles`, `pg_settings` and `information_schema`'s
  relations as `VIEW`); views are listed as not insertable or updatable (`NO`), even those
  EggDB can change (D151).
- `columns` gives each column's `data_type` as `format_type` names it (`character varying`,
  `timestamp with time zone`, `ARRAY` for arrays), `character_maximum_length`,
  `character_octet_length` (four bytes a character, 1073741824 without a limit),
  `numeric_precision`, `numeric_precision_radix`, `numeric_scale`, `datetime_precision`,
  `interval_type`, `udt_name` (`int4`, `varchar`, ...), the default as `pg_get_expr` shows it,
  and `is_identity`/`identity_*` for EggDB's identity columns (`BY DEFAULT`, from 1 by 1).
- `table_constraints`, `key_column_usage` (with `position_in_unique_constraint` for foreign
  keys), `referential_constraints` (`match_option` `NONE`, PostgreSQL's rule names),
  `constraint_column_usage` (a foreign key's referenced columns; a check constraint's columns
  are the ones its text names) and `check_constraints` (the clause as `pg_get_constraintdef`
  shows it). As in PostgreSQL 17, every `NOT NULL` column is also a `CHECK` constraint named
  `<schema oid>_<table oid>_<column number>_not_null` whose clause is `<column> IS NOT NULL`.
- `views` gives the stored text of each view and `check_option` `NONE`; `sequences` each
  sequence's type, bounds, increment and cycle; `schemata` each schema with its owner.
- `routines` and `parameters` are empty: EggDB has no stored functions yet.

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
the name), the checks (count; each its name, the dialect it was written in as one byte, `0`
PostgreSQL, `1` MySQL, `2` SQLite, `3` Oracle or `4` SQL Server (D124), and its expression
text) and the foreign keys (count; each its name, its column numbers, the parent's name, the
parent's column names, the `ON DELETE` and `ON UPDATE` actions as one byte each and a
`DEFERRABLE` flag). A table that another table's foreign key references, or
that owns a sequence (`OWNED BY`), is written in version 6 too, with a flags byte after the
primary key's name saying so, so that an older release refuses it by name instead of deleting a
referenced row or dropping the table without its sequence. Other tables keep their version and
bytes; an older release refuses a version 6 table by name. A table in a schema other than
`public` is stored under its schema's name, a zero byte and its name, and written in version 7
(D116): the version 6 layout with the constraints part always present; `public` tables keep
their names, versions and bytes, and an older release refuses a version 7 table by name. A view
is stored under its name in the same key space as version 8: its dialect (the same codes as a
check's), a flag for `*` in its select list, its query text, its columns with their types, the schemas of the search path when
it was created and the relations it names; an older release refuses it by name. A table with a
`GENERATED ALWAYS` identity column, an identity column restored from a dump's `ALTER TABLE ...
ADD GENERATED` (whose number comes from a `nextval(...)` default), an index with a method other
than `btree` or a column other than `ASC NULLS LAST`, or a unique index that is not a
constraint's, is written in version 9 (D146), in or outside `public`: the version 7 layout, then
its identity columns (count; each its column number and `1` for `ALWAYS` or `2` for `BY
DEFAULT`, in column order) and, for each index in order, its method (`0` `btree`, `1` `hash`), a
byte `1` when it is a unique index without a constraint (`0` otherwise), and two 32-bit
little-endian masks of its columns by key position, `DESC` and `NULLS FIRST`. An auto-increment
column `BY DEFAULT` needs no entry. A table with a Transact-SQL `DEFAULT` kept as written or an
`IDENTITY(seed, increment)` other than (1, 1) is written in version 9 too (D147), with two more
tags in its columns' default byte, which older versions do not allow: `3`, a default written in
another dialect (its dialect code as a check's, never `0`, then its text, never blank), and `4`,
an auto-increment column with its seed and increment as two 64-bit little-endian signed numbers
(never an increment of 0, never (1, 1), at most one per table); the tags `0` no default, `1` a
PostgreSQL default and `2` an auto-increment column counting from 1 by 1 keep their meaning.
Every other table keeps its version and bytes, and an older release refuses a version 9 table by
name.

Version 10 (D152) is written only for a table with an array, enum, composite or range column, a
column of a domain, a `NOT VALID` check or foreign key, or an `INITIALLY DEFERRED` foreign key: the
version 9 layout, whose column types may now be `21` an array (then its element code) or `23` a
defined type (then its kind, `1` enum, `2` composite, `3` range, `4` domain, and its 32-bit
little-endian type OID; a column's own type is never a domain), then the columns of a domain
(count; each its column number, ascending, and the domain's OID, at least 16384; the column holds
the domain's base type) and the constraint statuses (count; each a constraint's name, ascending,
naming a check or foreign key of the table, and a flag byte, never `0`: `1` not validated, `2`
initially deferred, only for a `DEFERRABLE` foreign key). In rows, an enum value is tag `24` and
its label's sort order as a 64-bit float, a composite value tag `25` (field count, a byte `1` when
field names follow, the names, then each field as a nested value), a range tag `26` (a flag byte,
`1` empty, `2`/`4` lower bound present/inclusive, `8`/`16` upper bound present/inclusive, then
the present bounds as nested values); a nested value is a row value whose enum, composite, range
and array forms also carry their type OID or element code. In keys an enum orders by its sort
order. An older release refuses a version 10 table by name.

A user-defined type is a record under the catalog prefix, `0x0A` and the type's schema-qualified
name: record version `1`, its kind (as above), its 32-bit OID (16384 to 2^31-1, picked from a hash
of its name and never reused while it exists), then an enum's labels (count; each its 64-bit sort
order, strictly ascending, and its label of 1 to 63 bytes), a composite type's attributes (count;
each its name and type, never the type itself), a range's subtype, or a domain's base type, a `NOT
NULL` flag byte, an optional default text (`0`, or `1` and the text), and its checks (count; each
its name, its expression text over `VALUE`, and a byte `1` when validated). Every statement that
names a type loads them once, and a damaged record is an error naming it, never a panic.

A table's or view's triggers are one record under the catalog prefix, `0x0B` and the table's
internal name: record version `1`, the count of triggers (1 to 1024), then each trigger in firing
order: its name, its dialect code, its timing (`1` BEFORE, `2` AFTER, `3` INSTEAD OF), its events
(bits: `1` INSERT, `2` UPDATE, `4` DELETE, `8` TRUNCATE), its level (`1` row, `2` statement), a
flag byte (`1` enabled, `2` has a condition, `4` an old transition table, `8` a new one), its
`UPDATE OF` columns (count; each a name), the condition text and transition table names its flags
announce, its action (`1` SQLite or MySQL statements, `2` a Transact-SQL batch, `3` a function
name and its arguments) and the whole `CREATE TRIGGER` text. A record whose trigger does not fit
its dialect (a Transact-SQL row trigger, say) is damaged, and a damaged record is an error naming
it, never a panic.

A routine name's functions and procedures are one record under the catalog prefix, `0x0C` and
the routine's internal name (D158): record version `1`, the count of overloads (1 to 256), then
for each its dialect code, its kind (`1` function, `2` procedure), its identity (the input
argument type names joined by `, `, typmods left out) and the whole `CREATE` text (at most
1 MiB). Loading parses the `CREATE` text again and checks that kind and identity match; a record
that does not, or holds two overloads with one identity, is damaged, and a damaged record is an
error naming it, never a panic.

A partitioned table and each of its partitions have one record under the catalog prefix, `0x0E`
and the table's internal name (D169): record version `1`, a flag byte (`1` partitioned, `2` a
partition), the strategy and key column names when partitioned, and the parent's name and the
bound when a partition. Bound values are kept as text and read back in the key column's type. A
damaged record is an error naming it, never a panic.

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
ORMs and migration tools (D138): Flyway's default two-connection lock needs two writers at once
and fails with `55P03` (use `flyway.postgresql.transactional.lock=false`); a deadlock between
advisory locks waits until one statement is cancelled instead of failing with `40P01`, there is
no `pg_locks` view, and unlocking a lock the session does not hold returns false without a
warning; in `INSERT ... SELECT` a text value goes into a non-text column only with a cast (where
PostgreSQL types an untyped literal of the select list by the target column); `SET ROLE` to a
role other than the session's user is `0A000`.

Schemas and views (D116): a view is computed each time a statement uses it; a `WHERE`
condition on its plain columns is pushed into it (D155), so a lookup through a view uses its
table's primary key and indexes, but a view with grouping, aggregates, `DISTINCT`, a limit, a
window or a computed column used by the condition, or one on the NULL side of an outer join, is
still computed in full before the condition filters it; a view that is not automatically
updatable has no rules to make it so (its `INSTEAD OF` triggers make it writable),
there are no materialized or
recursive views, renaming a table
or view that a view uses is `0A000` (PostgreSQL follows the rename), a view resolves its names
with the search path it was created with when it is used, where PostgreSQL binds them once, a
column `DEFAULT nextval('s')` finds an unqualified `s` through the search path when it runs, and
messages name objects outside `public` as `schema.name`. There are no roles, so `GRANT`,
`REVOKE` and owners are accepted and ignored; `current_schemas` returns text, not `name[]`.

`information_schema` (D131): its columns have the base types of PostgreSQL's domains (`name` for
`sql_identifier`, `integer` for `cardinal_number`, `character varying` for `character_data`,
`character varying(3)` for `yes_or_no`, `timestamptz(2)` for `time_stamp`), so a client that
asks for a column's type name sees the base type; `routines` and `parameters` are empty; a check
constraint's columns in `constraint_column_usage` are the column names its text uses (PostgreSQL
records them as dependencies); `pg_settings` shows `vartype` `string` and no `category`, unit or
bounds.

`COPY` (D117, D154): `ENCODING` takes `UTF8`, `LATIN1`, `LATIN9`, `WIN1252` and the `UTF16`
forms, and PostgreSQL's other encodings are `0A000`; `UTF16` is EggDB's own (PostgreSQL has no
such encoding); a script's inline data ignores `ENCODING`; `ON_ERROR ignore` skips rows with
values their type refuses, but a domain's `CHECK` is tested with the row's constraints and fails
the copy (PostgreSQL skips the row), and `REJECT_LIMIT` is not recognized yet; the binary format
refuses composite and range columns and multidimensional arrays, and an incomplete row count at
the end of the data is an error where PostgreSQL ends quietly; `FREEZE` is accepted and changes
nothing (PostgreSQL refuses it unless the table was created or truncated in the same
transaction); files and programs on the server are always `42501`; a `\.` inside a text-format field reads as `.` where
PostgreSQL may refuse it; `COPY TO` of a table writes rows in primary-key order (PostgreSQL in
storage order); and some option errors carry other SQLSTATEs or wording than PostgreSQL's.

The catalog (D129): user objects' OIDs are hashes of their names, so they change when an object
is renamed (PostgreSQL keeps them); the catalog relations are read-only, even for the superuser;
`pg_class` and `pg_attribute` hold no TOAST tables, composite types or statistics (`reltuples` is
-1); a `serial` column made by EggDB's own `CREATE TABLE` shows as an identity column; and an
expression whose display EggDB does not work out is shown as stored, where PostgreSQL would
print its own form.

The correctness suite (`crates/eggdb-sql/correctness/postgresql.test`, [correctness.md](correctness.md))
checks the PostgreSQL manual's documented behaviour and marks no case as differing (the last
two, `COPY` in the binary format and in another encoding, now match). The other differences:

- One statement at a time is planned by simple rules without statistics ([Indexes](#indexes)):
  a `WHERE` that fixes every primary-key column with `=` reads one row (in a join, of the first
  table), conditions on a leading part of the primary key or an index read that range, and
  anything else reads the whole table. Each statement's result
  is built in memory before it prints (a `SELECT` without `ORDER BY` holds only the rows it
  returns; `ORDER BY` holds every matching row and sorts them, with no top-N for `LIMIT` yet,
  unless a key gives the order).
- Indexes: partial indexes (`WHERE`), indexes on expressions, `INCLUDE`, `CONCURRENTLY`,
  `NULLS [NOT] DISTINCT` and index methods other than B-tree and hash answer `0A000`; a `USING
  hash` index is kept and shown as `hash` but stored like a B-tree, so it also serves ranges and
  `ORDER BY`. `DESC` and `NULLS FIRST` in an index are kept and shown, and the index is stored in
  ascending order, which serves `ORDER BY ... DESC` by reading it backward; an `ORDER BY` that
  mixes directions sorts. An index name is
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
  `22007`, and so do `allballs`, `noon` and the other special `time` inputs. Input reads a
  region's name but not a time zone abbreviation (`PST`, `CEST`: `22007`), `TimeZone` takes no
  POSIX rule strings (`EST5EDT,M3.2.0,M11.1.0`: `22023`), and `pg_timezone_abbrevs` is missing;
  a `timetz` of a region takes the offset in force at the statement's start. `now()` is the statement's
  start time, where PostgreSQL's is the transaction's. `date_trunc` of an interval is missing.
  `to_char` writes `TZ` as `UTC` or the offset, `L` as a space and day and month names in
  English (the `C` locale's); `to_date` and `to_timestamp` ignore the day of the week and `Q`
  and read `IYYY`, `IW` and `ID` as plain fields; `to_number` keeps the digits, the decimal point
  and a minus sign and drops the rest of the text, so it is laxer than PostgreSQL with text
  that does not fit the template.
- Regular expressions (D130): when a match has more than one way to split into its
  parenthesized parts, EggDB gives each part what the pattern's priority order gives it (earlier
  alternatives first, greedy quantifiers longest first), where PostgreSQL applies its
  hierarchical longest/shortest rule to every part; the whole match is the same. A pattern with
  back references explores every way it can match, bounded by the step budget, so a hostile one
  ends with `54000`.
- `to_json`, `to_jsonb` and `json_build_*` take a whole row only as a `FROM` item's name;
  `jsonb_path_query` and the rest of SQL/JSON path, `json_populate_record`, `json_to_record`,
  `json_object(text[])` and `JSON_TABLE` are missing.
- `interval` (D113): only the `postgres` `IntervalStyle` is printed. PostgreSQL 17's `infinity`
  and `-infinity` intervals read, print, compare, add, multiply, divide, `justify`, and give
  `isfinite` false and `date_part` an infinite double as PostgreSQL's do (D153), but a time stamp
  plus an infinite interval fails with `22008` where PostgreSQL gives an infinite time stamp
  (EggDB has none yet), and `extract` of a field that grows with the interval fails with `0A000`
  (a `numeric` holds no infinity yet); a NaN factor fails with `22008` as in PostgreSQL; text input longer than 4,096 bytes fails with `22007`. `DISTINCT`, `min` and `max` of
  intervals that are equal but written differently (`'1 mon'`, `'30 days'`) return the first one
  read, which PostgreSQL leaves unspecified too. `time` and `timetz` cannot be compared with each
  other (`42883`), and `timetz` has no time zone functions yet.
- Values of different types that one expression returns row by row (`CASE` branches of
  `numeric` and `double precision`) are grouped and made distinct by their value where a `numeric` is exactly a
  `double precision` (`2.5`); a `numeric` such as `0.1` and the double nearest it compare equal
  but group apart. PostgreSQL gives such an expression one type before it runs.
- `jsonb` sorts by its normalized text, not by PostgreSQL's jsonb ordering, and a `jsonb` value
  more than 16 times larger than its text once its numbers are written out (`1e999`) fails with
  `54000`.
- `serial` and identity columns follow the table's auto-increment column: a NULL or `0` given
  to it also takes the next number, the number of a deleted last row is given again, and the
  column must be the first of the primary key; a sequence with `DEFAULT nextval(...)` (as
  `pg_dump` writes `serial` and identity columns) has none of these limits. `OVERRIDING USER
  VALUE` also ignores a value given for a `serial` column, which EggDB keeps as an identity
  column.
- Constraints are checked when each statement ends, except deferred foreign keys (checked at
  `COMMIT`); `RESTRICT` behaves as `NO ACTION`, so a statement that changes a referenced key and puts it
  back, such as the key swap `UPDATE p SET id = 3 - id` over ids 1 and 2, passes where
  PostgreSQL's immediate `RESTRICT` refuses it. `ALTER COLUMN ... TYPE ... USING` and `ADD
  COLUMN` of a `serial` or identity column answer `0A000`.
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
- Subqueries and `WITH` (item 30, D112, D151): a correlated subquery may not stand in `GROUP BY`
  or a `JOIN ... ON` of the query it names (`0A000`); an aggregate inside a subquery whose
  argument names only the outer query's columns (`SELECT (SELECT sum(t.b) FROM u) FROM t`), which
  belongs to the outer query in PostgreSQL and SQLite, answers `0A000`; a correlated subquery
  runs once per distinct outer value, so one over a million distinct values runs a million
  times, with no statement timeout yet, unless it is an `EXISTS`, `IN`, `= ANY`, `<> ALL` or an
  aggregate subquery whose outer references are only `inner = outer_column` conditions, which
  runs once and is looked up by key (D112, D155); a derived table or `WITH` query cannot name
  the outer query's columns (a `LATERAL` one can name the items before it, but not a query around
  its own `SELECT`, `0A000`), a set-returning or window function in the select list of a
  correlated subquery is `0A000`, and `INSERT ... VALUES` cannot have a subquery that names the
  inserted row. Subqueries nest at most 32 levels and a statement holds at most 1,024 `SELECT`s
  (`54001`). Every `WITH` query runs, even one nothing names, and `MATERIALIZED` and `NOT
  MATERIALIZED` change nothing; `SEARCH` and `CYCLE`, row constructors (`(a, b) IN (SELECT
  ...)`), a data-modifying `WITH` query that resolves conflicts, and `EXPLAIN` of a query with
  subqueries answer `0A000`. A derived table, a `WITH` query, a `LATERAL` item and the operands of
  a set operation are held in memory, counted with the statement's other rows against the 1 GiB
  limit, and a correlated subquery keeps up to 32 MiB of results by outer value. A `LATERAL` item
  runs once per distinct set of the values it names, like a correlated subquery, and its join
  back to them compares with `IS NOT DISTINCT FROM` (no hash join) when one of those values is
  NULL. `LATERAL ... USING` and a `LATERAL` item after a comma and a `RIGHT` or `FULL` join answer
  `0A000`; `FULL JOIN ... USING` columns cannot be named unqualified from a subquery (`42702`). An `IN` list of constants is compared item by item (a list of
  more than 256 items is not used to choose a key range either), so a very long list against
  many rows is slow; `IN (SELECT ...)` looks values up in a hash set.
- `NATURAL` joins, `DISTINCT ON`, `GROUPING SETS`, `ROLLUP` and `CUBE`, hypothetical-set
  aggregates (`rank(x) WITHIN GROUP (...)`), `percentile_cont` and `percentile_disc` over an array
  of fractions, `percentile_cont` between two `interval` values, and the other aggregates
  (`stddev`, `variance`, ...) fail with `0A000`. Window functions (D151) do not take `DISTINCT`
  or `ORDER BY` inside an aggregate's parentheses (`0A000`, as in PostgreSQL); a window whose
  frame does not start at `UNBOUNDED PRECEDING`, or that excludes rows, recomputes its aggregate
  for each row, so a long sliding frame costs its length per row; a correlated subquery cannot
  hold a window function in its select list (`0A000`).
- Arrays and set-returning functions (D128): arrays cannot be stored in a table or a view, have
  one dimension, and take the element types above; `x[1:2]`, `'[1:2]={1,2}'`, `ARRAY[[1, 2]]`
  and array parameters answer `0A000`. Set-returning functions run in `FROM` and as select-list
  items; several of them in one select list run in lock-step, each input row giving as many rows
  as the longest one, the others padded with NULL, as in PostgreSQL 10 and later (D131); one
  inside an expression is `0A000`, and a function in `FROM` cannot name an item after it in the
  same `FROM`. `COLLATE` takes only the `C`-like collations.
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
- A `TIME` outside `-838:59:59` to `838:59:59` is an error (`22008`) where MySQL's non-strict
  modes clip it with a warning, and `HOUR()`, `TIME_TO_SEC()` and the other `TIME` functions are
  missing.

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
- `SET` remembers session variables without applying them (`sql_mode`,
  `transaction_isolation`: every transaction is serializable); `SET GLOBAL` is refused.
  `time_zone` is applied: `SYSTEM` (UTC), an offset such as `'+05:30'` or a named zone such as
  `'Europe/Paris'` from the built-in time zone database (no `mysql.time_zone` tables to load);
  another value is error 1298.
- `DESCRIBE` shows the engine's types (`smallint` for `TINYINT`).
- Dates and times: `DATE` and `DATETIME` take years 4714 BC to 5874897 (MySQL: 1000 to 9999),
  zero dates (`0000-00-00`) are refused, a `DATETIME(fsp)` prints without trailing zeros in its
  fraction, `YEAR`, `DATE_FORMAT`, `STR_TO_DATE`, `DATE_ADD`, `UNIX_TIMESTAMP` and
  `ON UPDATE CURRENT_TIMESTAMP` are not supported yet.
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
Views are read-only unless an `INSTEAD OF` trigger makes them writable, as in SQLite,
`CREATE TEMP VIEW`, `CREATE TEMP TRIGGER` and the `temp` schema are `0A000`, and
`sqlite_schema` does not list views yet (it lists tables, indexes and triggers).

The SQLite suite (`crates/eggdb-sql/correctness/sqlite.test`, 234 cases from sqlite.org's
documentation of SQLite 3.46, never SQLite's source) marks these as differing until they are
fixed:
- Indexes: `EXPLAIN` without `QUERY PLAN` (the bytecode listing), partial indexes and indexes
  on expressions answer an error; `ON CONFLICT (columns) DO ...` applies to a conflict on any
  unique key, not only the one named; `sqlite_schema.sql` of an index is regenerated from the
  catalog.
- A column of `INTEGER`, `TEXT`, `REAL` or `NUMERIC` affinity has one type: the affinity's.
  SQLite's columns may hold any storage class, so a `NUMERIC` column given `7` keeps the integer
  `7` where EggDB keeps the real `7.0`, and an `INTEGER` column given `'abc'` keeps the text
  where EggDB answers `datatype mismatch`. Only `BLOB` columns hold every class (D153), and
  they cannot be in a key or an index yet (`0A000`).
- Dates and times: `date()`, `time()`, `datetime()`, `julianday()`, `strftime()`,
  `unixepoch()` and `CURRENT_TIMESTAMP` are not supported yet; `DATE` and `DATETIME` columns
  keep their text, as SQLite's do.

And more, not in the suite yet:
- Columns without a declared type are text, so `typeof()` says `text` for numbers stored
  there, where SQLite gives them `BLOB` affinity as a `BLOB` column has. A real given to an
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
  generated columns, views, `ATTACH`, `VACUUM`, `UPDATE OR ...`, `VALUES` as a query,
  `group_concat` and `total`, the JSON and math functions, `GLOB` character
  classes and bare columns in aggregate queries.

## Known differences from Oracle
The Oracle suite (`crates/eggdb-sql/correctness/oracle.test`, 610 cases from Oracle's SQL
Language Reference and PL/SQL Language Reference, never Oracle's software) marks these as differing until they are fixed:
- `EXPLAIN PLAN FOR` and `DBMS_XPLAN`, bitmap, function-based and reverse-key indexes, and the
  primary key's index in `USER_INDEXES` (EggDB keeps the primary key as the table's own order).
- `ROWNUM` in the select list, `ORDER BY` or anywhere but `WHERE` conditions joined by `AND`.
- Division that does not end keeps `numeric`'s scale (about 20 digits; Oracle has 38), so `2 /
  3 * 3` is not exactly 2, `date - date` of 8 hours shows fewer 3s and `MONTHS_BETWEEN` fewer
  digits.
- An interval prints its leading field with at least two digits and its type's fractional
  digits (six by default): `TIMESTAMP - TIMESTAMP` and `NUMTODSINTERVAL` show `+01 12:00:00.000000`
  where Oracle shows their nine-digit precisions (`+000000001 12:00:00.000000000`). A column's
  declared leading precision (`DAY(5)`) is accepted but not kept, and fractional seconds keep
  six digits (D149).
- An identity column that is not the first column of the primary key.

And more, not in the suite yet:
- Sequences hold 64-bit values: `NOMAXVALUE` is 9223372036854775807 (Oracle's is 10^28 - 1) and
  a larger `MAXVALUE` counts as `NOMAXVALUE`. Two `NEXTVAL` references in one row give two values
  (Oracle gives the row one), and the column of `s.NEXTVAL` is headed `S.NEXTVAL`.
- `NUMBER(p)` with `p` up to 18 is enforced by its storage type's range, not by `p` (no
  `ORA-01438` for 1000 in a `NUMBER(3)`); `NUMBER` holds up to 1,000 digits (Oracle: 38
  significant digits).
- `EXTRACT(HOUR ...)` is `ORA-30076` for a `TIMESTAMP(0)` column too, which EggDB stores like a
  `DATE`; `TIMEZONE_REGION` and `TIMEZONE_ABBR` are `ORA-00907`.
- `DATE` and `TIMESTAMP` show as `YYYY-MM-DD HH24:MI:SS[.FF]` in result sets whatever
  `NLS_DATE_FORMAT` says. A `TIMESTAMP WITH TIME ZONE` keeps the instant, not the offset it was
  written with: it shows in the session's time zone, as `WITH LOCAL TIME ZONE` does (`TZH`,
  `TZR` print the session's offset), and `TZR` names only offsets and `UTC`/`GMT`; `TZD` prints
  nothing. Dates before 15 October 1582 follow the proleptic Gregorian calendar, where Oracle
  switches to the Julian one, so their `J` and day names differ, and 4714 BC is accepted.
  `DATE + INTERVAL` keeps fractional seconds of a `DAY TO SECOND` interval. `TS` writes
  `HH:MI:SS AM` and `DL` follows the documented `fmDay, Month dd, yyyy` (`December 1`), choices
  recorded in D149; `L` and `U` pad the leading symbol to twelve characters as the conversion
  table shows. Languages other than American, `NLS_CURRENCY`, `E`/`EE` (other calendars),
  `TO_DATE(... DEFAULT ... ON CONVERSION ERROR)`, `LISTAGG ... OVER` and `AT TIME ZONE` are not
  supported yet. Other string functions given a date (`SUBSTR(d, ...)`) write it by
  `NLS_DATE_FORMAT` whatever its type; a time stamp assigned to a character column takes the
  timestamp format only when it has a fractional second. `NEXTVAL` in a `MERGE`'s `VALUES` draws
  only for the rows inserted. An Oracle view read from another dialect converts text as that
  dialect does.
- `VARCHAR2(n BYTE)` counts characters, and `ORA-12899` gives the maximum but not the column's
  name or the actual length. A `CHAR` compared with a `VARCHAR2` value ignores the `CHAR`'s
  trailing blanks (PostgreSQL's rule) where Oracle pads the shorter value only between two
  `CHAR`s. A string given to a `RAW` or `BLOB` is read as hexadecimal digits, as `HEXTORAW`
  does (an odd count gets a leading zero, another character is `ORA-01465`); the functions
  `HEXTORAW`, `RAWTOHEX`, `UTL_RAW` and `DBMS_LOB` are missing.
- A statement that fails after it wrote rows rolls back the whole transaction, where Oracle
  rolls back only the statement (no statement savepoints yet); a failing single-row `INSERT`,
  `UPDATE` or `DELETE` that wrote nothing leaves the transaction as it was, as in Oracle.
  `SAVEPOINT` is not supported yet.
- `GENERATED ALWAYS` accepts a given value, and an identity column reuses the number of a
  deleted last row (Oracle's identity is a sequence that never goes back).
- Result columns of expressions are named after the expression in upper case with spaces
  removed outside quotes, close to SQL*Plus's headings; `SELECT *` from `USER_TABLES` shows the
  twenty columns apps read (`TABLE_NAME`, `TABLESPACE_NAME`, `STATUS`, `NUM_ROWS`, ...), not
  Oracle's fifty, and the other dictionary views likewise show their commonly read columns.
- The data dictionary (D134): `NOT NULL` constraints are not listed in `USER_CONSTRAINTS`;
  `CREATED` and `LAST_DDL_TIME` are 2000-01-01 and the statistics (`NUM_ROWS`, `BLOCKS`, ...)
  NULL, since EggDB keeps neither; every index is `NORMAL`, every table in `USERS`; the
  session user's own schema is fixed when the connection chooses Oracle's dialect (a schema
  made for the user later is used from the next connection). For PL/SQL, `USER_SOURCE` holds
  each procedure, function, package, package body and trigger line by line from its keyword
  (so its line numbers are the ones `ORA-06512` reports), `USER_OBJECTS` lists them with
  `PACKAGE BODY` next to its `PACKAGE`, `USER_PROCEDURES` and `USER_ARGUMENTS` list standalone
  units and a package's public subprograms (with `OVERLOAD` for overloads, `%TYPE` parameters
  resolved to their column's type, the result of a function at `POSITION` 0, no row for a
  procedure without parameters and no `DATA_LEVEL` 1 rows for record fields), and
  `USER_TRIGGERS` shows `TRIGGER_TYPE`, `TRIGGERING_EVENT`, `DESCRIPTION`, `WHEN_CLAUSE`,
  `TRIGGER_BODY` and `STATUS`. `USER_ERRORS` is always empty, since EggDB refuses a unit that
  does not compile instead of keeping it invalid.
- PL/SQL (D160): in a SQL statement inside PL/SQL, a variable or parameter wins over a column
  of the same name (Oracle takes the column), so prefix variables (`p_`, `v_`) as Oracle's own
  guides advise. A unit that does not compile is refused, not created invalid. `COMMIT` and
  `ROLLBACK` in a block or procedure end the transaction when the statement that ran it
  finishes, so an error after an in-block `COMMIT` rolls back the whole statement. A package
  replaced by another session gets a fresh state silently where Oracle raises `ORA-04068` once.
  A standalone function without parameters is called with `()` (`v := f();`; a bare `f` is read
  as a column), package functions work either way. A deleted nested-table element leaves no
  gap, so `EXTEND` and assignment count from the last element; `c%ROWCOUNT` inside a cursor
  `FOR` loop over `c` is `ORA-01001`; collection subscripts in SQL cannot name columns;
  three-part names (`schema.pkg.proc`, `pkg.rec.field`) are not resolved; `SELECT ... INTO
  v(i)` of several columns into a record element and `DBMS_OUTPUT.GET_LINES` are not
  supported; `CALL DBMS_OUTPUT.PUT_LINE(...)` outside PL/SQL is not answered. Triggers never
  fire themselves again and the mutating-table check (`ORA-04091`) is not made.
- `SYS_CONTEXT` takes only text constants as its arguments, and a third argument (the length)
  is accepted and ignored.
- Not supported yet: `CONNECT BY`, outer joins with `(+)`, `SAVEPOINT`, `INSERT ALL`, `RETURNING ... INTO`, synonyms, database
  links, `ROWID`, the regular-expression, analytic and XML functions, and views `WITH CHECK
  OPTION` (refused).
- `NUMBER`'s range ends are not Oracle's: `TO_NUMBER('1e126')` gives a 127-digit value where
  Oracle raises `ORA-01426`, and a magnitude below `1e-130` does not underflow to 0.

## Known differences from SQL Server
The SQL Server suite (`crates/eggdb-sql/correctness/sqlserver.test`, from Microsoft's
Transact-SQL reference, never SQL Server's software) and D122 list these:
- Names and aliases come back in lower case; a mixed-case table made in PostgreSQL with quotes
  (`"Users"`) cannot be named from Transact-SQL.
- Comparisons are binary (the collation reported is Latin1_General_100_BIN2): `'a' = 'A'` is
  false, ordering is by code point, and trailing spaces count in `varchar` and `nvarchar`
  (`'a' = 'a '` is false; `char(n)` ignores them), so a script's expected output must not
  depend on case-insensitive comparison or ordering.
- A column keeps the engine type its SQL Server type maps to, not the SQL Server type (D123):
  a `tinyint` column accepts -32,768 to 32,767, a `datetime` or `smalldatetime` column keeps
  what it is given without rounding to 1/300 s or the minute and without the 1753 limit,
  `money` has `numeric(19,4)`'s slightly wider range, follows decimal arithmetic and prints 4
  places (SQL Server prints 2), and `binary(n)` and `varbinary(n)` columns neither check their
  length nor pad. `CAST` and `CONVERT` apply each type's own rules exactly.
- A `datetimeoffset` keeps its instant but not its offset: values come back in UTC with
  `+00:00`. `datetime2(7)`, `time(7)` and `datetimeoffset(7)` keep six fractional digits.
- A `uniqueidentifier` sorts by its bytes in order, not in SQL Server's group order;
  `NEWSEQUENTIALID()` gives random values, not increasing ones.
- TDS sends `money` as `decimal(19,4)`, `datetime` as `datetime2(3)` and `tinyint` as
  `smallint`.
- `SET FMTONLY ON` does not mark identity columns in the columns it describes, so
  `SqlBulkCopy` sends values for an identity column that its column mappings name even without
  `KeepIdentity`, and EggDB keeps them. A bulk load checks `CHECK` and foreign key constraints
  even without `CHECK_CONSTRAINTS` (SQL Server skips them and marks them untrusted).
- `DATEDIFF(nanosecond, ...)` counts microseconds times 1,000; `CONVERT` to `varbinary` of a
  string takes its UTF-8 bytes.
- Because a column keeps its engine type, `DATALENGTH` of an `nvarchar` column counts its
  UTF-8 bytes (an `N'...'` constant counts two bytes per UTF-16 unit), and whether a value is
  `varchar(max)` for the 8,000-byte cut of `REPLICATE`, `REPLACE` and `STUFF` is judged by
  its engine type (a `varchar(n)` column and a constant are cut, `varchar(max)` and `text` are
  not). `STRING_AGG` has no 8,000-byte limit. The `money` styles of `CONVERT` apply to every
  number kept with four decimal places, as `money` is kept.
- Dividing decimals and `AVG` of a decimal truncate to the result's scale. `FORMAT` knows the
  cultures `en-US` and invariant (`''`); another culture is error 40517. `DATEPART(nanosecond,
  ...)` counts microseconds times 1,000. `STRING_SPLIT`'s `enable_ordinal` is an integer
  constant. The `float` style 3 is error 40517.
- A user function's result without an alias (`SELECT dbo.f(1)`) is named after the function
  (`f`), where SQL Server leaves the column without a name. Functions run as `STABLE` ones: a
  function a statement calls sees the database as that statement started.
- Not yet: `TOP ... PERCENT` and `WITH TIES`, `OUTPUT ... INTO`, `deleted.` in `UPDATE`,
  `SELECT ... INTO` and global `##temporary` tables (`##name` is the session's own table, like
  `#name`).
- An identity column that is not the first column of the primary key. The next identity number
  follows the largest one in the table (the smallest for a negative increment), so a deleted
  largest value is used again and an emptied table starts again at its seed, where SQL Server
  keeps a counter.
- A statement that fails after it wrote rows rolls back the whole transaction (no statement
  savepoints yet) and ends the batch; one that wrote nothing leaves the transaction open, as in
  SQL Server. An unknown column of an existing table is found when the statement runs, after
  the statements before it in the batch ran (SQL Server finds it when the batch compiles).
- A `DEFAULT` that names a column or holds a subquery is `0A000` (SQL Server's 128 and 1046
  not yet), and a Transact-SQL default's implicit conversions inside the expression follow the
  dialect of the inserting statement, as a view's and a `CHECK`'s do (D124). `WITH ENCRYPTION` on views is 40517,
  and `SCHEMABINDING` does not stop a table from being changed under the view. Unnamed `CHECK`
  constraints are named `ck__<table>__<column>`, not with SQL Server's random digits.
- A view or `CHECK` written in Transact-SQL and used from another dialect (or one written in
  another dialect and used from Transact-SQL) runs its functions in its own dialect, but its
  implicit conversions, date parsing and the errors raised inside it follow the dialect of the
  statement that uses it (D124). A column that a Transact-SQL `CHECK` names cannot be renamed
  to a name with upper-case letters, since Transact-SQL reads names in lower case.
- Accepted where SQL Server refuses or differs: `SELECT name IS NULL FROM t` (a boolean result),
  an `UPDATE` of an identity column (SQL Server's 8102), and `ROLLBACK TRAN <name>` ignores the
  name. Divide by zero and arithmetic overflow end the batch, where SQL Server ends only the
  statement.
- Batch logic (D137): cursors (`DECLARE CURSOR`), `GOTO` and labels, `WAITFOR`, DDL
  triggers, `INSERT ... EXEC` of dynamic SQL, `SET eggdb.dialect` inside a block or dynamic SQL, and a `DECLARE` whose value
  reads a variable the same `DECLARE` declares are 40517. A `SELECT` whose value reads a variable
  the same `SELECT` assigns (`SELECT @s = @s + name FROM t`) is 40517; `SELECT @s += name FROM t`
  gives SQL Server's result. A table variable's columns take no constraints, defaults or identity
  and `NOT NULL` is not checked. `EXEC` of anything but the catalog procedures, `sp_executesql`
  and `sp_unprepare`; rolling back to a save point (`SAVE TRANSACTION` is recorded);
  `QUOTED_IDENTIFIER OFF` and `ANSI_PADDING OFF`; three-part names and other databases.
- A batch that loops forever over TDS cannot be stopped by the client's attention signal while it
  runs (the connection reads its next request only when the batch ends); closing the server
  stops it. Errors inside dynamic SQL report the line of the `EXEC` statement, not their line in
  the dynamic text. `RAISERROR ... WITH LOG` writes no log. `ERROR_PROCEDURE()` is always NULL.
- The catalog (D133) shows each column as the SQL Server type its engine type is most often
  declared with: `varchar` and `char` columns as `nvarchar` and `nchar`, `tinyint` as `smallint`,
  `money` as `decimal(19,4)`, `smalldatetime` and `datetime2(0)` as `datetime2(0)`, `datetime2(3)`
  as `datetime`, `binary` as `varbinary(max)`, `sysname` as `nvarchar(128)`. A unique index made
  by `CREATE UNIQUE INDEX` shows as a unique constraint, a default constraint is named
  `df__<table>__<column>` (its own name is not kept), `create_date` and `modify_date` are
  2000-01-01, and a `CHECK`'s definition is its stored text in parentheses rather than SQL
  Server's normalized form. `sp_statistics` reports no row counts or pages; `sp_tables` and
  `sp_columns` list the database's own tables and views, not the `sys` and `INFORMATION_SCHEMA`
  views. Object ids change when an object is renamed.
- `GetSchema("Databases")` (it runs `EXEC sp_executesql` inside a batch over
  `master..sysdatabases`) and mssql-jdbc's `getSchemas()` (it names `sys.schemas.name` with three
  parts) are not answered yet.
- Under `IMPLICIT_TRANSACTIONS ON`, a `SELECT` starts no transaction (SQL Server's does), so
  `@@TRANCOUNT` stays 0 after reads alone.
- Over TDS: an `OUTPUT` parameter comes back in the type of its declaration, whatever TDS type
  the client sent it as; `sp_prepare` returns no
  column metadata; a `varchar` parameter under a collation other than Latin1 or UTF-8 is read as
  Windows-1252; a `datetimeoffset` parameter keeps its instant, not its offset.
- What SSMS and sqlpackage scripts write runs (D125): `SET IDENTITY_INSERT t ON|OFF` (explicit
  identity values, 0 and negative ones too, are kept; Msg 544, 545, 8101, 8106 and 8107),
  `INSERT` without `INTO`, `PRIMARY KEY` and `UNIQUE` with `CLUSTERED` or `NONCLUSTERED`, index
  options, `ON [filegroup]`, `TEXTIMAGE_ON`, `ROWGUIDCOL`, `COLLATE`, `NOT FOR REPLICATION`,
  `ALTER TABLE ... WITH CHECK|NOCHECK ADD`, `ADD DEFAULT ... FOR`, `CHECK CONSTRAINT n|ALL` (Msg
  4917 when missing), `CREATE [UNIQUE] [CLUSTERED|NONCLUSTERED] INDEX` with `INCLUDE`, `WHERE` and
  `WITH`, `DROP INDEX i ON t`, `CREATE SCHEMA ... AUTHORIZATION` and `SET NUMERIC_ROUNDABORT
  OFF`. With these differences: filegroups, partition schemes, index options, `INCLUDE`, `DESC`
  and `CLUSTERED` on a secondary index are accepted without effect; a non-unique filtered index
  covers every row, and a unique one is accepted only for a `col IS NOT NULL` filter on its key
  (EggDB's unique indexes already allow many NULLs); `COLLATE` names are accepted, but text
  still compares by code point; `WITH NOCHECK ADD` still checks the existing rows; `ADD DEFAULT
  ... FOR` does not keep the constraint's name; `TOP (100) PERCENT ... ORDER BY` in a view reads
  as neither, as Microsoft documents that the `ORDER BY` there only picks `TOP`'s rows; `NOCHECK
  CONSTRAINT`, `IGNORE_DUP_KEY = ON`, FILESTREAM, sparse, masked, encrypted, memory-optimized and
  temporal tables are Msg 40517.

## Next parts (roadmap)
- SQL/JSON path (`jsonb_path_query`, `@?`, `@@`), `json_populate_record` and `JSON_TABLE`.
- Point `SELECT`s as fast as SQLite's everywhere: an AST that borrows the SQL text and a plan
  cache; table statistics for choosing between key lookups and hash joins.
- `GROUPING SETS`, `ROLLUP` and `CUBE`, `DISTINCT ON`, `NATURAL` joins and the other aggregates
  (`stddev`, `variance`, hypothetical-set aggregates).
- Then the other dialects (item 23), each with its own correctness suite; for the PostgreSQL protocol,
  TLS, `information_schema` and the catalog queries of GUI tools beyond psql's `\d` family,
  and `LISTEN`/`NOTIFY` (M6a).
