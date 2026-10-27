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

These are the first parts of the SQL core (roadmap items 19 and 19b). What comes next is listed
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
  (`bool`, `int2`, `int4`, `int8`, `float4`, `float8`, `text`, `varchar`), results in text or
  binary form, and row limits on `Execute`.
- **PostgreSQL's answers**: type OIDs in `RowDescription`, command tags (`SELECT 2`,
  `INSERT 0 1`, `CREATE TABLE`, …), errors with SQLSTATE, severity, message, detail, hint and
  position, notices, and `ReadyForQuery`'s `I`, `T` and `E`. After an error inside `BEGIN`,
  statements fail with `25P02` until `ROLLBACK`, as in PostgreSQL.
- **What drivers ask at connect**: `server_version` (`16.0 (EggDB 0.2.0)`), `client_encoding`
  and `server_encoding` (`UTF8`), `DateStyle`, `TimeZone`, `integer_datetimes`,
  `standard_conforming_strings` and the other reported parameters; `SET`, `SHOW`, `RESET`,
  `version()`, `current_user`, `current_database()` and `current_setting()`.
- **Several connections**, one thread and one session each, up to `--max-connections` (64),
  at most 16 of them still logging in; readers run side by side, writers one at a time (a
  second waits 5 s, then `55P03`: retry once the other session's transaction ends).

Not yet (each fails readably or is ignored harmlessly): TLS, psql's `\d` commands and the
catalog queries of GUI tools (`pg_catalog`), `COPY`, `LISTEN`/`NOTIFY`, cancelling a running
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
  `bigint`, `UNSIGNED` takes the next wider type, `FLOAT` is `real`, `DOUBLE`, `REAL`,
  `DECIMAL` and `NUMERIC` are `double precision` until the `numeric` type exists, `CHAR(n)` and
  `VARCHAR(n)` are `varchar(n)`, the `TEXT` family is `text`; `DATE`, `DATETIME`, `TIMESTAMP`,
  `JSON`, `BLOB` and `ENUM` answer error 1235 until item 19d.
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
  functions keep them). `BLOB` columns wait for the blob type.
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
  to 18), `NUMBER(p,s)` with a scale, `NUMBER` without precision, `FLOAT`, `REAL`, `DOUBLE
  PRECISION` and `BINARY_DOUBLE` as `double precision`, `BINARY_FLOAT` as `real`,
  `INTEGER`/`INT`/`SMALLINT` (Oracle's `NUMBER(38)`) as `bigint`, `CLOB`, `NCLOB` and `LONG` as
  `text`, and `BOOLEAN` (23ai). `DATE`, `TIMESTAMP`, `RAW` and `BLOB` wait for their types.
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

## What runs today

**Statements**
- `CREATE TABLE [IF NOT EXISTS] name (column type [NOT NULL | NULL | PRIMARY KEY | UNIQUE | DEFAULT expr] ..., [PRIMARY KEY (columns)], [UNIQUE (columns)] ...)`.
  A table without a primary key keeps its rows in insertion order under a hidden row number.
  `CONSTRAINT name` names a `UNIQUE` constraint's index (otherwise `table_column_key`). At
  most 1,600 columns, as in PostgreSQL.
- `CREATE [UNIQUE] INDEX [IF NOT EXISTS] [name] ON table [USING btree | hash] (column [ASC | DESC] [NULLS FIRST | LAST], ...)`
  and `DROP INDEX [IF EXISTS] name [, ...] [RESTRICT]`: secondary indexes, described under
  [Indexes](#indexes). An unnamed index is `table_columns_idx`.
- `EXPLAIN statement` for `SELECT`, `UPDATE` and `DELETE`: the plan as PostgreSQL prints it
  without costs (`Seq Scan on t`, `Index Scan using t_b on t`, `Index Scan Backward ...`,
  `Sort`, `Limit`, `Nested Loop`, `Hash Join`).
- `DROP TABLE [IF EXISTS] name [, ...] [RESTRICT]`: removes the definition and every row, in
  one transaction.
- `INSERT INTO name [(columns)] VALUES (...), (...) | DEFAULT VALUES [RETURNING items]`, with
  `DEFAULT` in a values list.
- `SELECT [ALL | DISTINCT] items [FROM from_item [join ...]] [WHERE expr] [GROUP BY expr, ...] [HAVING expr] [ORDER BY expr [ASC | DESC] [NULLS FIRST | LAST], ...] [LIMIT n | ALL] [OFFSET n [ROW | ROWS]]`.
  `ORDER BY` takes an output column's number or alias, or any expression over the tables (with
  `DISTINCT`, only expressions in the select list, else `42P10`); a `SELECT` without `FROM`
  returns one row.
- **Joins**: `a [INNER] JOIN b ON cond`, `a LEFT [OUTER] JOIN b ON cond`, `JOIN ... USING (columns)`,
  `a CROSS JOIN b` and `FROM a, b`, chained left to right, each table with an optional alias.
  Columns are named `name` or `alias.name`; `alias.*` expands one table and `*` all of them,
  with a `USING` column once, first, as PostgreSQL does. A left join extends a left row that
  matches nothing with NULLs; conditions in `ON` decide the match, conditions in `WHERE`
  filter afterwards. A `JOIN` binds tighter than a comma, so `FROM a, b JOIN c ON ...` cannot
  name `a` in its `ON` (`42P01`, "invalid reference to FROM-clause entry").
- **Grouping**: `GROUP BY` takes expressions over the tables, an output column's number, or an
  output alias that is not also an input column. Every column the select list, `HAVING` or
  `ORDER BY` uses outside an aggregate must be grouped (`42803`), except the columns of a table
  whose whole primary key is grouped, as PostgreSQL allows. A query with an aggregate or
  `HAVING` and no `GROUP BY` returns one row, even over no rows. Groups come out in no promised
  order; use `ORDER BY`.
- **Aggregates**: `count(*)`, `count(expr)`, `sum`, `avg`, `min`, `max`, each with `DISTINCT` or
  `ALL`. They skip NULLs; `count` of nothing is 0, the others are NULL. Result types are
  PostgreSQL's: `count` is `bigint`; `sum` of `smallint` or `integer` is `bigint`, of `real` is
  `real`, of `double precision` is `double precision`; `avg` of a float is `double precision`;
  `min` and `max` keep their argument's type and order NaN above every number, as `ORDER BY`
  does. `sum` and `avg` of text, and `min` and `max` of booleans, fail with `42883`, as there.
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

Values follow PostgreSQL's rules: integer arithmetic stays in the wider operand's type and
fails with `22003` on overflow (`integer out of range`); `/` on integers truncates toward zero;
division by zero fails with `22012`; floating-point results that overflow fail; `real` and
`double precision` print in the shortest form that reads back exactly, with an exponent from
`1e+15` (`1e+06` for `real`), `NaN` and `Infinity`; `NaN` equals itself and sorts above every
other number. Assigning a longer string to `varchar(n)` fails with `22001` unless the excess is
spaces, and a cast truncates; a value of any type assigned to a text column is stored as its
text (`true` as `true`). A quoted constant such as `'42'` takes the type its context needs
(`WHERE id = '42'`, `INSERT INTO t (flag) VALUES ('yes')`), as PostgreSQL's "unknown" type does.

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
- Functions `coalesce`, `nullif`, `abs`, `length` (`char_length`, `character_length`), `lower`
  and `upper`.
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

## How SQL is stored

SQL lives in the database's one ordered key space, under keys that begin with the five bytes
`0xff 'S' 'Q' 'L' 0x00` (D98). Below that prefix:

| Key | Value |
|-----|-------|
| prefix, `0x01` | the catalog's format version and the next table number |
| prefix, `0x02`, table name | the table's definition: its number, columns, types, `NOT NULL`, defaults and primary key |
| prefix, `0x03`, table number (4 bytes, big-endian), primary key | the row |
| prefix, `0x04`, table number, index number (4 bytes each, big-endian), indexed values, primary key | empty: one entry of a secondary index |

A table definition is written in format version 1, or version 2 when it has an
`AUTO_INCREMENT` column, and in version 3 only when it has secondary indexes: version 3 adds
after the primary key a count of indexes and, for each, its number, name, whether it is unique
and its column numbers (D109). Tables without indexes keep their bytes, so older releases
still read them; an older release refuses a version 3 table by name rather than misreading it.

Primary keys are encoded so that byte order is value order (D98): each column is a marker byte
followed by the value; integers are big-endian at their width with the sign bit flipped, floating
point is IEEE 754 with the sign handled so negatives sort first (`-0` stored as `0`, every NaN as
one NaN above infinity), booleans are one byte, and text is UTF-8 with `0x00` written as
`0x00 0xff` and ended by `0x00 0x01`. Text compares byte by byte, as PostgreSQL's `C`
collation does; other collations come with D30. A table without a primary key uses an 8-byte
row number instead.

A row is a format version byte, the column count, and per column a type byte (0 for NULL) and
the value. A row shorter than its table reads its missing trailing columns as NULL, which leaves
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
The correctness suite (`crates/eggdb-sql/correctness/postgresql.test`, [correctness.md](correctness.md))
checks the PostgreSQL manual's documented behaviour; these cases are marked there as differing
until they are fixed:
- Decimal constants (`1.5`, `10.0 / 4`), `numeric` columns and `avg` of integers are `numeric` in
  PostgreSQL, which is not supported yet (`0A000`; roadmap: the `numeric` type, item 19's next parts).
- `RIGHT` and `FULL` joins answer `0A000` (roadmap: "Next parts" below).
- The string functions `substr`, `substring(... FROM ... FOR ...)`, `trim`, `ltrim`, `rtrim`,
  `btrim`, `replace`, `concat`, `position(... IN ...)`, `left`, `right`, `repeat` and `reverse`
  are missing (`42883`, or `42601` for the SQL-standard syntax and for `left`/`right`, which
  parse as join keywords; roadmap: the rest of PostgreSQL's string functions, below).

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
- `avg` of `smallint`, `integer` or `bigint` returns PostgreSQL's `numeric`, which is not
  supported yet: it fails with `0A000` and a hint to write `avg(x::double precision)`. `sum` of
  `bigint` is also `numeric` there: it runs where its value prints the same (a select item, an
  `ORDER BY` item, either side of a comparison) while the sum fits in a `bigint`, and fails with
  `0A000` inside arithmetic or past 64 bits.
- `RIGHT`, `FULL` and `NATURAL` joins, subqueries in `FROM`, `LATERAL`, `DISTINCT ON`,
  `GROUPING SETS`, `ROLLUP` and `CUBE`, window functions, `FILTER`, ordered aggregates and the
  other aggregates (`string_agg`, `bool_and`, `array_agg`, ...) fail with `0A000`.
  When the output stops being read (`eggdb sql ... | head`), the command stops before the next
  statement.
- `UPDATE` and `DELETE` collect the rows they change before changing them, in memory.
- A whole transaction must fit in the page cache (`--cache`), as for every write (D86).
- Type checks happen as rows are evaluated, so a type mismatch in a `WHERE` on an empty table
  is not reported (PostgreSQL reports it when planning).
- Decimal constants such as `1.5` are PostgreSQL `numeric`, which is not supported yet: they
  work where a `real` or `double precision` value is expected (a float column, a comparison
  with one, `1.5::float8`) and fail with `0A000` elsewhere.
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
The MySQL suite (`crates/eggdb-sql/correctness/mysql.test`, 118 cases from the MySQL 8.0
Reference Manual's documented behaviour) marks these as differing until they are fixed:
- `7 / 2` is `3.5` (a `double`) where MySQL gives the `DECIMAL` `3.5000`: decimals come with the
  `numeric` type (item 19d).
- `1 / 0` is error 1365 where MySQL's `SELECT` gives `NULL` with a warning.
- Text compares byte by byte (`'a' = 'A'` is false) where `utf8mb4_0900_ai_ci` ignores case and
  accents; collations come with D30.

And more, not in the suite yet:
- Comparisons and `IS` tests are booleans, shown as `1` and `0`, but arithmetic on them (`TRUE +
  1`) and `IF(1, ...)` with an integer condition are type errors where MySQL treats booleans as
  integers. Strings are not converted to numbers implicitly (`'5' + 1` with a typed string).
- A statement that fails after it wrote rows inside a transaction rolls the whole transaction
  back, where MySQL rolls back only the statement (no statement savepoints yet).
- `AUTO_INCREMENT` continues from the largest value in the table, so the value of a deleted last
  row is given again, which MySQL 8 does not do; `AUTO_INCREMENT=n` table options are ignored.
- `INSERT IGNORE` skips duplicate keys only; other errors still fail the statement.
- `UPDATE` reports rows matched, as with `CLIENT_FOUND_ROWS`, not rows changed.
- `BIGINT UNSIGNED` is refused; `UNSIGNED` columns accept negative values.
- `SET` remembers session variables without applying them (`sql_mode`, `time_zone`,
  `transaction_isolation`: every transaction is serializable); `SET GLOBAL` is refused.
- `DESCRIBE` shows the engine's types (`smallint` for `TINYINT`, `double` for `DECIMAL`).
- Indexes: a key on a column prefix (`KEY k (name(10))`) indexes the whole value, and a
  `UNIQUE` one answers 1235; `FULLTEXT` and `SPATIAL` keys answer 1235. `SHOW INDEX` gives
  `Cardinality` and `Sub_part` as `NULL`, and `EXPLAIN` gives `key_len`, `ref` and `rows` as
  `NULL`. `ALTER TABLE ... ADD INDEX` waits for `ALTER TABLE`.

## Known differences from SQLite
The SQLite suite (`crates/eggdb-sql/correctness/sqlite.test`, 155 cases from sqlite.org's
documentation of SQLite 3.46, never SQLite's source) marks these as differing until they are
fixed:
- Indexes: `EXPLAIN` without `QUERY PLAN` (the bytecode listing), partial indexes and indexes
  on expressions answer an error; `ON CONFLICT (columns) DO ...` applies to a conflict on any
  unique key, not only the one named; `sqlite_schema.sql` of an index is regenerated from the
  catalog.
- A column has one type: the affinity's. SQLite's columns may hold any storage class, so a
  `NUMERIC` column given `7` keeps the integer `7` where EggDB keeps the real `7.0`, and an
  `INTEGER` column given `'abc'` keeps the text where EggDB answers `datatype mismatch`.
- Dates and times: `date()`, `time()`, `datetime()`, `julianday()`, `strftime()`,
  `unixepoch()` and `CURRENT_TIMESTAMP` wait for the timestamp type (item 19d).

And more, not in the suite yet:
- Columns without a declared type are text, so `typeof()` says `text` for numbers stored
  there; `BLOB` columns and `x'...'` literals wait for the blob type. A real given to an
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
  generated columns, views, triggers, `ATTACH`, `VACUUM`, `UPDATE OR ...`, `WITH`, subqueries,
  window functions, `group_concat` and `total`, the JSON and math functions, `GLOB` character
  classes and bare columns in aggregate queries.

## Known differences from Oracle
The Oracle suite (`crates/eggdb-sql/correctness/oracle.test`, 164 cases from Oracle's SQL
Language Reference, never Oracle's software) marks these as differing until they are fixed:
- `EXPLAIN PLAN FOR` and `DBMS_XPLAN`, bitmap, function-based and reverse-key indexes, and the
  primary key's index in `USER_INDEXES` (EggDB keeps the primary key as the table's own order).
- `ROWNUM` in the select list, `ORDER BY` or anywhere but `WHERE` conditions joined by `AND`,
  and `FETCH ... PERCENT` and `WITH TIES`.
- `LISTAGG`, `MINUS` and the other set operators, and `MERGE`.
- `DATE` and `TIMESTAMP` columns, `SYSDATE`, `SYSTIMESTAMP` and `CURRENT_DATE`: they wait for
  the timestamp type (item 19d).
- Sequences (`CREATE SEQUENCE`, `seq.NEXTVAL`, `seq.CURRVAL`, `DROP SEQUENCE`): they need
  state kept in the file, a file-format change of their own (D108).
- An identity column that is not the first column of the primary key.

And more, not in the suite yet:
- `NUMBER` is exact decimal in Oracle; here `NUMBER` without precision and `NUMBER(p,s)` with a
  scale are binary floating point (`0.1 + 0.2` is `.30000000000000004`), precision is not
  enforced beyond the storage type's range (no `ORA-01438`), and integer arithmetic past 64 bits
  is `ORA-01426` where Oracle has 38 digits. They wait for the `numeric` type.
- `CHAR(n)` is not blank-padded, `VARCHAR2(n BYTE)` counts characters, and `ORA-12899` gives
  the maximum but not the column's name or the actual length.
- A statement that fails after it wrote rows rolls back the whole transaction, where Oracle
  rolls back only the statement (no statement savepoints yet); a failing single-row `INSERT`,
  `UPDATE` or `DELETE` that wrote nothing leaves the transaction as it was, as in Oracle.
  `SAVEPOINT` is not supported yet.
- `GENERATED ALWAYS` accepts a given value, and an identity column reuses the number of a
  deleted last row (Oracle's identity is a sequence that never goes back).
- Result columns of expressions are named after the expression in upper case with spaces
  removed outside quotes, close to SQL*Plus's headings; `SELECT *` from `USER_TABLES` shows the
  three columns above, not Oracle's fifty.
- Not supported yet: subqueries and views, `CONNECT BY`, outer joins with `(+)`, `RIGHT` and
  `FULL` joins, `ALTER TABLE`, `CHECK` and foreign keys, PL/SQL (`BEGIN ...
  END`, procedures, packages), `SAVEPOINT`, `INSERT ALL`, `RETURNING ... INTO`, synonyms,
  database links, `ROWID`, the date, regular-expression, analytic and XML functions, `NLS_`
  settings, and schemas other than the session user's.

## Next parts (roadmap)
- The `numeric`, `timestamp` and `json`/`jsonb` types, and `serial`/identity columns.
- Point `SELECT`s as fast as SQLite's everywhere: an AST that borrows the SQL text and a plan
  cache; table statistics for choosing between key lookups and hash joins.
- `RIGHT` and `FULL` joins, subqueries, and the other aggregates.
- The rest of PostgreSQL's string functions (`substr`, `trim`, `replace`, `concat`, `position`,
  `left`, `right`, ...) and the SQL-standard `substring(... FROM ... FOR ...)` syntax.
- Then the other dialects (item 23), each with its own correctness suite; for the PostgreSQL protocol,
  TLS, the `pg_catalog` queries of psql's `\d` and GUI tools, `COPY` and query cancellation
  (M6a).
