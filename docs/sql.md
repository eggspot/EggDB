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
- **`DEALLOCATE [PREPARE] {name | ALL}`** forgets the connection's prepared statements of that
  name, or all of them (psycopg sends it after a rollback), both the protocol's and those made
  with SQL `PREPARE`. Not yet: `26000` for a name that was
  never prepared, which is ignored instead.

psql's describe commands (`\d`, `\dt`, `\di`, `\dv`, `\ds`, `\dn`, `\l`, `\du` and their `+`
forms, `\df`, `\dT`, `\dD`, `\dp`, `\dx`, `\dd`, `\db`, `\dC`, `\dy`, `\dRp` and `\dRs`) read
EggDB's catalog: see [The PostgreSQL catalog](#the-postgresql-catalog).

Not yet (each fails readably or is ignored harmlessly): the catalog queries of GUI tools beyond
psql's, and non-ASCII passwords. TLS, `LISTEN`/`NOTIFY` and stored roles have their own sections.

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
  comments, `/*! ... */` run as SQL, `?` placeholders, `X'0102'` hex literals.
- **Statements**: `LIMIT offset, count`, `AUTO_INCREMENT`, `INSERT IGNORE`, `REPLACE`,
  `INSERT ... ON DUPLICATE KEY UPDATE` with `VALUES(col)`, multiple-table `UPDATE t1 [JOIN t2
  ...] | t1, t2 SET t1.a = ..., t2.b = ... [WHERE ...]` and `DELETE t1[.*], t2 FROM ... | DELETE
  FROM t1, t2 USING ...` (each listed table is changed or deleted once per row that joins, an
  unqualified `SET` column is the one table's that has it or error 1052, an unknown table is
  1109; D297), `GROUP BY ... WITH ROLLUP` and `GROUPING(expr, ...)` (D298), `SHOW TABLES`,
  `SHOW DATABASES`,
  `DESCRIBE`/`SHOW COLUMNS`, `SHOW CREATE TABLE`, `SHOW VARIABLES [LIKE ...]`, `SHOW WARNINGS`,
  `SHOW COUNT(*) WARNINGS`, `USE`, `SET` of session and user variables (`SET NAMES`, `SET
  autocommit`, `SET @x = ...`), `START TRANSACTION`, `LOCK TABLES`/`UNLOCK TABLES` (accepted),
  table options such as `ENGINE=` and `DEFAULT CHARSET=`; `KEY`/`INDEX` and `UNIQUE [KEY]`
  entries and `UNIQUE` columns make indexes (an unnamed one is named after its first column,
  then `_2`, `_3`), and `CREATE [UNIQUE] INDEX name [USING BTREE] ON t (...)`, `DROP INDEX name
  ON t`, `ALTER TABLE t ADD INDEX|KEY [name] (...)`, `ADD UNIQUE [INDEX|KEY] [name] (...)`,
  `RENAME INDEX|KEY a TO b` and `DROP INDEX|KEY name` (errors 1061 for a name the table already
  has, 1176 for renaming a key the table lacks, 1091 for dropping one; on a partitioned table
  the index reaches every partition, D316, D350), `SHOW INDEX|INDEXES|KEYS FROM t` and
  `EXPLAIN SELECT` (MySQL's columns: `type` `ALL`,
  `const`, `ref`, `range` or `index`, the `key`, and `Extra`; also `FORMAT=TRADITIONAL`) work as
  in MySQL, and so do `EXPLAIN FORMAT=TREE|JSON`, `EXPLAIN ANALYZE [FORMAT=TREE]` of a
  `SELECT`, a `TABLE t` and a multi-table `UPDATE` or `DELETE`
  (see [EXPLAIN ANALYZE and the slow-statement log](#explain-analyze-and-the-slow-statement-log)); `SHOW CREATE
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
  `YEAR_MONTH`) as integers; another unit is error 1064. `YEAR()`, `QUARTER()`, `MONTH()`,
  `DAY()`/`DAYOFMONTH()`, `HOUR()`, `MINUTE()`, `SECOND()`, `MICROSECOND()`, `DAYOFWEEK()`
  (1 = Sunday), `DAYOFYEAR()` and `WEEKDAY()` (0 = Monday) read a date, datetime, timestamp,
  time (`HOUR('272:59:59')` is 272), a string in those forms or a number such as `20240203`;
  NULL, an invalid date or a string that is no date gives NULL, and the zero date
  `'0000-00-00'` gives 0 for its year, quarter, month and day and NULL for the day of the week
  or year, as MySQL's (D350). `TIMESTAMP '...'` and `DATE '...'`
  literals. A binary-protocol row with a date outside years 0 to 9999 is error 1292.
- **Subqueries, `WITH` and set operations** as in "What runs today", with MySQL's rules: a
  derived table needs an alias (error 1248) and unique column names (error 1060), `WITH RECURSIVE` stops after 1,000 steps (error 3636,
  `cte_max_recursion_depth`'s default), there is no `FULL JOIN` (`FULL` is not a keyword, 1064)
  and no `MINUS`, a set operation of numbers and strings gives strings, and the errors are 1242
  ("Subquery returns more than 1 row"), 1241 ("Operand should contain 1 column(s)") and 1222.
  `GREATEST` and `LEAST` are NULL when any argument is.
- **Functions and operators**: `IFNULL`, `IF`, `CONCAT`, `LENGTH` (bytes), `CHAR_LENGTH`,
  `UPPER`/`UCASE`, `LOWER`/`LCASE`, `COALESCE`, `NULLIF`, `ABS`, `SQRT` (a `DOUBLE`, NULL for
  a negative argument; D350), `DATABASE()`, `SCHEMA()`,
  `VERSION()`, `USER()`, `CURRENT_USER()`, `CONNECTION_ID()`, `LAST_INSERT_ID()`, `@@var`
  (`@@version`, `@@sql_mode`, `@@max_allowed_packet`, ...), `@var`, `DIV`, `MOD`, `/` in
  floating point, `<=>`, `||` and `&&` as `OR` and `AND`, `!`. A result column without an alias
  is named by its text (`SELECT 1+1` gives a column `1+1`). `STD`, `STDDEV`, `STDDEV_POP`,
  `STDDEV_SAMP`, `VARIANCE`, `VAR_POP` and `VAR_SAMP` return `DOUBLE` (D321).
- **NULL ordering**: NULL sorts first for `ORDER BY ... ASC` and last for `DESC`, as in MySQL,
  in queries and window definitions; an index on a column that may hold NULLs is read in two
  ranges, its NULL entries first, in place of the sort (D321, D341).
- **Errors** with MySQL's numbers, SQLSTATEs and wording: `ERROR 1146 (42S02): Table
  'eggdb.missing' doesn't exist` (`eggdb` is the database name in `eggdb sql` and `eggdb shell`;
  through the server it is `--database`'s), 1064 for syntax errors (`... near '...' at line 1`), 1054,
  1062, 1048, 1050, 1051, 1049, 1193 and the rest. `SHOW WARNINGS` lists the last statement's
  warnings and notes, or its error.
- **Transactions as MySQL has them**: an error inside `START TRANSACTION` ... `COMMIT` rolls
  back only the failing statement, as InnoDB does: a multi-row `INSERT` or an `UPDATE` that
  fails part-way leaves none of its rows, and the transaction keeps what came before and goes
  on (D317); `SET autocommit = 0`
  keeps a transaction open from the first write to `COMMIT` or `ROLLBACK`; `CREATE TABLE`,
  `DROP TABLE` and `START TRANSACTION` commit an open transaction first. `START TRANSACTION READ
  ONLY` (and `WITH CONSISTENT SNAPSHOT`), `SET TRANSACTION READ ONLY | READ WRITE` for the next
  transaction (error 1568 while one is open) and `SET SESSION TRANSACTION READ ONLY` (the
  `transaction_read_only` variable) work as MySQL's: writes other than to temporary tables are
  error 1792 `Cannot execute statement in a READ ONLY transaction.` (D318).

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
  containing `INT` is an integer (`bigint`, 64 bits); `CHAR`, `CLOB` or `TEXT` gives `TEXT`
  affinity (lengths such as `VARCHAR(10)` are not enforced, as in SQLite); `BLOB` or no type at
  all gives `BLOB` affinity; `REAL`, `FLOA` or `DOUB` gives `REAL` affinity; the rest (`NUMERIC`,
  `DECIMAL(p,s)`, `BOOLEAN`, `DATE`, `DATETIME`) gives `NUMERIC` affinity. A column of any
  affinity but `INTEGER` holds every storage class, row by row, as SQLite's datatype page
  says (D153, D305): `TEXT` stores a number as its text (`5` becomes `'5'`, `2.0` `'2.0'`);
  `NUMERIC` stores text that is a well-formed integer or real as a number, preferring an integer
  (`'3.0e+5'` becomes `300000`; `'0x10'`, `'abc'` and `'2024-02-29'` stay text) and a real that
  is a whole number as an integer; `REAL` does the same and then makes every number a real (`7`
  becomes `7.0`); `BLOB` keeps every value as given. `typeof()` tells which class a value has,
  `quote()` writes a blob as `X'0A0B'`, `length()` counts a blob's bytes, and values of different
  classes compare and sort as SQLite orders them (numbers, then text, then blobs; an integer and
  a real compare by their exact values). A string compared with a column of `NUMERIC` or `REAL`
  affinity takes that affinity first, so `amount = '12'` finds the integer `12`. These columns
  can be keys, `UNIQUE` and indexed (D305): `3` and `3.0` are the same key, and a range such as
  `k > 2.5` takes the larger numbers, then all text, then all blobs, as SQLite's comparison
  does. `CAST` keeps its static types (`CAST(x AS TEXT)` is text, `AS REAL` a real). Blob
  literals are `x'0A0B'` (an even number of hexadecimal digits; anything else is `unrecognized
  token`).
- **Row ids**: a column declared exactly `INTEGER PRIMARY KEY` (on the column or as the table's
  one-column key) is the row id: a NULL or missing value takes one more than the largest in
  the table; `AUTOINCREMENT` is accepted on it (and refused elsewhere, as in SQLite);
  `last_insert_rowid()`, `changes()` and `total_changes()` report as SQLite does.
- **Statements**: `CREATE TABLE [IF NOT EXISTS]` with `WITHOUT ROWID` and `STRICT` accepted,
  `REFERENCES` and `FOREIGN KEY` accepted and not enforced (SQLite's default, `PRAGMA
  foreign_keys` off), `ON CONFLICT ABORT|FAIL` in constraints; `INSERT OR
  REPLACE|IGNORE|ABORT|FAIL|ROLLBACK`, `REPLACE INTO`, upserts (`ON CONFLICT [(cols [ASC|DESC])
  [WHERE expr]] DO NOTHING` and `DO UPDATE SET ... excluded.col`; D324), `RETURNING`, `UPDATE ... SET ... FROM ...`
  as in PostgreSQL (D297); `LIMIT n OFFSET m`, `LIMIT m, n`
  and a negative `LIMIT` for no limit; `BEGIN [DEFERRED|IMMEDIATE|EXCLUSIVE] [TRANSACTION]`,
  `COMMIT`, `END`, `ROLLBACK`, with SQLite's errors for a `BEGIN` inside a transaction and a
  `COMMIT` or `ROLLBACK` outside one; a failing statement inside a transaction backs out its
  own changes and leaves the transaction open (SQLite's default `ABORT`, also `INSERT OR
  ABORT`), `INSERT OR FAIL` keeps the rows it wrote before the conflict (in a transaction and
  on its own), and `INSERT OR ROLLBACK` rolls the transaction back on a conflict (D317, D338);
  `UPDATE OR ROLLBACK|ABORT|FAIL|IGNORE|REPLACE` likewise, row by row for `FAIL` (the rows
  changed before the conflict are kept), `IGNORE` (conflicting rows are left as they were) and
  `REPLACE` (the rows in the way are deleted, and a `NULL` for a `NOT NULL` column takes its
  default), with SQLite's `UNIQUE constraint failed: t.c` (D338), with `FROM` too, and on a view
  through its `INSTEAD OF` trigger (D373).
  Since `UPDATE OR REPLACE` deletes rows, a checked role needs `DELETE` on the table too
  (`42501` without it), and where a row-level security `DELETE` policy applies to the role it is
  refused (`0A000`), as `REPLACE` and `INSERT OR REPLACE` are (D338).
- **Names starting with `sqlite_`** are reserved, as in SQLite: `CREATE TABLE sqlite_x` is
  refused with `object name reserved for internal use`.
- **Length limit**: a function result longer than 1,000,000,000 bytes (SQLite's default
  `SQLITE_MAX_LENGTH`) is refused with `string or blob too big` (`SQLITE_TOOBIG`, 18) before it
  is built.
- **The schema table**: `sqlite_schema` and `sqlite_master` (with `type`, `name`, `tbl_name`,
  `rootpage` and `sql`) list the tables; `sqlite_temp_schema`/`sqlite_temp_master` are empty.
  A table, index or view created in SQLite's dialect keeps its `CREATE` text as SQLite stores
  it: as written from the object's name on, after `CREATE TABLE `, `CREATE [UNIQUE] INDEX ` or
  `CREATE VIEW ` (without `IF NOT EXISTS` or a schema before the name); triggers are shown the
  same way. `ALTER TABLE ... ADD COLUMN` edits that text as SQLite does, inserting the column
  as written after the last column definition, and `RENAME TO` writes the new name in double
  quotes in the table's text and its indexes' `ON` (D294). After any other `ALTER TABLE`, and
  for objects made in another dialect, `sql` is generated from the definition. Views are
  listed with `rootpage` 0. `eggdb shell --dialect sqlite` prints these texts for `.schema [table]`, each
  followed by `;`, with the table's indexes and triggers.
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
  case, `LIKE ... ESCAPE 'c'` and `like(pattern, text, escape)`, `GLOB` with `*`, `?` and
  character classes (`[abc]`, `[a-z]`, `[^0-9]`, `]` first and `-` first or last as themselves,
  an unclosed `[` matching nothing) and with a pattern from a column or a subquery, `NATURAL
  [LEFT | RIGHT | FULL] JOIN`, integers as conditions (`WHERE active`, `NOT 0`), `TRUE` and
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
  `instr`, `abs`, `round`, `sqrt` (NULL for a negative argument or text that is no number;
  D350), scalar `min` and `max`, `random`, `hex`, `quote`, `printf` and
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
- **NULL ordering**: NULL is smaller than any other value, so it sorts first for `ORDER BY ...
  ASC` and last for `DESC`, in queries, compound selects and window definitions, unless
  `NULLS FIRST` or `NULLS LAST` says otherwise; an index on a column that may hold NULLs is
  read in two ranges, its NULL entries first, in place of the sort (D340, D341).
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
  `GROUP BY` and `HAVING` (with `ROLLUP`, `CUBE`, `GROUPING SETS`, `GROUPING`, `GROUPING_ID`
  and `GROUP_ID()`, which numbers the repeats of a grouping set from 0, D298), `LIKE`
  (case-sensitive), `!=`, `^=` and `~=` for `<>`, `q'[...]'`
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
  `NLS_DATE_LANGUAGE` with American, German, French, Spanish, Italian, Portuguese or Dutch:
  `MON`, `MONTH`, `DY` and `DAY` write and read that language's names and Oracle's
  abbreviations (`MRZ`, `FÉVR.`, `MIÉ`), `MONTH` and `DAY` padded to the language's longest
  name; `NEXT_DAY` takes the session language's day names.
- **Number format models** (D126, D149): `TO_CHAR(n, fmt)` and `TO_NUMBER(char, fmt
  [, nlsparam])` with `9`, `0`, `.`, `D`, `,`, `G`, `$`, `L`, `C`, `U`, `B`, `S` (leading or
  trailing), `MI`, `PR`, `V`, `EEEE`, `RN`/`rn`, `X`/`x` (with leading `0`s), `TM`, `TM9`, `TME`
  and `FM`, as the Format Models page's "Results of Number Conversions" table shows them
  (`TO_CHAR(123.456, '9.9EEEE')` is `  1.2E+02`, `FM999.009` keeps the zeros of its `0`s);
  `TO_NUMBER` reads `$`, `L`, `C`, `U`, `PR`, trailing `S`, `EEEE`, `RN` and `X`, and refuses
  `V` with `ORA-01481`. `L` writes and reads `NLS_CURRENCY`, `C` the code of
  `NLS_ISO_CURRENCY`'s territory (`GERMANY` is `EUR`), `D` and `G` the two
  `NLS_NUMERIC_CHARACTERS`, while `.` and `,` stay themselves; the `nlsparam` of `TO_CHAR` and
  `TO_NUMBER` sets any of those for the call (`'NLS_NUMERIC_CHARACTERS = '',.'''`), and a bad
  one is `ORA-12702`. The dual currency stays `$`.
- **Session settings** (D126): `ALTER SESSION SET` `NLS_DATE_FORMAT`, `NLS_TIMESTAMP_FORMAT`,
  `NLS_TIMESTAMP_TZ_FORMAT`, `NLS_LANGUAGE`/`NLS_DATE_LANGUAGE` (the seven languages above;
  an unknown one is `ORA-12705`), `NLS_CURRENCY` (up to ten bytes), `NLS_ISO_CURRENCY` (a
  territory name), `NLS_NUMERIC_CHARACTERS` (two different characters, neither a digit nor
  `+`, `-`, `<`, `>`), `NLS_TERRITORY` (America, which resets the formats, currencies and
  numeric characters it implies),
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
- **Hierarchical queries** (D304): `[START WITH condition] CONNECT BY [NOCYCLE] condition` (in
  either order, after `WHERE`) walks the rows of the `FROM` table, view, subquery or `DUAL` depth
  first from the rows `START WITH` finds (every row without it), as Oracle returns them. `PRIOR
  expr` reads the parent row in `CONNECT BY` and the select list (NULL for a root); `LEVEL` (also
  in `CONNECT BY`: `SELECT LEVEL FROM dual CONNECT BY LEVEL <= 10`), `CONNECT_BY_ROOT expr`,
  `CONNECT_BY_ISLEAF`, `CONNECT_BY_ISCYCLE` (`ORA-30930` without `NOCYCLE`) and
  `SYS_CONNECT_BY_PATH(expr, 'separator')` (a constant separator, `ORA-30003`; a value holding it
  is `ORA-30004`; a path longer than 4,000 bytes is `ORA-01489`) can be selected, filtered, grouped and sorted. `WHERE` filters after the walk,
  so a filtered-out row's descendants stay. `ORDER SIBLINGS BY` sorts the rows under each parent
  (and the roots) and keeps the hierarchy; elsewhere it is `ORA-30929`. A loop (a child with the
  `PRIOR` values of one of its ancestors) is `ORA-01436`; with `NOCYCLE` the walk stops there and
  marks the row with `CONNECT_BY_ISCYCLE = 1`. The pseudo-columns without `CONNECT BY` are
  `ORA-01788`. `CONNECT BY` over a join, `PRIOR` beside a subquery and `ORDER SIBLINGS BY` a
  pseudo-column answer `0A000` for now.
- **Multitable inserts** (D304): `INSERT ALL INTO t [(columns)] [VALUES (...)] [INTO ...]
  subquery`, `INSERT ALL WHEN condition THEN INTO ... [WHEN ...] [ELSE INTO ...] subquery` (every
  `WHEN` that holds) and `INSERT FIRST WHEN ... THEN INTO ... [ELSE INTO ...] subquery` (the first
  that holds). The subquery runs once; `WHEN` and `VALUES` read its columns, `VALUES` takes
  `DEFAULT`, and an `INTO` without `VALUES` takes the subquery's columns in order. It is one
  statement: a failing target undoes them all, and the result counts every row inserted.
- **Indexes**: `CREATE [UNIQUE] INDEX name ON t (...)` with `TABLESPACE`, `ONLINE`, `LOGGING`
  and `COMPUTE STATISTICS` accepted, `DROP INDEX name`, `UNIQUE` columns and `CONSTRAINT name
  UNIQUE (...)` (an unnamed one is `SYS_C` and seven digits), `USER_INDEXES`/`ALL_INDEXES`
  (`INDEX_NAME`, `INDEX_TYPE`, `TABLE_NAME`, `UNIQUENESS`) and
  `USER_IND_COLUMNS`/`ALL_IND_COLUMNS`; `ORA-00001` for a duplicate, `ORA-01452` when `CREATE
  UNIQUE INDEX` finds one, `ORA-00955` for a name in use and `ORA-01418` for a missing index.
- **Identity columns**: `GENERATED [ALWAYS | BY DEFAULT [ON NULL [FOR INSERT ONLY | FOR INSERT
  AND UPDATE]]] AS IDENTITY [(options)]` on a column of the primary key, in a
  partitioned table too (D106, D150, D337, D364).
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
  before and after them. A failing statement rolls back only its own changes, as Oracle's
  implicit savepoint before each statement does, and the transaction goes on (D317). `SET
  TRANSACTION READ ONLY [NAME 'name']` starts a read-only transaction that reads one snapshot
  until `COMMIT` or `ROLLBACK` and refuses changes with `ORA-01456: may not perform
  insert/delete/update operation inside a READ ONLY transaction`; a `CREATE TABLE` or another
  statement that commits implicitly ends it first. `SET TRANSACTION` once a transaction is open
  is `ORA-01453: SET TRANSACTION must be first statement of transaction` (D318). `eggdb
  sql` and `eggdb shell` commit an open transaction when they end, as SQL*Plus does on exit;
  the embedded API rolls it back on `close` unless you commit.
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
  `REPLACE`, `CHR`, `ASCII`, `HEXTORAW`, `RAWTOHEX`, `ABS`, `ROUND`, `TRUNC`, `MOD`, `CEIL`,
  `FLOOR`, `POWER`, `SQRT`,
  `SIGN`, `GREATEST`, `LEAST` (NULL if any argument is NULL), `TO_CHAR` for numbers (format
  elements `9`, `0`, `.`, `D`, `,`, `G`, `$`, `S`, `MI`, `FM`; `#`s when the number does not
  fit) and for dates, `TO_NUMBER`, `TO_DATE`, `TO_TIMESTAMP`, `ADD_MONTHS`, `LAST_DAY`, the aggregates `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`, and `USER`. Other
  dialects' functions are not there (`CHAR_LENGTH` is `ORA-00904`), and Oracle's are not in the
  other dialects. `LISTAGG([ALL | DISTINCT] expr [, delimiter] [ON OVERFLOW ERROR | TRUNCATE
  ['indicator'] [WITH | WITHOUT COUNT]]) [WITHIN GROUP (ORDER BY ...)]` joins a group's values
  (NULLs left out, NULL for none); past 32,767 bytes it is `ORA-01489`, or truncated at the last
  whole value with the indicator (`...`) and the count of values left out (D149). The analytic
  form `LISTAGG(...) [WITHIN GROUP (ORDER BY ...)] OVER ([PARTITION BY ...])` gives each row its
  partition's values joined in `WITHIN GROUP` order; `ORDER BY` inside `OVER` is `ORA-30487` and
  `DISTINCT` there answers `0A000` for now (D300).
- **Analytic functions** (D332): `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `PERCENT_RANK`, `CUME_DIST`,
  `NTILE`, `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE` and `NTH_VALUE`, and every aggregate, with
  `OVER ([PARTITION BY ...] [ORDER BY ...] [ROWS | RANGE ...])`, computed by the same window code
  as the other dialects (D151). `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE` and `NTH_VALUE` take
  `IGNORE NULLS` or `RESPECT NULLS` inside the parentheses after the first argument or after
  them (`LAG(x IGNORE NULLS, 2)`, `LAG(x, 2) IGNORE NULLS OVER (...)`), and `NTH_VALUE(x, n)
  FROM FIRST | FROM LAST`. The ranking functions and `LAG`/`LEAD` without `ORDER BY` in their
  window are `ORA-30485`, any of them without `OVER` is `ORA-30484`, the wrong number of
  arguments `ORA-00909`. `RATIO_TO_REPORT(expr) OVER ([PARTITION BY ...])` is each value over its
  partition's sum (NULL when the value is NULL or the sum is 0; `ORDER BY` in its `OVER` is
  `ORA-30487`). `MIN`, `MAX`, `SUM`, `AVG`, `COUNT`, `VARIANCE` and `STDDEV` (and their `_SAMP`
  and `_POP` forms) take `KEEP (DENSE_RANK FIRST | LAST ORDER BY ...)`, aggregating only the rows
  that rank first or last in that order, ties included, in a group or with `OVER ([PARTITION BY
  ...])`.
- **Regular expressions** (D332): `REGEXP_LIKE(source, pattern [, match_parameter])`,
  `REGEXP_COUNT(source, pattern [, position [, match_parameter]])`, `REGEXP_INSTR(source, pattern
  [, position [, occurrence [, return_option [, match_parameter [, subexpression]]]]])`,
  `REGEXP_SUBSTR(source, pattern [, position [, occurrence [, match_parameter [,
  subexpression]]]])` and `REGEXP_REPLACE(source, pattern [, replace_string [, position [,
  occurrence [, match_parameter]]]])`, on the regular expression engine of D128 and D130 (POSIX
  classes such as `[[:digit:]]`, `\d`, `\w`, `\s`, lazy quantifiers, back references `\1` to
  `\9` in patterns and in `replace_string`). The match is Oracle's: the leftmost one, and there
  the first the pattern finds in its own order (alternatives left to right, greedy quantifiers
  longest first, lazy ones shortest first), so `REGEXP_SUBSTR('ab', 'a|ab')` is `a` where
  PostgreSQL's `regexp_substr` gives `ab`. `match_parameter` takes `i`, `c`, `n` (`.` matches a
  newline, which it does not by default), `m` (`^` and `$` at each line) and `x` (spaces in the
  pattern ignored); the last of `i` and `c` wins and another letter is `ORA-01760`. `occurrence`
  0 in `REGEXP_REPLACE` (the default) replaces every match. A position or occurrence below 1, or a
  subexpression outside 0 to 9, is `ORA-01428`. An empty source, pattern or result is NULL, as
  Oracle's empty strings are; `REGEXP_REPLACE` with a NULL pattern gives the source back.
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
- `SELECT TOP n` and `TOP (expression)`, with `PERCENT` (a percent of the rows the query finds,
  rounded up; outside 0 to 100 is error 1031) and `WITH TIES` (the rows after the last one that
  sort equal to it; without `ORDER BY` it is error 1062) (D300); `N'...'` strings; `--` and nested `/* */` comments;
  the operators `+ - * / % = <> != < > <= >= !< !>`.
- **NULL ordering**: NULL is the lowest value, so it sorts first for `ORDER BY ... ASC` and last
  for `DESC`, in queries, `UNION`s, window definitions and `STRING_AGG ... WITHIN GROUP`; an
  index on a column that may hold NULLs is read in two ranges, its NULL entries first, in place
  of the sort (D340, D341).
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
  scale, with truncation as its third argument), `CEILING`, `FLOOR`, `SQRT` (a `float`; error
  3623 `An invalid floating point operation occurred.` for a negative argument, D350), `NEWID()`,
  `SCOPE_IDENTITY()`, `UPPER`, `LOWER`, `ABS`, `COALESCE`, `NULLIF` and the aggregates:
  `COUNT` returning `int` and `COUNT_BIG` `bigint`, `SUM` and `AVG` with SQL Server's result
  types (`int` for `tinyint`, `smallint` and `int`, `bigint`, `decimal(38, s)` and for `AVG`
  `decimal(38, max(s, 6))`, `float`; error 8115 past the type), `STRING_AGG` with `WITHIN GROUP
  (ORDER BY ...)`, also as a window function with `OVER ([PARTITION BY ...])` (the form Azure
  Synapse documents; each row gets its partition's values joined in `WITHIN GROUP` order, D300).
  `STRING_SPLIT(text, separator [, enable_ordinal])` is a table with a `value`
  column (and `ordinal`), and a one-character separator (error 214 otherwise). Dividing
  decimals gives SQL Server's precision and scale (`2 / 3.0` is `0.666666`, `10.0 / 4` is
  `2.500000000000`). `@@VERSION`, `@@SPID`, `@@SERVERNAME`, `@@TRANCOUNT`, `@@LANGUAGE`,
  `@@ROWCOUNT`, `@@IDENTITY`, `@@ERROR`. Another function is error 195, another `@@` name error
  137, and so is an `@variable` unless the batch declares it or it is a parameter of a
  parameterized batch (`sp_executesql` and the prepared statements of SqlClient and mssql-jdbc,
  item 23b).
- `GROUP BY ROLLUP (...)`, `CUBE (...)`, `GROUPING SETS (...)` and `()`, the older `GROUP BY a, b
  WITH ROLLUP` and `WITH CUBE`, `GROUPING(column)` and `GROUPING_ID(column, ...)` (D298).
- `ORDER BY ... OFFSET n ROWS [FETCH {FIRST | NEXT} m ROWS ONLY]`, `OUTPUT inserted.*` on
  `INSERT` and `UPDATE` and `OUTPUT deleted.*` on `DELETE` (columns, expressions and constants
  such as EF Core's `OUTPUT 1`), `INSERT ... [OUTPUT ...] SELECT ...`, `(VALUES (...), ...) AS
  alias (columns)` as a table, and `SELECT total = qty * price`.
- `UPDATE target SET [alias.]column = expr, ... [OUTPUT ...] FROM table_source [WHERE ...]` and
  `DELETE [FROM] target [OUTPUT ...] FROM table_source [WHERE ...]` (D297): the target is the
  table in `FROM` whose alias it names, or the one reference to it there; otherwise it joins the
  `FROM` list as one more table, as in PostgreSQL. A target row that joins several rows is
  changed once, from one of them, as SQL Server's manual warns. `OUTPUT inserted.` (and, in a
  `DELETE`, `deleted.`) names the target's row and the other tables' columns can be output too;
  `deleted.` in a joined `UPDATE` is Msg 40517 for now, as in a single-table one. A column
  qualified by another name is Msg 4104, and a target named by its table name when the `FROM`
  lists that table more than once, each under an alias, is Msg 8154.
- **`MERGE`** (D138): `MERGE [TOP (n) [PERCENT]] [INTO] target [[AS] alias] USING source [[AS]
  alias [(columns)]] ON condition`, then up to two `WHEN MATCHED [AND condition] THEN UPDATE SET
  ... | DELETE` (D303), one `WHEN NOT MATCHED [BY TARGET] [AND condition] THEN INSERT [(columns)]
  VALUES (...)`, and up to two `WHEN NOT MATCHED BY SOURCE [AND condition] THEN UPDATE SET ... |
  DELETE` for target rows no source row matches (the source's columns are NULL there) (D291).
  Of two `WHEN MATCHED` or two `WHEN NOT MATCHED BY SOURCE` clauses the first needs `AND`, and
  they are one `UPDATE` and one `DELETE`; a matched or unmatched row takes the first clause whose
  condition holds. Anything else, and a second `WHEN NOT MATCHED`, is Msg 10714. `TOP (n)` changes
  at most `n` rows, `TOP (n) PERCENT` that share of the rows the clauses would change, rounded up
  (Msg 1031 past 100): the rows of `WHEN MATCHED` first, then those of `BY SOURCE`, then the
  inserted ones, as SQL Server leaves the order unspecified. `OUTPUT` takes `inserted.`,
  `deleted.`, the source's columns and `$action` (`INSERT`, `UPDATE` or `DELETE`, as
  `nvarchar(10)`), in the same order, and `OUTPUT ... INTO` as below; the tag is `MERGE n`. Table
  hints are 40517, and a `MERGE` without `WHEN` is Msg 10713.
- **`OUTPUT ... INTO {@table_variable | table} [(columns)] [OUTPUT ...]`** on `INSERT`, `UPDATE`,
  `DELETE` and `MERGE` (D303): the rows the `OUTPUT` list makes go into the table variable or the
  table (a `#temporary` one too) instead of the client, and a second `OUTPUT` list after it still
  returns rows. The statement and the rows it adds are one change: when the rows break the
  target's key, `NOT NULL` or types, or the target is missing (Msg 208), nothing the statement did
  is kept. A column count that does not match the target's is Msg 213.
- **`SELECT ... INTO new_table [FROM ...]`** (D303): creates `new_table` with the query's columns
  (their names and types, without keys, defaults or identity; a column copied directly from the
  one table of the query keeps its `NOT NULL`, every other column is nullable, D342) and fills it
  with the query's rows; the tag is `SELECT n`. `#name` makes a temporary table. An existing name
  is Msg 2714, a column without a name Msg 1038, and `INTO` in a later query of a `UNION`,
  `INTERSECT` or `EXCEPT` Msg 196; in a subquery it is 40517.
- **`OPENJSON(json [, path]) WITH (column type ['path'] [AS JSON], ...) [AS] alias`** in `FROM`
  (D138): one row per element of a JSON array (an object is one row), each column the value at
  its path (`$`, `$.key`, `$."key"`, `$[n]`, `lax`; without a path the key named as the column
  is written) converted to its type, NULL when missing or, without `AS JSON`, when it is an
  object or array; text that is not an array or object is Msg 13609, a bad path Msg 13607.
  Without `WITH` (D291), `OPENJSON(json [, path]) [AS] alias` returns SQL Server's default
  schema: one row per array element or object member with `key` (the index from 0, or the
  member's name), `value` (the text of a scalar, NULL for `null`, the JSON text of an array or
  object) and `type` (0 null, 1 string, 2 number, 3 `true`/`false`, 4 array, 5 object).
  `strict` paths and `CROSS APPLY` are 40517.
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
  open transaction back and ends the batch (`@@TRANCOUNT` 0); `OFF` (the default) ends only the
  statement for the errors SQL Server ends only the statement for (a duplicate key, `NOT NULL`,
  `CHECK` and foreign-key violations, truncation): the statement's own changes are rolled back,
  a multi-row `INSERT` leaving none of its rows, and the transaction and the batch go on
  (D317). A run-time error that ends the batch (a conversion or arithmetic error) rolls the
  transaction back, whether or not its statement had changed rows, and inside `TRY` dooms it
  (D371). Syntax errors and `RAISERROR` are not affected,
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

## MySQL stored procedures and functions

In the MySQL dialect, `CREATE PROCEDURE` and `CREATE FUNCTION` take MySQL's syntax (D199):
`DEFINER = ...`, `IN`, `OUT` and `INOUT` parameters, `RETURNS` with a MySQL type, and the
characteristics (`DETERMINISTIC`, `CONTAINS SQL`, `READS SQL DATA`, `MODIFIES SQL DATA`, `SQL
SECURITY`, `COMMENT`, `LANGUAGE SQL`). The body is one statement or a compound statement:

- `[label:] BEGIN ... END [label]` with `DECLARE name[, ...] type [DEFAULT expr]` first;
- `SET local = expr[, ...]`, `SELECT ... INTO local[, ...]` and any SQL statement;
- `IF ... THEN ... ELSEIF ... ELSE ... END IF` and both forms of `CASE ... END CASE` (no
  matching branch and no `ELSE` fails with SQLSTATE `20000`, as in MySQL);
- `[label:] WHILE ... DO ... END WHILE`, `REPEAT ... UNTIL ... END REPEAT`, `LOOP ... END LOOP`,
  with `LEAVE label` and `ITERATE label`;
- `RETURN expr` in functions and `SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = ...`;
- `DECLARE CONTINUE | EXIT HANDLER FOR ...` (error numbers, SQLSTATEs, named conditions,
  `SQLEXCEPTION`, `SQLWARNING`, `NOT FOUND`), `DECLARE name CONDITION FOR ...`, cursors
  (`DECLARE c CURSOR FOR`, `OPEN`, `FETCH ... INTO`, `CLOSE`), `RESIGNAL` and `GET DIAGNOSTICS`
  (D418).

`CALL name(...)` (or `CALL name` without parentheses) runs a procedure; an `OUT` or `INOUT`
argument written as a user variable (`@total`) receives the value. When a statement inside the
procedure fails, only that statement is undone: the statements before it keep their changes
(committed in autocommit mode), as in MySQL (D370). `DROP PROCEDURE` and `DROP
FUNCTION` take `IF EXISTS`. User and system variables set inside a body
reach the session when the `CALL` ends (D419); a stored function called from a `SELECT` can't
set them yet. Not yet: events.

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

A client's attention (cancel) stops the running batch or `sp_executesql` call at once: a watcher
thread notices it on the socket while the request runs, the request ends without an error, and
its last `DONE` token carries the attention acknowledgement. Not yet: TLS, MARS, and SSMS's and Azure Data Studio's own catalog queries (the `sys`
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
07:08:09.123` for `datetime2`). Three switches pick bcp's other layouts:

- `--unicode` reads bcp's Unicode character format (`bcp ... -w`): UTF-16LE with or without a
  byte order mark, with the same terminators (`eggdb_sql::BcpUnicodeInput` for embedders).
- `--native` reads bcp's native format (`bcp ... -n`), each column laid out as `bcp format ...
  -n` lays out the SQL Server type EggDB reports for it in `sys.columns`: `smallint`, `int`,
  `bigint`, `bit`, `real`, `float`, `datetime` and `uniqueidentifier` in their fixed sizes (with
  a one-byte length prefix when the column allows NULL), `decimal`, `numeric` and the date and
  time types with a one-byte prefix, `nvarchar(n)` and `nchar(n)` as UTF-16 with a two-byte
  prefix, and `nvarchar(max)` and `varbinary(max)` with an eight-byte prefix; a prefix of all
  `0xFF` bytes is NULL. A column of a type with no native layout (an interval, an array, a
  user-defined type) is an error (`0A000`) naming the column. EggDB keeps the engine type, not
  the declared SQL Server type, so `tinyint`, `varchar`, `char`, `text`, `money`, `smallmoney`,
  `smalldatetime`, `binary(n)`, `varbinary(n)` and `image` columns are reported (and laid out) as
  `smallint`, `nvarchar`, `nchar`, `nvarchar(max)`, `decimal`, `datetime2(0)` and
  `varbinary(max)`; a native file SQL Server wrote from a table with such columns lays them out
  differently, so load it with the format file `bcp format ... -n -f` writes on the source
  server (`--format-file`).
- `--format-file <file>` reads the data as a bcp format file (`bcp ... -f`) lays it out, non-XML
  (version line, field count, then order, host type, prefix length, length, quoted terminator,
  server column order, name and collation per field) or XML (`<BCPFORMAT>` with `<RECORD>` and
  `<ROW>`); a field with server column order 0, or one no `<COLUMN>` names, is skipped.
  `SQLNCHAR` terminators are UTF-16 bytes as `bcp format ... -w` writes them (`"\t\0"`,
  `"\r\0\n\0"`); one that is not (`"\t"`) is widened to UTF-16. `SQLCHAR` data is read as UTF-8
  (bcp's `-C 65001`), so text in another code page with bytes over 127 is error 4864. A format
  file that cannot be read, or one with no fields or a field with no prefix, no terminator and
  a length of 0 (a row that reads no bytes), is a usage error naming what is wrong (SQL
  Server's 4823).

They go with `--dialect sqlserver` and one at a time; `--delimiter` and `--row-terminator` go
only with the character layouts. Embedders use `eggdb_sql::BcpFileReader` (native or a
`BcpFormat`) as the `BulkRows` of `Session::execute_bulk_load`, and `eggdb_sql::write_bcp_table`
writes a table in the character, Unicode or native layout as `bcp ... out` does, with
`BcpFormat::to_format_file` for its format file (`SQLNCHAR` terminators written as `"\t\0"`).
Not yet: `eggdb export` writing bcp files, and `SQLCHAR` data in code pages other than UTF-8.

`BULK INSERT t FROM 'file' WITH (...)` and `OPENROWSET(BULK 'file', ...)` parse with every option
SQL Server documents (`FORMAT = 'CSV'`, `FIELDTERMINATOR`, `ROWTERMINATOR`, `FIRSTROW`,
`LASTROW`, `FORMATFILE`, `CHECK_CONSTRAINTS`, `KEEPNULLS`, `KEEPIDENTITY`, `TABLOCK`, `ORDER`,
`SINGLE_BLOB`, `SINGLE_CLOB`, `SINGLE_NCLOB` and the rest; an unknown option is error 195), and
then are refused with error 4834 (SQLSTATE 42501), as `COPY ... FROM 'file'` is: EggDB never reads
a file on the server from SQL, so a client can't read the server's files through it. Load the file
from the client instead, with `bcp in`, `SqlBulkCopy` or `eggdb import`. Loads check every
constraint: SQL Server skips `CHECK` and foreign key constraints without `CHECK_CONSTRAINTS` and
marks them untrusted, which EggDB has no notion of, so it always checks them.

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
  row by row (`SELECT @names += name + N',' FROM t`). A value that reads the variable it assigns
  is worked out row by row too, so `SELECT @s = @s + name FROM t ORDER BY id` and `SELECT @csv =
  COALESCE(@csv + N',', N'') + name FROM t` concatenate (without aggregates, subqueries or
  `DISTINCT` in that value; reading another variable the same `SELECT` assigns stays 40517).
- **Cursors**: `DECLARE name CURSOR [LOCAL | GLOBAL] [FORWARD_ONLY | SCROLL] [STATIC | KEYSET |
  DYNAMIC | FAST_FORWARD] [READ_ONLY | SCROLL_LOCKS | OPTIMISTIC] [TYPE_WARNING] FOR select [FOR
  READ ONLY]` (and the ISO form `DECLARE name [INSENSITIVE] [SCROLL] CURSOR FOR ...`), `OPEN`,
  `FETCH [NEXT | PRIOR | FIRST | LAST | ABSOLUTE n | RELATIVE n] FROM [GLOBAL] name [INTO @a, @b,
  ...]` (`n` a number or a variable; without `INTO` the row comes back as a result set),
  `@@FETCH_STATUS` (0, or -1 past either end; -1 before the first fetch), `@@CURSOR_ROWS`,
  `CLOSE`, `DEALLOCATE` and `CURSOR_STATUS('local' | 'global' | 'variable', 'name')` (1, 0, -1,
  -3). Every cursor reads its rows when it is opened, as a `STATIC` cursor does, so later changes
  to the tables do not show in it; variables in its query keep the values they had at `DECLARE`.
  Cursors are `GLOBAL` unless declared `LOCAL`; a local one ends with its batch, procedure or
  dynamic SQL. Only `NEXT` moves a forward-only (or `FAST_FORWARD`) cursor (Msg 16911); the other
  mistakes are SQL Server's errors (16905, 16915, 16916, 16917, 16924). A connection holds at most
  1,000 cursors. Not yet: cursor variables (`DECLARE @c CURSOR`), `FOR UPDATE` and `WHERE CURRENT
  OF`, and `sp_cursor*` server cursors over RPC.
- **`GOTO label`** jumps to `label:` anywhere in the batch or procedure, forward or backward (an
  unknown label is Msg 133, a repeated one 132, and a jump into a `TRY` or `CATCH` block 1026).
- **`WAITFOR DELAY 'hh:mm[:ss[.fff]]'`** waits that long and **`WAITFOR TIME 'hh:mm[:ss]'`**
  until that time of day (session time zone); a cancel (TDS attention, PostgreSQL cancel
  request) or the statement timeout ends the wait, and a malformed time is Msg 148.
  `WAITFOR (RECEIVE ...)` is 40517.
- **Control of flow**: `IF condition statement [ELSE statement]` with any condition (subqueries,
  `[NOT] EXISTS (...)`, `OBJECT_ID(...) IS NULL`, `@@ROWCOUNT`), `BEGIN ... END`, `WHILE
  condition statement` with `BREAK` and `CONTINUE`, and `RETURN`. A condition leaves
  `@@ROWCOUNT` as the statement before it set it.
- **Errors**: inside `BEGIN TRY ... END TRY BEGIN CATCH ... END CATCH` an error moves to the
  `CATCH` block instead of reaching the client; there `ERROR_NUMBER()`, `ERROR_SEVERITY()`,
  `ERROR_STATE()`, `ERROR_LINE()`, `ERROR_MESSAGE()` and `ERROR_PROCEDURE()` (the procedure
  the error was raised in, NULL outside one) describe it,
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
- **Limits**: procedures, functions, triggers and the dynamic SQL they run share one nesting
  count of at most 32 levels (Msg 217), as SQL Server's `@@NESTLEVEL` does.

Inside a procedure `@@PROCID` is its object id (so `OBJECT_NAME(@@PROCID)` is its name; 0
outside a procedure), and `ERROR_PROCEDURE()` in a `CATCH` block names the innermost procedure
the caught error came from; over TDS the `ERROR` token carries that procedure's name too.

`CROSS APPLY dbo.f(t.id)` and `OUTER APPLY` call a table-valued function (inline or
multi-statement) whose arguments read the columns of an earlier `FROM` item: the function runs
once for each distinct set of argument values. `CREATE PROCEDURE #name` makes a temporary
procedure that only its session sees and that goes away with the session (`DROP PROCEDURE #name`
drops it sooner); temporary functions are refused, as SQL Server refuses them.

Not yet: a global temporary procedure (`##name`) is created like a `#name` one, visible only to
its own session; a procedure called by its name alone as the first statement of a batch (write
`EXEC`), `EXEC @result = dbo.scalar_function`, numbered procedures (`name;2`), CLR routines (`AS
EXTERNAL NAME`), and `COMMIT` or `ROLLBACK` of the caller's transaction inside a function.

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
  next to Oracle's `:name` and `:1`, and over TDS the `@name` parameters of `sp_executesql`,
  `sp_prepare` and `sp_prepexec` (`@P0`, as mssql-jdbc and SqlClient write them) are bound by
  name in Oracle SQL too. A stored procedure called by name over RPC (mssql-jdbc's
  `{call p(?, ?)}`, SqlClient's `CommandType.StoredProcedure`) runs the PL/SQL procedure of that
  name when the connection reads Oracle SQL and one exists (`log_it`, `hr.log_it`,
  `"HR"."LOG_IT"`, `pkg.proc`), and otherwise the Transact-SQL procedure, its parameters given
  by position or by name (`@p_n` for `p_n`, in any case and order) and the omitted ones taking
  their defaults; a PL/SQL procedure's `OUT` and `IN OUT` parameters that the call marks as
  output come back as RPC return values, as SQL Server's output parameters do. Over the
  PostgreSQL protocol, Oracle's `CALL p(...)` binds by position or with `=>`, `$1` included, and
  returns the `OUT` and `IN OUT` parameters as one row, as PostgreSQL does. A TDS batch that
  executes one of SQL Server's catalog procedures (`EXEC sp_tables ...`, mssql-jdbc's
  `INSERT ... EXEC sp_columns_100 ...` for `getColumns`, and the same procedures called by name
  over RPC) runs as Transact-SQL whichever dialect the connection chose, so mssql-jdbc's
  `DatabaseMetaData` (`getTables`, `getColumns`, `getPrimaryKeys`, `getProcedures`,
  `getProcedureColumns`) works while the connection reads Oracle SQL, and `sp_stored_procedures`,
  `sp_sproc_columns` and `sys.objects` list PL/SQL procedures and functions too. Values go out with the protocol's closest types (`NUMBER`
  as `numeric`, `DATE` as `timestamp(0)`, `VARCHAR2` as `text` or `varchar`, over TDS as SQL
  Server's), so the drivers read them unchanged. A pooled TDS connection's reset returns to
  Transact-SQL.

Known differences: a query string that the previous dialect cannot split into tokens (an
Oracle `q'[...]'` literal sent in one psql `-c` with the `SET` before it) fails before anything
runs, so send the `SET` on its own; a statement that names `pg_catalog.` (the queries of psql's
backslash commands and pgjdbc's `DatabaseMetaData`) runs in PostgreSQL's dialect on a PostgreSQL
connection whichever dialect it chose, so it sees the same tables in their stored (lower-case)
names, and a `SET eggdb.dialect` in the same query string as such a statement is undone after
it; over TDS, mssql-jdbc's
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
  most 32 deep, as SQL Server nests them, so a runaway recursion ends with an error long before the stack does.
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

- **`GOTO` and labels**: `GOTO label` jumps to a `<<label>>` in its own statement sequence or an
  enclosing one (out of loops, `IF`, blocks and from a handler to an enclosing block); a jump into
  an `IF`, `CASE`, loop or block, or from a handler back into its block, is `PLS-00375` at
  `CREATE`, an unknown label `PLS-00201`.
- **Pipelined functions and collections in SQL**: `PIPELINED` functions with `PIPE ROW`, read with
  `SELECT ... FROM TABLE(f(...))` (scalar rows in `COLUMN_VALUE`, record rows in their fields);
  `TABLE(f(...))` of a function returning a nested table or varray and `TABLE(v)` of a PL/SQL
  collection in a block's SQL; `x [NOT] MEMBER OF c`, `a [NOT] SUBMULTISET OF b`, `CARDINALITY`
  (`NULL` for an empty collection) and `MULTISET UNION`/`INTERSECT`/`EXCEPT` with `ALL` or
  `DISTINCT`. `MEMBER OF` is `FALSE` for an empty collection, `NULL` for a `NULL` element or
  collection and when no element matches but one is `NULL`. `SUBMULTISET OF` follows Oracle's
  rules: an empty collection is a submultiset of anything (a `NULL` collection too), a `NULL`
  collection on either side is otherwise `NULL`, and the result is `NULL` rather than `FALSE`
  when replacing the `NULL` elements could make it `TRUE`. A `MULTISET` result holds at most
  4,194,304 elements (`54000`, as a PL/SQL collection), and the operators match elements by
  sorting, so large collections take time in proportion to n log n and can be cancelled.

Not supported yet (`0A000`, or `ORA-06550` with the reason at `CREATE`): `PRAGMA
AUTONOMOUS_TRANSACTION` (EggDB runs one write transaction at a time, so a routine cannot commit
apart from its caller), call specifications (`LANGUAGE`, `EXTERNAL`), user-defined aggregates,
`TABLE(...)` that reads columns of an earlier `FROM` item, `TABLE(...)` in a join, `IS A SET`,
`SET(c)`, `MULTISET(subquery)`, `CAST(... AS type)` to a schema collection type, `OUT` binds in `EXECUTE IMMEDIATE ... USING`, `COMMIT` and `ROLLBACK` through `EXECUTE
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
  `Sort`, `Limit`, `Nested Loop`, `Hash Join`). A `SELECT`'s subqueries are drawn under the step
  that reads them, as PostgreSQL 16 draws them: an uncorrelated scalar, `EXISTS` or `ARRAY`
  subquery is an `InitPlan n (returns $k)` run once and named `$k`, an uncorrelated `IN`,
  `= ANY` or `<> ALL` is a `SubPlan n` that the step reads as `(hashed SubPlan n)` (EggDB builds
  its set once and hashes it), and a correlated subquery, or another quantified comparison, is
  a `SubPlan n` evaluated per row. The step that uses them carries a `Filter:` line when its
  `WHERE` names one (other steps still print no conditions), a nested subquery is numbered
  before the one holding it, and `EXPLAIN ANALYZE` gives each subplan's top node
  `(actual time=a..b rows=n loops=n)` per run, averaged over its `loops` (D327).
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
- `INSERT ... ON CONFLICT [(columns) [WHERE predicate] | ON CONSTRAINT name] DO NOTHING | DO
  UPDATE SET column = expr, ...` (`EXCLUDED` names the row that was proposed; `DO UPDATE` needs
  the columns or a constraint name, as in PostgreSQL). `RETURNING` gives the inserted and the
  updated rows, not the skipped ones, and each counts once in the tag. The columns must be those
  of the primary key or a unique index (`42P10` otherwise); a conflict on another unique key
  fails with `23505`, as in PostgreSQL, and `DO UPDATE` reaching one row twice in a statement is
  `21000`. `ON CONSTRAINT name` names the primary key or a unique constraint as the arbiter, on a
  partitioned table too: a name that is no constraint of the table, or a unique index created
  without a constraint, is `42704` (`constraint "x" for table "t" does not exist`), and a
  `CHECK` or foreign key is `42809`. The `WHERE` after the columns is the index predicate
  PostgreSQL uses to infer a partial unique index; EggDB has no partial indexes yet, so, as
  PostgreSQL does when a full unique index matches, the predicate's columns are checked (`42703`)
  and the full unique index on the columns is the arbiter. An expression in the columns
  (`ON CONFLICT (lower(email))`) is `42P10`, as no index is on an expression yet, and `ASC`,
  `DESC` and `NULLS` are `42601` (D324). Not yet: `WHERE` on `DO UPDATE`, and `COLLATE` or an
  operator class on a target column.
- `VALUES (expr, ...), (expr, ...) [ORDER BY ...] [LIMIT ...] [OFFSET ...]` as a statement, an
  operand of `UNION`, `INTERSECT` and `EXCEPT`, a subquery (`x IN (VALUES (1), (2))`), a `WITH`
  query and a `FROM` item (`(VALUES (1, 'a')) AS v(n, s)`): its columns are `column1`,
  `column2`, ... and each takes one type by the `UNION` rules below; rows of different lengths
  are `42601` (D128).
- `SELECT [ALL | DISTINCT | DISTINCT ON (expr, ...)] items [FROM from_item [join ...]] [WHERE expr] [GROUP BY [ALL | DISTINCT] grouping_item, ...] [HAVING expr] [ORDER BY expr [ASC | DESC] [NULLS FIRST | LAST], ...] [LIMIT n | ALL] [OFFSET n [ROW | ROWS]] [FETCH {FIRST | NEXT} [n] {ROW | ROWS} {ONLY | WITH TIES}]`.
  `FETCH FIRST n ROWS ONLY` is `LIMIT n` (no count is 1); `WITH TIES` (D300) also returns the
  rows after the last one that sort equal to it, and needs `ORDER BY` (else `42601`) and a count
  that is not NULL (`2201W`, D321); `LIMIT` and `FETCH` together are `42601`.
  `ORDER BY` takes an output column's number or alias, or any expression over the tables (with
  `DISTINCT`, only expressions in the select list, else `42P10`); a `SELECT` without `FROM`
  returns one row. `DISTINCT ON (expr, ...)` (PostgreSQL, D299) keeps the first row of each set
  of rows equal in those expressions (NULLs equal), in `ORDER BY` order, before `LIMIT` and
  `OFFSET`; its expressions follow `ORDER BY`'s rules (an output column's number or alias, or
  any expression over the tables), and they must be the leftmost `ORDER BY` items in any order,
  else `42P10` "SELECT DISTINCT ON expressions must match initial ORDER BY expressions" (a
  column matches with or without its table's name). A constant that is not an integer is
  `42601` "non-integer constant in DISTINCT ON". In a
  grouped query they name output columns (another expression is `0A000`), and beside a window or
  set-returning function in the select list they answer `0A000`. A view with `DISTINCT ON` is
  not automatically updatable.
- **`SELECT items INTO [TEMPORARY | TEMP | UNLOGGED] [TABLE] new_table [FROM ...]`**
  (PostgreSQL and SQL Server, D303) creates `new_table` with the query's columns, their names
  and types (all nullable, without keys or defaults), and fills it with the query's rows; the tag
  is `SELECT n`. It belongs to the transaction: `ROLLBACK` removes the table. Its other spelling is
  `CREATE TABLE ... AS` (below). An existing name is `42P07`, `INTO` in a later query of a `UNION`, `INTERSECT`
  or `EXCEPT` is `42601` and in a subquery `0A000`; `UNLOGGED` makes an ordinary table. Inside
  PL/pgSQL, `SELECT ... INTO` still assigns variables.
- **`CREATE [TEMP] TABLE [IF NOT EXISTS] name [(column, ...)] AS query [WITH [NO] DATA]`**
  (D323) makes a table from a query in PostgreSQL, MySQL, SQLite and Oracle: the columns take the
  query's names (or the listed ones; more names than columns is `42601` "too many column names
  were specified", and in Oracle any other count is `ORA-01730`) and types, without keys or
  defaults, and the rows are copied in the same transaction; the tag is `SELECT n`, and
  `CREATE TABLE AS` for `WITH NO DATA`, which copies only the shape. `IF NOT EXISTS` on an
  existing table runs nothing and answers a notice. PostgreSQL and SQLite make every column
  nullable; MySQL and Oracle keep `NOT NULL` of a column copied directly from the one table of
  the query. MySQL's `CREATE TABLE t [(definitions)] [IGNORE | REPLACE] [AS] SELECT ...` puts the
  defined columns the query doesn't name first, uses a definition's type for a query column of
  the same name, keeps the keys and indexes defined, and reports the rows as affected. SQLite
  declares each column by its expression's affinity (`INT`, `TEXT`, `REAL`, `NUM`, or no type
  for an expression) and keeps the text `CREATE TABLE t(a INT,c)` in `sqlite_schema`. Oracle
  needs an alias for an expression (`ORA-00998`), and `WHERE 1 = 0` copies the shape.
  PostgreSQL's `CREATE TABLE name [(column, ...)] [USING heap] [WITH (option = value, ...) |
  WITHOUT OIDS] [ON COMMIT ...] [TABLESPACE pg_default] AS ...` (D342), and a plain `CREATE
  TABLE` with the same clauses after its columns (and `PARTITION BY`) (D386), checks the options as
  PostgreSQL does and keeps nothing from them: another access method is `42704` (an index method
  such as `btree` `42809`), another tablespace `42704` (`pg_global` `22023`), an unknown storage
  parameter, a value out of its range or a parameter given twice `22023`, and `oids = true`
  `0A000`; the heap parameters (`fillfactor`, `autovacuum_*`, `toast.*`, `parallel_workers` and
  the rest) are accepted. `CREATE TABLE name AS EXECUTE statement[(argument, ...)] [WITH [NO]
  DATA]` makes the table from a prepared `SELECT` (`42809` for another kind of statement,
  `26000` for an unknown name).
- **`PREPARE name [(type, ...)] AS statement` and `EXECUTE name[(argument, ...)]`** (D342):
  PostgreSQL's SQL-level prepared statements, kept by the session for `SELECT`, `VALUES`,
  `TABLE`, `INSERT`, `UPDATE`, `DELETE`, `MERGE` and `WITH`. The arguments are evaluated and
  bound to `$1`, `$2`, ... as the extended protocol binds values, typed by the declared types
  (an undeclared one is a constant of unknown type, typed by where it stands); a wrong count is
  `42601`, an existing name `42P05`, an unknown one `26000`, and each of them, as any error,
  aborts a transaction block. `DEALLOCATE` and
  `DISCARD ALL` forget them. A session keeps at most 4096 of them and 64 MiB of their text
  (`54000`). Not yet: running the extended protocol's named statements with `EXECUTE` (the two
  are kept apart).
- **`TABLE name`** (D342) is `SELECT * FROM name` in PostgreSQL (`TABLE ONLY name` too) and MySQL
  (`TABLE t [ORDER BY ...] [LIMIT ...]`): a statement, a set operand, a `WITH` query, a derived
  table, a subquery (`IN (TABLE t)`), the source of `INSERT INTO t2 TABLE t1` and the query of
  `CREATE TABLE ... AS TABLE t`. `EXPLAIN TABLE t` plans it as `SELECT * FROM t` (code written, unbuilt: the parser routes it, a correctness case checks it).
- **`CREATE [TEMPORARY] TABLE [IF NOT EXISTS] new LIKE old`** and `(LIKE old)` (MySQL, D342)
  make an empty table with `old`'s columns, types, `NOT NULL`, defaults, `AUTO_INCREMENT`,
  generated columns, primary key, indexes, checks, comments and partitions, without its rows,
  foreign keys or triggers. An existing name is `1050`, a missing `old` `1146`, a view `1347`
  ("is not BASE TABLE"), and a `TEMPORARY` copy of a partitioned table `1562`.
- **`VALUES` as a query** (D323): in MySQL `VALUES ROW(1, 2), ROW(3, 4)` is a statement and a
  derived table with columns `column_0`, `column_1`, ...; in SQLite `VALUES (1, 2), (3, 4)` names
  them `column1`, `column2`, ...; a derived table's alias list (`(VALUES ...) AS t(a, b)`, also in
  SQL Server) renames them.
- **Row constructors** (D323): `(a, b)` (and `ROW(a, b)` in PostgreSQL and MySQL) compare with
  `=`, `<>`, `<`, `<=`, `>`, `>=` against another row constructor or a subquery of as many columns
  in PostgreSQL, MySQL and SQLite, by SQL's rules: `=` holds when every pair is equal, and an
  ordering is decided by the first pair that is not equal, NULL when that pair holds a NULL.
  `(a, b) [NOT] IN (SELECT x, y ...)`, `(a, b) IN ((1, 2), (3, 4))` and `(a, b) = ANY (SELECT
  ...)` work there and in Oracle, with NULL making the answer unknown as the standard says. A
  width that differs is `42601` "unequal number of entries in row expressions" (or "subquery has
  too many/few columns"; MySQL's 1241, SQLite's and Oracle's own), and a row subquery with more
  than one row is `21000` (SQLite uses its first row). Outside PostgreSQL a row constructor is
  only an operand of a comparison or `IN` (MySQL's 1241, SQLite's `row value misused`), and
  Oracle compares rows only with `=` and `<>` (ORA-00920). SQL Server has no row constructors.
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
  `a NATURAL [INNER | LEFT | RIGHT | FULL] JOIN b` (D299; not in Transact-SQL, no `FULL` in
  MySQL), `a CROSS JOIN b` and `FROM a, b`, chained left to right, each table with an optional
  alias. A `NATURAL` join is `USING` every column name both sides show, in the left side's
  order; with none it is a cross join, and a name the left side has twice is `42702`.
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
  PostgreSQL 14's clauses after the parentheses add columns to it (D323): `SEARCH DEPTH FIRST BY
  columns SET ordercol` adds a `record[]` of the listed columns' values along the path from the
  first row (`{"(1,2)","(2,3)"}`), so `ORDER BY ordercol` gives depth-first order; `SEARCH
  BREADTH FIRST BY columns SET ordercol` adds a `record` of the step's depth and the columns
  (`(1,2,3)`), for breadth-first order; `CYCLE columns SET is_cycle [TO value DEFAULT value]
  USING path` adds the mark (`true`/`false` by default, else the two constants' common type) and
  the path as a `record[]`, and a row whose columns are already on its path is returned marked
  and not followed further (fields compare with NULLs equal). With both, the columns are added
  in the order `ordercol`, `is_cycle`, `path`. Misuse is answered as PostgreSQL does: a query that
  is not recursive, a column not in the list or a new column name already in it, and a left or
  right side of the `UNION` that is not a `SELECT` or a recursive reference not at the top of
  the right-hand `SELECT` are `42601`, a column listed twice `42701`, and mark values of no
  common type `42804`; other dialects answer `0A000`. A `SELECT *` over a `USING` or `NATURAL`
  join in the recursive term of such a query is `0A000`.
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
- **Grouping sets** (D298): `GROUP BY ROLLUP (a, b)` groups by `(a, b)`, `(a)` and `()`; `CUBE
  (a, b)` by every subset of its keys; `GROUPING SETS ((a), (b, c), ())` by each set listed, which
  may itself be a `ROLLUP`, a `CUBE` or another `GROUPING SETS`; `()` is the grand total, and a
  parenthesized list such as `ROLLUP ((a, b), c)` counts as one key. Several items multiply
  their sets (`GROUP BY a, ROLLUP (b)` is `(a, b)` and `(a)`), `GROUP BY DISTINCT` drops repeated
  sets and `GROUP BY ALL` is the default. In a row of one set, the keys the set leaves out are
  NULL, and `GROUPING(a, b, ...)` tells such a NULL from a NULL in the data: one bit per argument,
  the last argument the lowest, 1 when the key is left out of the row's set (`42803` when an
  argument is not a grouping key). The empty set returns its row even over no rows. Every set
  is computed in one pass over the rows; a query that only rolls up (each set is the first keys
  of the one before) returns its rows sorted by the keys, each subtotal after its rows and the
  total last, as PostgreSQL's sorted plan and MySQL do; other sets come out one set after the
  other. A primary key determines its table's other columns only when every set groups by it.
  More than 4096 sets is `54001`, a `CUBE` of more than 12 keys `54000` and `GROUPING` with
  more than 31 arguments `54023`, as in PostgreSQL; the sets times the `GROUP BY` keys may be at
  most 1,048,576 (`54001`, EggDB's own bound on the work per row).
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
- **Statistical aggregates** (D321): `stddev_samp`, `stddev_pop`, `var_samp` and `var_pop`, and
  each dialect's own names: PostgreSQL's `stddev` and `variance` (the sample forms), Oracle's
  `STDDEV` and `VARIANCE` (the sample forms, but 0 for one row), MySQL's `STD`, `STDDEV` and
  `VARIANCE` (the population forms, as MySQL defines them) and Transact-SQL's `STDEV`,
  `STDEVP`, `VAR` and `VARP`; SQLite has none (`no such function`). They skip NULLs, are NULL
  over no rows, the sample forms are NULL for one row, and they take `DISTINCT`, `FILTER` and
  `OVER`. In PostgreSQL and Oracle an integer or `numeric` argument gives an exact `numeric`,
  computed from exact sums and divided at PostgreSQL's scale (`variance` of 1, 2, 3 and 4 is
  `1.6666666666666667`, `stddev` its square root at the same scale), and a float gives `double
  precision` from a running mean (Welford's method), so large offsets cost no precision; MySQL
  and Transact-SQL return `DOUBLE` and `float` for every argument. A non-numeric argument is
  `42883`.
- **Hypothetical-set aggregates** (D321), in PostgreSQL and Oracle: `rank(args) WITHIN GROUP
  (ORDER BY exprs)`, `dense_rank`, `percent_rank` and `cume_dist` give the rank the row of
  `args` would have among the group's rows in that order, with as many arguments as `ORDER BY`
  expressions (else `42883`, ORA-00909), each column's `ASC`, `DESC` and `NULLS FIRST` or
  `LAST`; `rank` and `dense_rank` are `bigint`, `percent_rank` (`(rank - 1) / rows`) and
  `cume_dist` (`(rows before or equal to it + 1) / (rows + 1)`) `double precision`. Over no rows
  they are 1, 1, 0 and 1. They take `FILTER`, not `OVER` (`0A000`). A direct argument may
  name only grouped columns (else `42803`, D340).
- **Two-variable statistics** (D340), in PostgreSQL and Oracle: `regr_count`, `regr_sxx`,
  `regr_syy`, `regr_sxy`, `regr_avgx`, `regr_avgy`, `regr_r2`, `regr_slope`, `regr_intercept`,
  `covar_pop`, `covar_samp` and `corr`, each `(Y, X)` with the dependent value first, skipping
  pairs with a NULL on either side. `regr_count` is a `bigint` (0 over no pairs); the others are
  `double precision`, NULL over no pairs, `covar_samp` NULL for one pair, `regr_slope`,
  `regr_intercept`, `regr_r2` and `corr` NULL when X has no spread, `corr` NULL and `regr_r2` 1
  when Y has none. They are computed with running sums of deviations (the Youngs-Cramer method),
  so large offsets cost no precision, and take `FILTER`, `OVER` and, in PostgreSQL, `DISTINCT`
  (ORA-30482 in Oracle). A non-numeric argument is `42883`; MySQL, SQL Server and SQLite have
  none of them and answer their unknown-function errors. Oracle's rank correlations
  `CORR_S(expr1, expr2)` (Spearman's rho, with average ranks for ties) and `CORR_K(expr1,
  expr2)` (Kendall's tau-b, counted in n log n) return the coefficient, NULL for fewer than two
  pairs or no spread; their third argument (`ONE_SIDED_SIG` and the other significances) is not
  taken yet (D374). `STATS_MODE(expr)` returns the most frequent non-null value, the smallest
  of several equally frequent ones (Oracle picks any of them) (D375).
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
- `UPDATE name [[AS] alias] SET column = expr | DEFAULT, ... [FROM from_list] [WHERE expr]
  [RETURNING items]`. Changing a primary key moves the row; the uniqueness check runs after every
  row moved, so `UPDATE t SET id = id + 1` succeeds (PostgreSQL may refuse it, depending on the
  order it visits rows).
- `DELETE FROM name [[AS] alias] [USING from_list] [WHERE expr] [RETURNING items]`.
- **Joined `UPDATE` and `DELETE`** (D297): `FROM` and `USING` take any `FROM` list (joins,
  subqueries, `VALUES`, functions); `WHERE` joins it to the target, and a target row is changed
  or deleted when at least one combination of rows matches. A target row that matches several
  rows is changed once, from one of them, as PostgreSQL's manual says (which one is not defined).
  `SET`, `WHERE` and `RETURNING` read the other tables' columns, `RETURNING *` returns the
  target's columns and then theirs, and naming the target again in the list without an alias is
  `42712`, as in PostgreSQL. Subqueries in `SET` and `RETURNING` of a joined statement, a view,
  partitioned table or table with row-level security as its target, and `EXPLAIN` of it are
  `0A000`.
- `BEGIN [WORK | TRANSACTION] [READ ONLY | READ WRITE] [ISOLATION LEVEL ...]`,
  `START TRANSACTION`, `COMMIT`/`END`, `ROLLBACK`/`ABORT`. `SET SESSION CHARACTERISTICS AS
  TRANSACTION ...` and `SET TRANSACTION ...` take every isolation level and `[NOT] DEFERRABLE`
  (D131): every transaction runs serializable, so the level changes nothing, as `SHOW
  transaction_isolation` says. Read-only transactions work as PostgreSQL's (D318): `BEGIN READ
  ONLY`, `START TRANSACTION READ ONLY`, `SET TRANSACTION READ ONLY | READ WRITE`, `SET
  transaction_read_only = on`, and `default_transaction_read_only` (also set by `SET SESSION
  CHARACTERISTICS AS TRANSACTION READ ONLY`), which makes every new transaction read-only,
  single statements outside a block included. A read-only transaction refuses `INSERT`,
  `UPDATE`, `DELETE`, `MERGE` and `COPY FROM` unless they write a temporary table, every
  `CREATE`, `ALTER` and `DROP` (temporary ones too), `COMMENT`, `GRANT`, `REVOKE`, `TRUNCATE`
  and `nextval()`, with `25006 cannot execute INSERT in a read-only transaction`, and
  `SELECT ... FOR UPDATE | NO KEY UPDATE | SHARE | KEY SHARE` of a permanent table, with `25006
  cannot execute SELECT FOR UPDATE in a read-only transaction` (Oracle's read-only transaction
  answers `ORA-01456`); `LOCK TABLE` is accepted, as in PostgreSQL. A writing function called
  from `SELECT` is `25006` too, and a procedure that commits runs every transaction it starts
  read-only under `default_transaction_read_only` (D338). Under `default_transaction_read_only =
  on` a top-level `CALL` or `DO` runs non-atomic and holds the single writer for the whole
  procedure (D338).
  `SHOW transaction_read_only` and `current_setting('transaction_read_only')` say whether the
  current transaction is read-only. A transaction that wrote may turn read-only and still commit
  what it wrote, but `SET TRANSACTION READ WRITE` after the transaction's first query is `25001
  transaction read-write mode must be set before any query`, and a different isolation level
  after it is `25001 SET TRANSACTION ISOLATION LEVEL must be called before any query` (the
  level the block began with, or `serializable`, is the current one); inside a savepoint both
  are `25001`. Outside a block `SET TRANSACTION` warns `SET TRANSACTION can only be used in
  transaction blocks` and changes nothing; in an implicit transaction (a multi-statement
  `Query`) it sets that transaction (D338). `pg_export_snapshot()` in a `READ ONLY` block names
  its snapshot (`0A000` elsewhere), and another session's `SET TRANSACTION SNAPSHOT 'id'`, as
  the first statement of its block (`25001` after a query, `25P01` outside a block), then sees
  exactly that snapshot while the exporting block is open (`22023 invalid snapshot identifier`
  after it ends); a read-write block takes it only when nothing has committed since (D369).
- `SET [SESSION | LOCAL] name {TO | =} value | DEFAULT`, `SET [LOCAL] TIME ZONE`, `SET NAMES`,
  `RESET name | ALL`, `SHOW name | ALL`: PostgreSQL's run-time parameters (D104). `client_encoding`
  takes `UTF8` (and `SQL_ASCII`, which passes bytes through) only; settings EggDB does not apply
  yet are remembered and shown. `lock_timeout` limits how long a statement waits for the
  database's single writer (`55P03 canceling statement due to lock timeout`, D393). `max_identifier_length`,
  `max_index_keys` and `max_function_args` are fixed, as PostgreSQL's server limits. Settings
  follow transactions as in PostgreSQL (D301): a `SET` inside a transaction is undone by its
  `ROLLBACK` (and by `ROLLBACK TO SAVEPOINT` to a savepoint set before it) and kept by its
  `COMMIT`, unless the `COMMIT` itself fails (a deferred constraint), which undoes them with the
  data; `SET LOCAL` and `set_config(name, value, true)` last to the end of the transaction
  whether it commits or not (a later `SET` of the same parameter in the transaction is what
  stays after its `COMMIT`). `SET [LOCAL] name TO DEFAULT` is `RESET name`: the value the client
  sent when it connected, else the built-in default. Outside a transaction block `SET LOCAL` warns `SET LOCAL can only be
  used in transaction blocks` and changes nothing, and `set_config(..., true)` lasts for its own
  statement; in an implicit transaction (a multi-statement `Query` message, a pipeline up to its
  `Sync`) both last until it ends.
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
  PostgreSQL (D146). The column must be a column of the primary key, at any position, in a
  partitioned table too (D337, D364); a named sequence and
  `DEFAULT nextval('s')` have no such limit. Identity options in `CREATE TABLE` (`GENERATED ...
  AS IDENTITY (START WITH 100 INCREMENT BY 5)`) are kept; `MINVALUE`, `MAXVALUE` and `CYCLE`
  that end the identity before its type's range answer `0A000`. `ALTER COLUMN c SET GENERATED
  {ALWAYS | BY DEFAULT}`, `SET INCREMENT BY`, `SET START WITH`, `RESTART [WITH n]` (on an identity
  backed by a sequence) and `DROP IDENTITY [IF EXISTS]` change it later (`55000` on a column that
  is not an identity). Generated columns (`GENERATED ALWAYS AS (expr) [STORED | VIRTUAL]`) are
  described in [Generated columns](#generated-columns). `ALTER TABLE t
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
  takes the database's write transaction, so a read-only file cannot have them, except inside a
  read-only transaction, which writes them without the writer (D318). A temporary table
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
places), `ceil`/`ceiling`, `floor` and `abs` take `numeric` (and floats). `sqrt` of a
`numeric` is a `numeric` with at least 16 significant digits (`sqrt(2::numeric)` is
`1.414213562373095`), of an integer or float a `double precision`, and of a negative number
`2201F` (`cannot take square root of a negative number`) (D350). `NaN` is accepted,
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

Time stamp input and `AT TIME ZONE` also read the time zone abbreviations of PostgreSQL's
`Default` set in any letter case (`2024-01-01 10:00 PST`, `CEST`, `JST`, `UTC`; `MSK` and the
other abbreviations whose meaning changed over time follow their region), and
`pg_timezone_abbrevs` lists them with their offset and daylight saving flag; the `TimeZone`
setting refuses abbreviations with `22023`, as PostgreSQL does. `TimeZone` and `AT TIME ZONE`
also take POSIX rule strings: `EST5EDT,M3.2.0,M11.1.0` (offsets west of UTC, `Mm.w.d`, `Jn` and
`n` day rules with an optional `/time`), `<+03>-3` for a quoted abbreviation, and a daylight
time without rules (`PST8PDT`) follows the United States' rules. `date`, `timestamp` and
`timestamptz` take `infinity` and `-infinity` (stored as PostgreSQL stores them, the type's
largest and smallest values, so they sort after and before every finite value and equal each
other across the three types): `isfinite` is false for them, adding or subtracting a finite
interval or a number of days keeps them, a finite time stamp plus an infinite interval is
infinite, `infinity - infinity` and `infinity + '-infinity'` fail with `22008`, the difference
with a finite time stamp is an infinite interval, subtracting infinite dates fails with `22008`,
and `to_char` returns NULL for them (D271). `date_part` of an epoch, `julian`, `year`,
`isoyear`, `decade`, `century` or `millennium` field is `Infinity` or `-Infinity` and of any other
field NULL, as in PostgreSQL 16; `extract` gives the same NULLs and, for the infinite fields,
the `numeric` `Infinity` or `-Infinity` (D290). SQL Server's and Oracle's own date functions,
conversions and arithmetic refuse an infinite value as out of range (SQL Server error 242,
`ORA-01841`), since those databases have none. `age` with one infinite argument is an infinite
interval (`age('infinity'::timestamp, t)` is `infinity`, `age(t, 'infinity')` `-infinity`) and
with two equal infinities fails with `22008`, as in PostgreSQL 17. `date_trunc`, `date_bin` and
`time_bucket` return an infinite input unchanged, and `date_bin` with an infinite origin fails
with `22008`.

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

SQL/JSON path (D272) works as in PostgreSQL: `jsonb_path_exists`, `jsonb_path_match`,
`jsonb_path_query` (set-returning, in `FROM` or a select list), `jsonb_path_query_array` and
`jsonb_path_query_first`, each with optional `vars` (an object, else `22023`) and `silent`
arguments and a `_tz` form, and the operators `jsonb @? path` and `jsonb @@ path`. A path is
`lax` (the default, arrays unwrapped and structural errors ignored) or `strict`, and has member
(`.a`, `."a b"`, `.*`), element (`[0]`, `[1 to 3]`, `[last]`, `[*]`) and recursive (`.**`,
`.**{1 to 2}`) accessors, filters `? (...)` with comparisons, `&&`, `||`, `!`, `is unknown`,
`exists`, `like_regex` (with `flag`) and `starts with`, arithmetic (`+ - * / %`, unary signs),
the methods `type()`, `size()`, `double()`, `number()`, `integer()`, `string()`, `abs()`,
`floor()`, `ceiling()`, `bigint()`, `boolean()`, `keyvalue()` and PostgreSQL 17's
`decimal([precision [, scale]])` (a number or numeric string rounded to that precision and scale,
`2203G` when it does not fit, `22023` for a precision outside 1 to 1000; D302), the datetime methods
`datetime()`, `datetime(template)`, `date()`, `time([p])`, `time_tz([p])`, `timestamp([p])` and
`timestamp_tz([p])` (ISO strings or a `to_timestamp` template; comparisons that need the session's
time zone only in the `_tz` functions, else `22031`), and `$name` variables (D289). Errors carry PostgreSQL's
SQL/JSON states (`2203A` member not found, `22039` array not found, `22033` subscript, `22036`
non-numeric item, `22038` singleton required). The `jsonpath` type name is accepted as text, and
the path is checked when it is used. A path of more than 1,000 nodes fails with
`54001`, one producing too many items with `54000`, and `like_regex` runs within a step budget.
`json_to_record`, `jsonb_to_record`, `json_to_recordset` and `jsonb_to_recordset` in `FROM`
take a column definition list (`AS x(a int, b text)` or `AS (a int)`): each column is the
member of that name converted to its type (`NULL` when missing or JSON `null`; an object or
array kept as JSON text in a text column), and without the list they fail with `42601`. A JSON
array fills an array column (`c int[]` from `[1,2,3]`) element by element, and a JSON object a
composite column (`r myrowtype`) member by member, as in PostgreSQL's documented example; a
nested array fails with `0A000` (arrays have one dimension), an array mixing nested arrays and
scalars with `22P02`. `json_populate_record`, `jsonb_populate_record`, `json_populate_recordset`
and `jsonb_populate_recordset` in `FROM` take their columns from the first argument's row type,
`NULL::t` or `CAST(NULL AS t)` for a table, view or composite type `t`. A base row of a
composite type that is not NULL (`ROW(1, 2)::pt`) gives each column the JSON member's value when
the object has that key and the base row's value otherwise, and is returned whole for a NULL
document (D302).

The SQL/JSON query functions (D289) work as in PostgreSQL 17 and Oracle: `JSON_EXISTS(doc, path
[PASSING v AS name, ...] [{TRUE | FALSE | UNKNOWN | ERROR} ON ERROR])`, `JSON_VALUE(... [RETURNING
type] [{NULL | ERROR | DEFAULT expr} ON EMPTY] [... ON ERROR])` for one scalar (several items
`22034`, an object or array `2203F`, nothing with `ERROR ON EMPTY` `22035`), and `JSON_QUERY(...
[RETURNING type] [WITH [CONDITIONAL | UNCONDITIONAL] [ARRAY] WRAPPER | WITHOUT WRAPPER] [{KEEP |
OMIT} QUOTES] [... ON EMPTY] [... ON ERROR])` with `EMPTY ARRAY` and `EMPTY OBJECT` behaviors.
`JSON_TABLE(doc, 'path' [AS name] [PASSING ...] COLUMNS (...) [{ERROR | EMPTY [ARRAY]} ON ERROR])`
in `FROM` takes `name FOR ORDINALITY`, `name type [FORMAT JSON] [PATH 'path']` with the wrapper,
quotes and behavior clauses, `name type EXISTS [PATH 'path']` and `NESTED [PATH] 'path' COLUMNS
(...)` (children joined to their parent, siblings in a union), may follow another `FROM` item it
reads, and produces at most 1,000,000 rows (`54000`) with at most 32 nested levels. Oracle's
dialect accepts the same forms, columns without a type and `NULL ON ERROR`.

PostgreSQL 17's SQL/JSON constructors and predicate (D302): `expr IS [NOT] JSON [VALUE | SCALAR |
ARRAY | OBJECT] [{WITH | WITHOUT} UNIQUE [KEYS]]` tests a `text`, `bytea`, `json` or `jsonb`
value (false for text that is not JSON, NULL for NULL, `42804` for other types; a `jsonb` value
never has duplicate keys), and works in `CHECK` constraints. `JSON(expr [FORMAT JSON] [{WITH |
WITHOUT} UNIQUE [KEYS]])` reads text or `bytea` as `json`, keeping the text as written (`22P02`
for text that is not JSON, `22030` for a duplicate key under `WITH UNIQUE KEYS`, `42804` for a
non-string argument). `JSON_SCALAR(expr)` gives the `json` value `to_json` gives (a number or
boolean as itself, anything else as a string, a date or time stamp in ISO form).
`JSON_SERIALIZE(expr [FORMAT JSON] [RETURNING type [FORMAT JSON]])` gives a JSON value's text as
`text` (the default), another character type or `bytea` (other types are `42804`). Their columns
are named `?column?`, `json`, `json_scalar` and `json_serialize`, as in PostgreSQL.

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
its text. A literal may start with its bounds, `'[0:2]={1,2,3}'`, giving an array whose first
subscript is 0; such an array prints with its bounds, keeps them when stored and in PostgreSQL's
binary format both ways (parameters included), and `array_lower` reports them; bounds that do not
match the elements are `22P02` (D288). `citext[]` and `hstore[]` columns work like the others
(D288). Multidimensional arrays, slices (`a[1:2]`) and arrays of other element types answer
`0A000`. Parameters declared with an array type OID (`int4[]`, `text[]` and every array a column
can hold) bind in the text and binary formats (D265). An array may
hold at most 2^27 elements (`54000`) and counts against the session's value limit.

**Expressions**
- Arithmetic `+ - * / % ^`, unary `-` and `+`, string concatenation `||`; in the PostgreSQL
  dialect the integer operators `&`, `|`, `#` (exclusive or) and prefix `~` (not) on `smallint`,
  `integer` and `bigint`, with PostgreSQL's precedence (D131, D288).
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
  and `\` escapes, `[NOT] LIKE | ILIKE pattern ESCAPE 'c'` in every dialect (D299; `ESCAPE ''`
  means no escape character, a longer one and a pattern ending in its escape character are
  `22025`, `ESCAPE NULL` gives NULL; SQLite's dialect refuses a constant escape that is not one
  character, `ESCAPE ''` included, with "ESCAPE expression must be a single character"; PostgreSQL's `like_escape(pattern, escape)` is the same
  rewrite), `IS [NOT] NULL`, `ISNULL`, `NOTNULL`, `IS [NOT] TRUE | FALSE | UNKNOWN`,
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
`BEGIN READ ONLY` reads one snapshot and refuses changes with `25006` without taking the
writer, so it never waits for another writer; it may still write the session's temporary
tables (D318). A failed statement inside
a block rolls the block back at once and the session answers `25P02` until `ROLLBACK`; `COMMIT`
of a failed block reports `ROLLBACK`, as PostgreSQL does. In MySQL's, SQLite's, Oracle's and SQL
Server's dialects a failed statement rolls back only its own changes and the block goes on, as
each of them does (D317). DDL is transactional: a rolled-back
block that created a table leaves no table.

## Transactions, savepoints and locks
- **Savepoints** work in every dialect: `SAVEPOINT name`, `ROLLBACK TO [SAVEPOINT] name` and
  `RELEASE [SAVEPOINT] name` (PostgreSQL, MySQL, SQLite, Oracle), and `SAVE TRANSACTION name` with
  `ROLLBACK TRANSACTION name` (SQL Server). Rolling back to a savepoint undoes the changes made after
  it and keeps it. In PostgreSQL's dialect a failed statement after a savepoint leaves the block
  aborted until `ROLLBACK TO` brings it back; without savepoints the block is rolled back as
  before. In the other dialects a failed statement undoes only itself, savepoint or not, and
  a later `ROLLBACK TO` still undoes everything after its savepoint (D317). In SQLite's, a `SAVEPOINT` outside a transaction begins one, and releasing it commits.
  A name that was never set fails with each dialect's error (`3B001`, MySQL 1305, ORA-01086,
  SQL Server 6401). In `eggdb shell` the key-value commands of a block (`put`, `del`, `incr`,
  `expire`, `cas` and the rest) are undone by `ROLLBACK TO SAVEPOINT` with the SQL changes
  made after the savepoint, and embedded code gets the same through
  `SharedWriteStore::with_open_transaction_undoable` (D319). To undo a statement, EggDB keeps
  the previous value of every key it writes, with 64 bytes per entry, against the statement's
  1 GiB work memory: inside a savepoint in every dialect, and inside any transaction in the
  dialects other than PostgreSQL's. A statement that passes it fails with `53200` (out of
  memory) and is undone like any failed statement (D317). The savepoint's log holds its entries
  in memory until `RELEASE`, `ROLLBACK TO` or `COMMIT`.
- **Row locks.** `SELECT ... FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE`, `FOR KEY SHARE` with
  `OF`, `NOWAIT`, `SKIP LOCKED` (and Oracle's `WAIT n`, MySQL's `LOCK IN SHARE MODE`) and
  `LOCK TABLE ... IN ... MODE [NOWAIT]` are accepted (in a read-only transaction the `SELECT`
  forms are `25006` in PostgreSQL's and Oracle's dialects, D338). A transaction block holds the database's
  one writer from `BEGIN` on, so no other session changes the rows it read, and the lock
  clauses need nothing more; they will take part when SQL sessions run on concurrent
  transactions (D172).
- **Isolation levels.** `SET TRANSACTION`, `BEGIN ISOLATION LEVEL` and each dialect's forms are
  accepted. Every block runs serializably today, which meets every level's guarantees.

## Full-text search

PostgreSQL's text search works on any text column (D195):

```sql
SELECT id FROM docs
WHERE to_tsvector('english', body) @@ websearch_to_tsquery('english', '"fat rat" or cat -dog')
ORDER BY ts_rank(to_tsvector(body), plainto_tsquery('rat')) DESC;
```

`to_tsvector` and the `to_tsquery` family (`plainto_`, `phraseto_`, `websearch_`) take the
`english` (stop words dropped, English stems) or `simple` configuration, and print as PostgreSQL
prints them; `@@` matches with `&`, `|`, `!`, phrases (`<->`, `<N>`) and prefixes (`:*`).
`ts_rank` ranks by how often the query's lexemes occur; its values are EggDB's own. There is no
full-text index yet, so `@@` reads every row.

## EXPLAIN ANALYZE and the slow-statement log

`EXPLAIN ANALYZE` (or `EXPLAIN (ANALYZE[, COSTS OFF, ...])`) runs the statement and prints its
plan with the rows it returned or changed, and the planning and execution times, as
PostgreSQL does. Each step the executor measures carries PostgreSQL's
`(actual time=a..b rows=n loops=1)`: the top node, a scan, the top of a join, a `Sort` and each
branch of an `Append`, with times in milliseconds from the start of execution to the step's
first and last row, and `(never executed)` on a step it never read (a scan under `LIMIT 0`).
The steps inside a join and an `UPDATE` or `DELETE`'s scan carry none yet, and the timing costs
nothing when the statement is not analyzed (D312); an `UPDATE` or `DELETE` under it changes the
data. A subquery's `InitPlan` or `SubPlan` carries its runs on its top node: `loops` is how
many times it ran (once for an `InitPlan`, once per row read for a correlated one, a run answered
from the per-statement memo included), `rows` and the time are per run, and one that never ran
says `(never executed)` (D327). In MySQL's dialect, `EXPLAIN FORMAT=TREE SELECT ...` and
`EXPLAIN ANALYZE [FORMAT=TREE] SELECT ...` answer MySQL 8's tree in one row of one `EXPLAIN`
column: `-> ` iterators indented four spaces per level (`Table scan on t`, `Single-row index
lookup on t using PRIMARY (id=2)`, `Index lookup`, `Index range scan`, `Index scan`, `Filter:
(t.a > 1)`, `Aggregate: count(0)`, `Group aggregate: sum(t.b)`, `Group (no aggregates)`,
`Stream results`, `Sort: t.b DESC`, `Limit: 2 row(s)`, `Nested loop inner join`, `Inner hash
join (t1.a = t2.c2)` over `Hash`, `Rows fetched before execution`, and `Select #2 (subquery in
condition; run only once)` or `dependent` for a subquery), each with `  (cost=c rows=r)` and,
under `ANALYZE`, ` (actual time=a..b rows=n loops=n)` where EggDB measures. The estimates come
from EggDB's own cost model over the row counts `ANALYZE TABLE` recorded (a table never analyzed
is counted up to 10,000 rows), stated in D345, and differ from the numbers MySQL prints.
`EXPLAIN FORMAT=JSON` answers MySQL 8's `query_block` document (`cost_info`,
`ordering_operation`, `grouping_operation`, `table` or `nested_loop`, each table's
`access_type`, `key`, `rows_examined_per_scan`, `rows_produced_per_join`, `filtered`,
`cost_info` and `attached_condition`). A multi-table `UPDATE` or `DELETE` is explained as the
join it reads under `Update t1 (buffered)` or `Delete from t1 (buffered)`, and `EXPLAIN ANALYZE`
runs it; a single-table one under `EXPLAIN ANALYZE` answers 1235 as MySQL does, and its tree is
`<not executable by iterator executor>`. `TABLE t` is explained as `SELECT * FROM t` (D345).
`EXPLAIN (ANALYZE,
BUFFERS)` adds the page cache's lookups in PostgreSQL's form: `Buffers: shared hit=N read=M`
under the top node for the whole statement (pages found in memory, pages read from the file;
a count of zero is left out) and the same under `Planning:` for planning, counted on the
session's own thread so other sessions' reads never mix in (D286). Setting
`log_min_duration_statement` (milliseconds, or with `ms`, `s` or `min`; `-1` turns it off) makes
`eggdb serve` log every simple query that takes at least that long, with the client's address,
the duration and the statement, in PostgreSQL's `duration: ... ms  statement: ...` form.
`statement_timeout` (same units; `0`, the default, turns it off) stops any statement
that runs longer with `57014 canceling statement due to statement timeout`, checked between
rows and while `pg_sleep` waits.

`eggdb serve` keeps statement statistics (D286) for every protocol: each statement that
completes is counted under its normalized text, the statement with each literal replaced by
`$1`, `$2`, ... (numbered after the statement's own parameters), its spaces and comments folded
and a trailing `;` left out, so values and passwords are never kept. `pg_stat_statements` lists
them for the current database with PostgreSQL's column names: `userid`, `dbid`, `toplevel`,
`queryid` (a stable hash of the normalized text), `query`, `calls`, `total_exec_time`,
`min_exec_time`, `max_exec_time`, `mean_exec_time`, `stddev_exec_time` (milliseconds) and
`rows`. `pg_stat_statements_info` has `dealloc` and `stats_reset`, and
`pg_stat_statements_reset()` empties them (with a non-zero third argument, only that query
ID). Sessions in the MySQL, SQL Server and Oracle dialects read them as
`pg_catalog.pg_stat_statements`. `--statement-statistics` sets how many statement shapes are
kept (5000 by default); a full table drops its least-called twentieth. `--log-slow-statements
<milliseconds>` logs each statement that takes at least that long to stderr, on every
protocol, as one line: `slow statement duration_ms=... database=... user=... protocol=...
rows=... query="..."` after the UTC time, with the same normalized text. `lock_timeout` is
applied to the wait for the writer (D393); limits per role are not there yet.

## Plans in Oracle's and SQL Server's dialects

In Oracle's dialect, `EXPLAIN PLAN [SET STATEMENT_ID = 'id'] [INTO table] FOR <SELECT, UPDATE
or DELETE>` plans the statement without running it and writes one row per operation to
`PLAN_TABLE` (created on first use with the columns of Oracle's reference) or to the table
`INTO` names, which must exist. The rows are written all or none: a failure part-way (a too
narrow `INTO` column, a constraint) leaves no row, and each plan gets a `PLAN_ID` no other
session of the same server takes. `SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY)` (with optional
`table`, `statement_id` and `format` arguments) returns the last plan as `PLAN_TABLE_OUTPUT`
lines:

```
Plan hash value: <the same number for the same plan>

----------------------------------
| Id  | Operation         | Name |
----------------------------------
|   0 | SELECT STATEMENT  |      |
|   1 |  TABLE ACCESS FULL| EMP  |
----------------------------------
```

In SQL Server's dialect, `SET SHOWPLAN_TEXT ON` and `SET SHOWPLAN_ALL ON` make every later
statement return its plan instead of running (the statement's text, then the `|--` tree; or
SQL Server's 18 `SHOWPLAN_ALL` columns). Only `SET` options run, so `SET SHOWPLAN_TEXT OFF` can
end it; `BEGIN`, `COMMIT`, `ROLLBACK` and `SAVE TRANSACTION`, `USE`, `CREATE` and `DROP
DATABASE`, `INSERT BULK`, `THROW`, `RAISERROR`, `PRINT`, `WAITFOR` and variable assignments are
shown and not run. `DECLARE` declares its variables without their initial values, so later
statements can name them. `IF` and `WHILE` conditions are not evaluated: each statement of the
batch is shown once in order, both branches of an `IF` and a loop's body once. `EXEC` of a
procedure and `EXEC(...)` show the plans of the statements inside. `SET SHOWPLAN_TEXT` and
`SET SHOWPLAN_ALL` inside `EXEC(...)`, `sp_executesql` or a procedure fail with SQL Server's
error 1067 and end the batch, as SQL Server refuses them there, and `SET eggdb.dialect` is
refused in nested code (`0A000`), so nested code cannot turn plans off and run what follows. `SET STATISTICS PROFILE ON` runs each statement
and adds that result with `Rows` and `Executes`. EggDB has no estimates, so costs and row
estimates are NULL, and it measures whole statements, so only the top operator's rows are
filled in (D312).

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
are listed in `pg_roles`. `GRANT` and `REVOKE` on schemas, sequences, functions and the other
objects EggDB does not check yet are accepted with a `WARNING` and change nothing; on `ALL TABLES
IN SCHEMA` they are refused (`0A000`) rather than ignored, so a `REVOKE` never seems to work when
it did not. An upsert (`ON CONFLICT DO UPDATE`) also needs `UPDATE`, and `REPLACE` also needs
`DELETE`. Only a superuser creates, changes or drops objects, and turns triggers on or off. Not
yet: logins of stored roles over MySQL's and SQL Server's protocols, schema privileges, object
owners and `SET ROLE`.

PostgreSQL's privilege inquiry functions answer from the same grants the statements are checked
against (D326), for the current user or any role named (a name, an OID or `public`):

```sql
SELECT has_table_privilege('myschema.mytable', 'select');
SELECT has_table_privilege('joe', 'mytable', 'INSERT, SELECT WITH GRANT OPTION');
SELECT has_column_privilege('clerk', 'staff', 'name', 'UPDATE');
```

`has_table_privilege`, `has_column_privilege`, `has_any_column_privilege`,
`has_sequence_privilege`, `has_schema_privilege`, `has_database_privilege`,
`has_function_privilege` and `pg_has_role` take the object by name or OID and a comma-separated
list of privileges in any case, each optionally `WITH GRANT OPTION` (and `WITH ADMIN OPTION` for
roles), and are true when any of them is held. A superuser holds everything; a checked role never
holds a grant option, since only superusers grant. Schemas give every role `USAGE` and only
superusers `CREATE`, databases `CONNECT` to the current one and `CREATE` and `TEMPORARY` to
superusers, sequences `USAGE` and `UPDATE` to every role (`nextval` and `setval` are not checked
yet) and `SELECT` by the table grants, and functions `EXECUTE` to every role, which is what the
engine enforces. An unknown role is `42704`, table `42P01`, column `42703`, schema `3F000`,
database `3D000`, function `42883`, a relation that is not a sequence `42809` and an unknown
privilege `22023 unrecognized privilege type`, as on PostgreSQL; an object named by an OID that
does not exist gives NULL. `information_schema.table_privileges`, `role_table_grants`,
`column_privileges`, `role_column_grants` and `usage_privileges` list grants whose grantee is
the current user, a role it belongs to or (except in the `role_*` views) `PUBLIC`, as the SQL
standard defines them; a superuser sees every grant.

The rest of PostgreSQL's privilege catalog answers from the same model (D344):
`has_type_privilege` (every role uses every type), `has_tablespace_privilege` (`pg_default` and
`pg_global`, `CREATE` for superusers), `has_parameter_privilege` (`SET` and `ALTER SYSTEM` for
superusers, as no parameter grants are stored), and `has_server_privilege` and
`has_foreign_data_wrapper_privilege` (there are no foreign servers, so a name is `42704`).
`aclexplode`, `acldefault` and `makeaclitem` read and build ACLs as pgAdmin and DBeaver use them,
and a cast to `aclitem` reads PostgreSQL's item text with its errors:

```sql
SELECT * FROM aclexplode((SELECT relacl FROM pg_class WHERE relname = 'accounts'));
SELECT acldefault('f', 10);                         -- {=X/eggdb,eggdb=X/eggdb}
SELECT makeaclitem(0, 10, 'SELECT, INSERT', false); -- =ar/eggdb
```

`pg_namespace.nspacl` shows what every schema allows (its owner `USAGE` and `CREATE`, `PUBLIC`
`USAGE`), and `information_schema.enabled_roles`, `applicable_roles`, `routine_privileges` and
`udt_privileges` list the current user's roles, their memberships and the default `EXECUTE` and
`TYPE USAGE` held by `PUBLIC` (and by the owner, for a superuser session).

Privileges can also be granted on single columns, as PostgreSQL (and MySQL) write them (D285):

```sql
GRANT SELECT (id, name), UPDATE (name) ON staff TO clerk;
REVOKE SELECT (name) ON staff FROM clerk;
```

A role without `SELECT` on the whole table then reads only its granted columns: naming another
one anywhere in a statement (the select list, `*`, `WHERE`, `ORDER BY`, a subquery, `RETURNING`,
`COPY ... TO`) fails with `42501 permission denied for table staff`, and `SELECT count(*)`
works. Writes need `INSERT` or `UPDATE` on the columns they write and `SELECT` on the columns
they read, so `UPDATE staff SET name = 'x' WHERE salary > 0` needs `SELECT (salary)`, as on
PostgreSQL. Column grants show in `pg_attribute.attacl` (psql's `\dp`) and
`information_schema.column_privileges`, follow column and table renames and go with a dropped
column.

Tables can limit which rows a role sees and changes with PostgreSQL's row-level security
(D186):

```sql
ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
CREATE POLICY own_notes ON notes FOR ALL TO PUBLIC
  USING (owner = current_user) WITH CHECK (true);
```

A checked role then reads, updates and deletes only the rows its policies' `USING` let through,
and none when no policy names it; superusers are not filtered. Every row an `INSERT` or `UPDATE`
writes must pass the role's `WITH CHECK` conditions (a policy's `USING` when it has none), after
`BEFORE` triggers ran, or the statement fails with `42501 new row violates row-level security
policy for table "notes"` and writes nothing (D285); with `RETURNING`, the new rows must also be
ones the role may see. Under row
security, `COPY ... FROM` is refused as in PostgreSQL ("use INSERT statements instead"), and so
are `INSERT ... ON CONFLICT DO UPDATE`, `REPLACE` and `MERGE`, which would reach rows the
policies hide. `COPY ... FROM` into a table with enabled `INSERT` triggers is refused too, since
COPY does not fire them yet.

`ALTER TABLE [ONLY] t FORCE ROW LEVEL SECURITY` and `NO FORCE ROW LEVEL SECURITY` (D326) make the
table's owner subject to its policies too, as on PostgreSQL. Every table's owner is a superuser
until object owners land, and superusers bypass row security even when it is forced, as they do
on PostgreSQL, so the switch is kept for `pg_dump` and tools and takes effect with owners.
`pg_class.relrowsecurity`, `relforcerowsecurity` and `pg_tables.rowsecurity` show the switches,
and `pg_policy` and `pg_policies` list the policies with their roles, command and conditions in
`pg_get_expr`'s form, so psql's `\d` and `\dp` show them.

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
  partition may itself be partitioned. A key part is a column or an expression:
  `PARTITION BY RANGE ((id / 100))`, `LIST (lower(name))`, `HASH ((a + b))`, and MySQL's
  `RANGE (EXTRACT(YEAR FROM d))`. Subqueries, aggregates, functions whose value changes between calls
  (`random()`, `now()`, `nextval`, user-defined routines) and expressions without a column are
  refused with PostgreSQL's errors, and so are primary keys and unique constraints when the key
  has an expression (D316).
- **Rows go where they belong.** `INSERT`, `COPY` and `INSERT ... SELECT` into the partitioned
  table route each row to its partition. A row no partition takes fails with `23514`, `no
  partition of relation ... found for row`, and a row written straight into a partition must
  fit its bound (`violates partition constraint`). `UPDATE`, `DELETE` and `TRUNCATE` on the
  partitioned table reach every partition. An `UPDATE` through the partitioned table that
  changes a row's key moves the row to the partition that takes it (its count and `RETURNING`
  include it), or fails with `23514 no partition ... found for row` when none does; an `UPDATE`
  that names a partition moves rows only within it (D316). As in PostgreSQL, a moved row fires
  the `BEFORE UPDATE` row triggers, then the source partition's `DELETE` row triggers (a `BEFORE
  DELETE` trigger returning NULL keeps the row where it is) and the target partition's `INSERT`
  row triggers, and no `INSERT` statement triggers (D368).
- **Pruning.** A query reads only the partitions its `WHERE` can match: comparisons, `BETWEEN`,
  `IN` and `IS NULL` on the key (a key expression written as in the key, such as
  `lower(name) = 'ann'`) against constants. `EXPLAIN` lists the partitions read; when one
  is left, the query reads it directly and uses its indexes.
- **Constraints and indexes.** A primary key or unique constraint of a partitioned table must
  include every key column, and each partition gets its own. `PARTITION OF` copies the parent's
  columns, defaults, primary key and checks. `CREATE INDEX` on a partitioned table gives every
  partition, at every level, a matching index (one it already has with the same definition, or
  a new `<partition>_<columns>_idx`), and so do partitions created or attached later. `CREATE
  INDEX ... ON ONLY t` makes the parent's index alone, which `pg_index.indisvalid` shows as false
  until `ALTER INDEX t_idx ATTACH PARTITION p_idx` has attached an index of every partition.
  `DROP INDEX` of the parent's index drops the partitions' indexes; an attached partition index
  can't be dropped alone (`2BP01`). `serial` columns share their sequence.
- **Identity and `AUTO_INCREMENT`.** The partitioned table numbers its rows: an `INSERT` gives
  each row its id in statement order before routing it (so `HASH (id)` routes on the new id),
  from one counter past the furthest id any partition holds, and `LAST_INSERT_ID()` is the
  first id it gave. The column may sit anywhere in the primary key (`PRIMARY KEY (created_at,
  id)`, D364; in MySQL it must also lead the key or an index, as InnoDB requires). A row written
  straight into a partition takes the next id of its partitioned table (D363).
- **Catalogs and restores.** `pg_class` shows `relkind` `p` and `relispartition`, `pg_inherits`
  links partitions to their parent, `pg_partitioned_table`, `pg_get_partkeydef` and
  `pg_get_expr(relpartbound, oid)` describe them, and `pg_dump` files restore with their
  partitions (`ATTACH PARTITION`).
- **MySQL and Oracle.** `PARTITION BY RANGE|LIST|HASH|KEY (...)` with named partitions inside
  `CREATE TABLE` (`VALUES LESS THAN`, `VALUES IN`, `PARTITIONS n`, Oracle's `VALUES (DEFAULT)`)
  makes one partition per name, stored as a table named `<table>#P#<partition>`. MySQL's
  `t PARTITION (p1, p2)` after a table name in `SELECT`, `INSERT`, `REPLACE`, `UPDATE` and
  `DELETE`, and Oracle's `PARTITION (p)` and `PARTITION FOR (key)`, read and write only the
  named partitions: a row outside them is MySQL `1748` / `ORA-14401`, an unknown partition
  `1735` / `ORA-02149`, and a table that is not partitioned `1747` / `ORA-14501` (D337).
- **Partition maintenance** (MySQL and Oracle, D358). `ALTER TABLE t ADD PARTITION (PARTITION p
  VALUES LESS THAN (...) | VALUES IN (...), ...)` (Oracle without the parentheses) adds range
  partitions above the last one (`1493`, strictly increasing) and list partitions; on a hash table
  `ADD PARTITION PARTITIONS n` spreads every row over the new count. `DROP PARTITION p, ...`
  removes partitions with their rows (`1508` / `ORA-14083` for all of them, `1512` on hash
  tables), `TRUNCATE PARTITION p, ... | ALL` empties them, MySQL's `REORGANIZE PARTITION p, ...
  INTO (PARTITION ...)` and Oracle's `SPLIT PARTITION p AT (v) | VALUES (...) INTO (PARTITION a,
  PARTITION b)` rebuild them and route every row again, and `EXCHANGE PARTITION p WITH TABLE t`
  swaps a partition with a table of the same columns, always checking that the table's rows fit
  the partition (`WITHOUT VALIDATION` does not skip it). Oracle's `INCLUDING | EXCLUDING INDEXES`
  and `UPDATE [GLOBAL] INDEXES` are accepted. MySQL's `COALESCE PARTITION n` removes the last `n`
  hash partitions and spreads their rows over the rest (`1509` on range and list tables, `1508`
  for all of them); Oracle's `MERGE PARTITIONS a, b, ... INTO PARTITION c` joins adjacent range
  partitions (`ORA-14274` otherwise) or list partitions (with a `DEFAULT` one the result is the
  default), and `MODIFY PARTITION p ADD VALUES (...) | DROP VALUES (...)` changes a list
  partition's values (`ORA-14312`, `ORA-14313`, `ORA-14314`, `ORA-14324` for values whose rows sit
  in the default partition, `ORA-14518` for dropped values that still have rows) (D360).
  MySQL's `INSERT IGNORE` skips a row outside the named `PARTITION (...)` set (`1748`) or with no
  partition at all (`1526`) with a warning, as MySQL does.
- **Subpartitions** (D361). MySQL's `SUBPARTITION BY HASH | KEY (...) [SUBPARTITIONS n]` under a
  range or list table (`1500` under a hash table), with generated names `<partition>sp<n>` or
  each partition's own `(SUBPARTITION s, ...)` (`1485` when the count disagrees), and Oracle's
  composite partitioning with `SUBPARTITION BY HASH | RANGE | LIST` and each partition's
  subpartitions and their bounds (a range or list subpartitioning names them in every partition,
  `ORA-14160`). A subpartition is a partition that is itself partitioned; `PARTITION (...)` and
  Oracle's `SUBPARTITION (...)` select subpartitions by name. `ADD`, `REORGANIZE` and the other
  maintenance `ALTER`s do not create subpartitions yet.
- **Interval partitioning** (Oracle, D362). `PARTITION BY RANGE (c) INTERVAL (expr) [STORE IN
  (...)] (PARTITION ...)` keeps the highest bound of the listed partitions as the transition
  point; a row at or above it creates the partition `[transition + (k - 1) * interval,
  transition + k * interval)` that holds it, named `SYS_P<n>`, as Oracle does. The key is one
  column (`ORA-14750`), the interval a positive constant of the key's type, such as a number or
  `NUMTOYMINTERVAL(1, 'MONTH')` (`ORA-14752`), no listed partition may be `MAXVALUE`
  (`ORA-14761`), `ADD PARTITION` is `ORA-14760`, and a key more than 1048575 intervals above
  the transition is `ORA-14300`. An `UPDATE` that moves a row above the transition does not
  create a partition yet.
- **Row movement** (Oracle, D361). A partitioned table created in the Oracle dialect refuses an
  `UPDATE` that would move a row to another partition with `ORA-14402`, as Oracle does, until
  `ENABLE ROW MOVEMENT` (in `CREATE TABLE` or `ALTER TABLE`); `DISABLE ROW MOVEMENT` turns it off
  again. Tables of the other dialects, and Oracle tables created before EggDB kept the switch,
  move rows.
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
`ORDER BY` with `NULLS FIRST` on an ascending column (or `NULLS LAST` on a descending one), the
default in MySQL's, SQLite's and SQL Server's dialects, reads the key in two ranges, its NULL
entries first for `ASC` and last for `DESC`, when that column is the first `ORDER BY` column
after the ones fixed by `=` and every later `ORDER BY` column is `NOT NULL`; a `WHERE` that
excludes NULL from the column (a comparison, `IS NOT NULL`, `BETWEEN`, `IN` or `LIKE`) reads
one range; anything else sorts (D341). An `ORDER BY` with `LIMIT n [OFFSET m]` that no key
serves keeps only the first n + m rows while it reads, in a bounded heap whose ties keep their
arrival order, so it returns the rows the full sort would (D341). A join looks rows of its inner table up through an index
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
transaction that used the value. A sequence reads as PostgreSQL's one-row relation (`SELECT
last_value, log_cnt, is_called FROM s`, which `pg_dump` sends), and the `pg_sequences` view lists
every sequence.
`INSERT`, `UPDATE` and `DELETE` on a sequence fail with `42809` (`cannot change sequence "s"`), as
in PostgreSQL. Oracle's sequences are the same objects: `CREATE SEQUENCE s
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
  assignment would (`42804 column "c" cannot be cast automatically to type t` otherwise), or
  with `USING expr` sets each row's value to the expression over the row's old values (a
  subquery or aggregate in it is refused) (D388).
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
[COLUMN] old definition` (the whole definition is replaced, as in MySQL), `FIRST` or `AFTER
column` after `ADD`, `MODIFY` and `CHANGE` (the column moves there with its values), `ALTER [COLUMN] c SET
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

## Generated columns

A generated column computes its value from the other columns of its row (D322), in every
dialect's spelling:

| Dialect | Definition | Kind |
|---|---|---|
| PostgreSQL | `c type GENERATED ALWAYS AS (expr) [STORED \| VIRTUAL]` | `VIRTUAL` when neither is written, as in PostgreSQL 18 |
| MySQL | `c type [GENERATED ALWAYS] AS (expr) [VIRTUAL \| STORED]` | `VIRTUAL` by default |
| SQLite | `c [type] [GENERATED ALWAYS] AS (expr) [VIRTUAL \| STORED]` | `VIRTUAL` by default |
| SQL Server | `c AS expr [PERSISTED] [NOT NULL]` (a computed column) | `PERSISTED` is stored |
| Oracle | `c [type] [GENERATED ALWAYS] AS (expr) [VIRTUAL]` (a virtual column) | virtual |

A computed column without a type (SQL Server, Oracle) takes the type of its expression; one EggDB
cannot work out answers `0A000` and asks for a `CAST`. The value is computed on every `INSERT`,
on every `UPDATE` (so a change to a column it reads changes it), by `MERGE`, upserts, `COPY`,
cascaded foreign-key updates and after `BEFORE` triggers, as PostgreSQL does. A `VIRTUAL` column
is kept like a `STORED` one, so reading it costs nothing and gives the same value its expression
would; the catalogs still name it virtual.

- **Writes.** A value written into one fails with the dialect's error: PostgreSQL's `428C9`
  (`cannot insert a non-DEFAULT value into column "c"`, `column "c" can only be updated to
  DEFAULT`, detail `Column "c" is a generated column.`), MySQL's 3105 (`The value specified for
  generated column 'c' in table 't' is not allowed.`), SQLite's `cannot INSERT into generated
  column "c"` and `cannot UPDATE generated column "c"`, SQL Server's 271 and Oracle's
  `ORA-54013` and `ORA-54017`. `DEFAULT` is accepted where the dialect accepts it (PostgreSQL,
  MySQL, Oracle; SQL Server refuses it too). An `INSERT` without a column list leaves generated
  columns out in SQLite and SQL Server, and gives them `DEFAULT` elsewhere; `INSERT ... SELECT`
  into one fails. `COPY` without a column list leaves them out, and `COPY ... FROM` naming one
  answers `42P10` (`Generated columns cannot be used in COPY.`).
- **Expressions.** No subqueries, aggregates or functions whose result changes between calls
  (`random()`, `now()`, `nextval` and the like): PostgreSQL's `0A000`, `42803` and `42P17`
  (`generation expression is not immutable`), MySQL's 3763, SQLite's `subqueries prohibited in
  generated columns` and `non-deterministic functions prohibited in generated columns`, SQL
  Server's 1046, 175 and, for a `PERSISTED` column, 4936, Oracle's `ORA-54002`. SQL Server
  allows such a function in a column that is not `PERSISTED` and computes it on each read;
  EggDB keeps computed values, so it refuses that column with `0A000` and a hint. A call to a
  user-defined function is refused too (not immutable, or `0A000` for an `IMMUTABLE` one). A generated
  column may read another one only in MySQL (one defined before it, 3107 otherwise) and SQLite
  (without a loop); PostgreSQL answers `42P17`, SQL Server 1759 and Oracle `ORA-54012`. MySQL
  refuses one that reads an `AUTO_INCREMENT` column (3109), SQLite one in the primary key and
  PostgreSQL a `DEFAULT` with a generation expression.
- **Indexes** work on generated columns as on any other, except PostgreSQL's virtual ones
  (`0A000`, as in PostgreSQL 18).
- **ALTER TABLE.** `ADD COLUMN ... GENERATED ALWAYS AS (...)` computes the value of every
  existing row (SQLite refuses a `STORED` one, `Cannot add a STORED column`). PostgreSQL's
  `ALTER COLUMN c SET EXPRESSION AS (expr)` recomputes every row and the column's indexes
  (`0A000` for a column in the primary key or a unique index), and `DROP EXPRESSION [IF EXISTS]`
  keeps the values and makes the column an ordinary one (`55000` when it is not generated)
  (D385). Renaming a column
  rewrites the expressions that read it. Dropping a column a generated column reads fails:
  PostgreSQL's `2BP01` (`cannot drop column a of table t because other objects depend on it`),
  with `CASCADE` dropping the generated column too; MySQL's 3108, SQL Server's 5074, Oracle's
  `ORA-54031`, SQLite's `error in table t after drop column`.
- **Catalogs.** `pg_attribute.attgenerated` is `s` or `v` and `atthasdef` true, `pg_attrdef`
  holds the expression for `pg_get_expr`, `information_schema.columns` has `is_generated`
  `ALWAYS` and `generation_expression`; MySQL's `information_schema.columns.EXTRA` and `SHOW
  COLUMNS` say `STORED GENERATED` or `VIRTUAL GENERATED`; SQLite's `PRAGMA table_xinfo` has
  `hidden` 2 (virtual) or 3 (stored) and `PRAGMA table_info` leaves them out; SQL Server's
  `sys.columns.is_computed` is 1; Oracle's `USER_TAB_COLS.VIRTUAL_COLUMN` is `YES` with the
  expression in `DATA_DEFAULT`. `CREATE TABLE` text written in PostgreSQL's dialect (`eggdb
  dump`) keeps the generation clause.

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
  `ORA-04088: error during execution of trigger 'TRIGGER'`, and undoes the statement with
  everything its triggers did while the transaction goes on (D372); a trigger body that does not
  compile is refused at `CREATE` (`ORA-06550`) instead of being kept invalid. Line numbers count
  from the block's first line, as in Oracle. Compound, DDL, database event and crossedition
  triggers are `0A000`, and a trigger does not fire itself again.

Triggers that fire triggers nest at most 32 deep; deeper is an error, never a crash, also when
a cascade fires a trigger that sets off another cascade (D260). Rows a cascading
foreign key changes or removes (`ON DELETE CASCADE`, `SET NULL`, `SET DEFAULT`, `ON UPDATE
CASCADE`) fire the child table's `UPDATE` or `DELETE` triggers, as in PostgreSQL, SQL Server,
SQLite and Oracle: row triggers once per row, statement triggers (and a Transact-SQL trigger's
`deleted` and `inserted` tables) once per statement and child table with every cascaded row.
A PostgreSQL-dialect `AFTER` row trigger fires once its cascaded action has written all its rows
(after that action's `BEFORE` triggers), as PostgreSQL's queued events do; SQLite's and Oracle's
fire as each row is written. Triggers written in the MySQL dialect don't fire for cascades, as MySQL's own don't. A `BEFORE`
trigger may change a cascaded row's new values; one that skips a cascaded row fails the statement
with `23503` (the row would point at nothing), where PostgreSQL completes the statement and leaves
the row behind as an orphan, and one that changes or removes the row it fires
for fails it with `27000`, as PostgreSQL does. An `INSTEAD OF` trigger on a table a cascade
changes is refused (`55000`, SQL Server's 1787/1788). MySQL's `REPLACE` fires `BEFORE INSERT`,
then `BEFORE DELETE` and `AFTER DELETE` for each row it removes, then `AFTER INSERT`, MySQL's
order. SQLite's `REPLACE` conflict resolution doesn't fire `DELETE` triggers written in the
SQLite dialect (as SQLite does with `recursive_triggers` off, its default), but fires those from
another dialect or from an EggDB extension (retention, ledger, change capture), which expect to
see every removed row. Dropping
a table or view drops its triggers, and renaming one keeps them. Not yet: temporary and
constraint and event triggers, SQL Server DDL and logon triggers, Oracle compound and event
triggers, and transition tables in row-level triggers (`0A000` when the trigger fires). PostgreSQL statement triggers
fire on `TRUNCATE` too. A PostgreSQL `AFTER ... FOR EACH STATEMENT` trigger declared with
`REFERENCING OLD TABLE AS o NEW TABLE AS n` reads the rows its statement removed and added
(for `UPDATE`, the old and new versions) as tables `o` and `n` in the statements of its trigger
function, including rows that cascading foreign keys changed when their triggers fire; functions it
calls don't see them. As in PostgreSQL, `OLD TABLE` needs `DELETE` or `UPDATE` and `NEW TABLE`
needs `INSERT` or `UPDATE` among the events (`42P17`), and `TRUNCATE` triggers can't have them
(`0A000`).

## Geometry (PostGIS-compatible functions)

Geometries are written as WKT or EWKT text (`'SRID=4326;POINT(-71.06 42.36)'`) or WKB bytes
(D204). `CREATE EXTENSION postgis` is accepted, columns may be declared `geometry` or
`geography` (with or without `(Point, 4326)`), and these functions work, by PostGIS's names
(MySQL's `ST_` functions of the same names too):

- reading and writing: `ST_GeomFromText`, `ST_GeomFromEWKT`, `ST_GeogFromText`,
  `ST_GeomFromWKB`, `ST_GeomFromGeoJSON`, `ST_MakePoint`, `ST_Point`, `ST_SetSRID`, `ST_AsText`,
  `ST_AsEWKT`, `ST_AsBinary`, `ST_AsEWKB`, `ST_AsGeoJSON`;
- inspecting: `ST_X`, `ST_Y`, `ST_SRID`, `ST_NPoints`, `ST_GeometryType`, `ST_IsEmpty`,
  `ST_Envelope`;
- measuring (planar, in the coordinates' units): `ST_Distance`, `ST_Area`, `ST_Length`, and
  `ST_DistanceSphere` in metres for longitude/latitude points;
- predicates: `ST_DWithin`, `ST_Contains`, `ST_Within`, `ST_Intersects`, `ST_Disjoint`,
  `ST_Equals`;
- web maps (D250): `ST_TileEnvelope(zoom, x, y)` gives an XYZ map tile's square in Web
  Mercator (SRID 3857), `ST_Transform(geometry, srid)` projects between longitude/latitude
  (4326) and Web Mercator (3857) on its sphere, and `ST_GeoHash(point [, precision])` gives a
  point's geohash (20 characters unless a precision from 1 to 20 is given).

Geometries of two different SRIDs are refused, as in PostGIS. Not yet: a stored geometry type
(values are text, shown as EWKT rather than hex EWKB), spatial indexes (R-tree/GiST), Z and M
coordinates, geography measures on the spheroid, transformations between other SRIDs, set operations
(`ST_Union`, `ST_Intersection`, `ST_Buffer`), and SQL Server's `geometry` methods.

## citext and hstore

`citext` and `hstore` are built in (D259): `CREATE EXTENSION citext` and
`CREATE EXTENSION hstore` succeed and list in `pg_extension`, and the types keep PostgreSQL's
names in `pg_type` (OIDs 9000 to 9003, `citext[]` and `hstore[]` included, as columns, values and
parameters, D288) and its text and binary forms on the wire.

- `citext` keeps the text as written and compares, orders, groups, hashes and indexes by one
  full case fold, so a unique `citext` index refuses `'Ann'` next to `'ANN'`, and `LIKE` and
  `ILIKE` on a `citext` value ignore case, and so do the regular expression operators `~`, `~*`,
  `!~` and `!~*` when the value they test is `citext` (D288). As in the citext extension,
  `regexp_match`, `regexp_matches`, `regexp_replace`, `regexp_split_to_array`,
  `regexp_split_to_table`, `replace`, `split_part`, `strpos` and `translate` ignore case when their
  first argument is `citext`. `replace`, `split_part`, `strpos` and `translate` fold case the
  way `citext`'s `=` does, by the same full case fold over all of Unicode: they match the search
  text literally on the folded forms and return slices of the original text, so
  `replace('CAFÉ'::citext, 'é', 'e')` gives `CAFe` and `strpos('Straße'::citext, 'SS')` gives 5;
  a match that would start or end inside one character's fold (the `s` of `ß`) does not count.
  `split_part` gives NULL for a field that does not exist, as the extension's array subscript
  does. The regular expression ones add the `i` flag unless the flags hold `c`, and that flag
  ignores the case of ASCII letters only, so `regexp_replace('CAFÉ'::citext, 'é', 'e')` gives
  `CAFÉ` (D302). A `citext` argument
  anywhere else is read as `text`. Full case folding differs from PostgreSQL's
  `lower()`-based `citext`: `'ß'::citext = 'SS'` is true in EggDB and false in PostgreSQL (D259),
  and the four functions follow `=`, so PostgreSQL gives `strpos('Straße'::citext, 'SS')` 0,
  `replace('Straße'::citext, 'SS', 'x')` `Straße` and `split_part('aßbSSc'::citext, 'ss', 2)`
  `c` where EggDB gives 5, `Straxe` and `b` (D302).
- `hstore` keeps its pairs sorted as PostgreSQL does and prints `"a"=>"1", "b"=>NULL`. Its
  operators `->` (a key or an array of keys), `?`, `?|`, `?&`, `@>`, `<@`, `||` and `-` (a key,
  an array of keys or an `hstore`) and its functions `akeys`, `avals`, `skeys`, `svals`, `each`,
  `exist`, `defined`, `delete`, `slice`, `hstore(text, text)`, `hstore(text[], text[])`,
  `hstore(text[])`, `hstore_to_json`, `hstore_to_jsonb` and `hstore_to_array` work as in
  PostgreSQL. `?` is an operator only in the PostgreSQL dialect.
- The prefix operator `%%` gives an `hstore`'s keys and values alternating in a `text[]`, and
  `populate_record(record, hstore)` and `record #= hstore` set each field of a composite value
  whose name is a key to that key's value read as the field's type (`22023` when the first
  argument is not a row) (D288).

Not yet: `hstore_to_matrix` and `%#` of a non-empty `hstore` (they need multidimensional arrays,
`0A000`; an empty one gives `{}`) (D288).

## Vectors (pgvector)

Vectors are written as pgvector writes them, `'[1,2,3]'` (D203). `CREATE EXTENSION vector` is
accepted, a column may be declared `vector(n)`, and pgvector's operators and functions work on
vector text and on numeric arrays:

| Operator or function | Result |
| --- | --- |
| `a <-> b`, `l2_distance(a, b)` | Euclidean distance |
| `a <=> b`, `cosine_distance(a, b)` | cosine distance (NaN for a zero vector) |
| `a <#> b`, `inner_product(a, b)` | negative inner product, and the inner product |
| `a <+> b`, `l1_distance(a, b)` | taxicab distance |
| `vector_dims(a)`, `vector_norm(a)`, `l2_normalize(a)` | dimensions, Euclidean norm, unit vector |

```sql
SELECT id FROM items ORDER BY embedding <-> '[3,1,2]' LIMIT 5;
```

For search without an external model, `eggdb_embed(text [, dimensions])` gives a local
embedding (D205): stemmed English words and word pairs hashed into a unit vector of 256
dimensions by default, the same every time, nothing downloaded and nothing sent anywhere. It
finds texts that share words, not synonyms. `rrf_score(rank [, k])` is one list's term of
reciprocal rank fusion (`1 / (k + rank)`, k 60): add it across a vector ranking and a full-text
ranking for hybrid search.

```sql
WITH semantic AS (SELECT id, row_number() OVER (ORDER BY eggdb_embed(body) <=> eggdb_embed('sleepy cats')) AS rank FROM docs),
     lexical AS (SELECT id, row_number() OVER (ORDER BY ts_rank(to_tsvector(body), plainto_tsquery('sleepy cats')) DESC) AS rank FROM docs)
SELECT id, rrf_score(semantic.rank) + rrf_score(lexical.rank) AS score
FROM semantic JOIN lexical USING (id) ORDER BY score DESC LIMIT 10;
```

Nearest-neighbour queries are exact for now: they read every row. Not yet: `vector(n)` as its
own stored type (it is text, and its dimensions are not checked on insert), HNSW and IVFFlat
indexes in SQL (the HNSW graph is written, in `eggdb-vector`), `halfvec`, `sparsevec`,
`bit` vectors, `+`/`-`/`*` on vectors, and the binary wire format.

## Time series: time_bucket

`time_bucket(width, time [, offset | origin])` puts a time in its bucket as TimescaleDB does:
buckets of `width` counted from 2000-01-03 (a Monday, so weekly buckets start on Mondays), or
from `origin`, or shifted by an `offset` interval. Month and year widths bucket by calendar
month from January 2000. It takes `timestamp`, `timestamptz` (bucketed in UTC) and `date`,
returning the same type, and integers (`time_bucket(10, 23)` is 20). PostgreSQL's `date_bin`
works too.

```sql
SELECT time_bucket('15 minutes', at) AS bucket, avg(value)
FROM readings GROUP BY bucket ORDER BY bucket;
```

Not yet: `time_bucket_gapfill`, `locf` and `interpolate`, the `first` and `last` aggregates,
hypertables with automatic time partitions, retention policies and continuous aggregates.

## Materialized views

`CREATE MATERIALIZED VIEW [IF NOT EXISTS] name AS query [WITH [NO] DATA]` stores the query's
rows in a table of that name, `REFRESH MATERIALIZED VIEW [CONCURRENTLY] name [WITH [NO] DATA]`
replaces them with the query's current rows in one transaction, and `DROP MATERIALIZED VIEW [IF
EXISTS] name[, ...]` removes it. The defining queries are kept in
`eggdb_internal.materialized_views`. Reads of a materialized view are reads of its table, so
they are as fast as any table and see the rows of the last refresh. A materialized view may be
named in any schema (`CREATE MATERIALIZED VIEW reports.daily AS ...`), takes indexes like a
table, and is created, refreshed and dropped inside a transaction block like any other statement:
the first one in a database leaves the block usable (D308). Not yet: keeping the rows up to date
on every commit (incremental maintenance), column name lists, indexes on the view being kept
across a refresh, and `pg_class` showing it as a materialized view (`relkind` `m`) rather than a
table.

## Change streams (eggdb_cdc)

`CREATE EXTENSION eggdb_cdc` installs a transactional outbox, `eggdb_cdc.outbox`, and a row
trigger function, `eggdb_cdc.capture()` (D210). A table whose changes should stream gets a
trigger:

```sql
CREATE EXTENSION eggdb_cdc;
CREATE TRIGGER orders_cdc AFTER INSERT OR UPDATE OR DELETE ON orders
    FOR EACH ROW EXECUTE FUNCTION eggdb_cdc.capture();
```

Each change is written to the outbox in the same transaction as the change itself, so a
rolled-back change never streams and a committed one always does. `eggdb serve
--cdc-webhook http://...` delivers the outbox as Debezium-style events (`before`, `after`,
`source`, `op` of `c`, `u` or `d`, `ts_ms`), oldest first, at least once. `eggdb serve
--cdc-queue <name>` delivers the same events into EggDB's durable queue `<name>`, where AMQP
0-9-1, AMQP 1.0, STOMP and SQS clients consume them (D217). Rows a cascading foreign key or a
`REPLACE` changes or removes are captured like any other change (D260). Not yet:
Kafka and MQTT sinks, Debezium's schema section, and capture without triggers.

### Transactional outbox and inbox

The outbox pattern needs nothing more: write the business row and capture it (or insert an
event row into a captured table) in one transaction, and `--cdc-queue` or `--cdc-webhook`
relays it at least once. The inbox pattern makes a consumer's effect happen once even though
delivery is at least once. Every message from a durable queue carries an id that stays the
same when it is delivered again (`<queue>-<number>`: the `message-id` header over STOMP, the
`message-id` property over AMQP 1.0, the message id over SQS), so the consumer records it in
the same transaction as its effect:

```sql
CREATE TABLE inbox (message_id text PRIMARY KEY, received_at timestamptz NOT NULL DEFAULT now());
BEGIN;
INSERT INTO inbox (message_id) VALUES ('orders-42') ON CONFLICT DO NOTHING;
UPDATE stock SET reserved = reserved + 1 WHERE sku = 'egg-1';
COMMIT;
```

When the `INSERT` inserts no row, the message was handled before: roll back instead of
committing, and acknowledge the message.

## Tenants (eggdb_tenancy)

`CREATE EXTENSION eggdb_tenancy` isolates tenants that share tables (D234):

```sql
CREATE EXTENSION eggdb_tenancy;
CREATE TABLE notes (id serial PRIMARY KEY, tenant text NOT NULL, body text);
SELECT eggdb_tenancy.isolate('public', 'notes', 'tenant');
INSERT INTO eggdb_tenancy.tenants (tenant, max_rows) VALUES ('acme', 10000);
SET eggdb.tenant = 'acme';
```

After `isolate`, a session sees and writes only the rows whose tenant column equals its
`eggdb.tenant` setting; a session that has set none sees no rows and cannot add any, and a
new row for another tenant fails with SQLSTATE `42501`. This holds for roles that are not
superusers, so connect the application as such a role. `TRUNCATE` of an isolated table is
refused (`42501`) for every role but a superuser, since it would remove every tenant's rows. A
cascading foreign key may change or remove its rows: the quota trigger fires for the rows a
cascade changes, as for an `UPDATE`. A
tenant with `max_rows` gets SQLSTATE `53400` once it holds that many rows in a table.

## Ledger tables (eggdb_ledger)

`CREATE EXTENSION eggdb_ledger` makes tables append-only and tamper-evident (D236):

```sql
CREATE EXTENSION eggdb_ledger;
CREATE TABLE payments (id integer PRIMARY KEY, amount numeric NOT NULL);
SELECT eggdb_ledger.protect('public', 'payments');
INSERT INTO payments (id, amount) VALUES (1, 10);
SELECT eggdb_ledger.verify('public', 'payments');
```

`protect` adds `ledger_seq`, `ledger_previous` and `ledger_hash` columns: each new row carries
the hash of the row before it and its own SHA-256, and updates, deletes, `TRUNCATE` and `DROP TABLE` (D392) fail with SQLSTATE
`55000`. `verify` recomputes the chain and returns how many rows do not match (0 when intact).
Protect a table before adding rows to it. `sha256(x)` and `sha512(x)` return a digest as
`bytea`, of `bytea` or of text's UTF-8 bytes.

## Text from documents: extract_text

`extract_text(file bytea)` returns the text of a Word, Excel or PowerPoint file (`.docx`,
`.xlsx`, `.pptx`) or an OpenDocument file (`.odt`, `.ods`, `.odp`), one paragraph per line, so
uploaded documents can be searched with the full-text functions (D249):

```sql
SELECT id FROM uploads WHERE to_tsvector(extract_text(body)) @@ to_tsquery('invoice & overdue');
```

Word documents give their paragraphs with tabs and line breaks, spreadsheets their shared
strings, presentations each slide in order with a blank line between slides, and OpenDocument
files their headings and paragraphs. Files are read with EggDB's own ZIP and DEFLATE code, each
part checked against its CRC-32 and limited to 64 MiB once decompressed; anything else (an
encrypted, damaged or unsupported file) fails with SQLSTATE `22023`.

## Industry messages: FIX, HL7 v2 and EDIFACT

Messages from trading, healthcare and supply-chain systems parse into `jsonb` with plain
functions, so they can be stored, indexed and queried next to everything else (D243):

```sql
SELECT fix_field(raw, 55) AS symbol FROM orders;            -- one FIX tag's value, or NULL
SELECT fix_parse(raw) FROM orders;                          -- [{"tag": 8, "value": "FIX.4.4"}, ...]
SELECT msg -> 1 -> 'fields' -> 4 FROM (SELECT hl7_parse(raw) AS msg FROM lab_feed) AS m;
SELECT edifact_parse(raw) -> 1 -> 'elements' FROM invoices;
```

- `fix_parse` reads `tag=value` fields separated by SOH (or `|` when the text has no SOH),
  keeps repeated tags in order, and checks `BodyLength(9)` and `CheckSum(10)` when present.
- `hl7_parse` takes the separators from the `MSH` segment, splits segments on carriage returns
  or new lines, and gives each segment its fields numbered as in HL7 (`MSH-1` is the field
  separator). A field with repetitions, components or subcomponents becomes a nested array,
  and the `\F\`, `\S\`, `\T\`, `\R\` and `\E\` escapes are decoded.
- `edifact_parse` honours a `UNA` service string advice and the release character, and gives
  each segment its tag and elements, an element with components becoming an array.

A malformed message fails with SQLSTATE `22P02` and says what is wrong; nothing is guessed. A
message is at most 4 MiB (`54000`).

## Schema diagrams (eggdb_console)

`CREATE EXTENSION eggdb_console` adds `eggdb_console.er_diagram(schema, format)`, which returns
the schema's tables, columns, primary keys and foreign keys as a Mermaid `erDiagram` (`'mermaid'`)
or a Graphviz graph (`'dot'`), ready to paste into Markdown or render with `dot -Tsvg` (D237):

```sql
CREATE EXTENSION eggdb_console;
SELECT eggdb_console.er_diagram('public', 'mermaid');
```

## Reports and alerts (eggdb_bi)

`CREATE EXTENSION eggdb_bi` sends query results to webhooks on a schedule (D238):

```sql
CREATE EXTENSION eggdb_bi;
CREATE EXTENSION pg_cron;
INSERT INTO eggdb_bi.reports (name, query, target, only_when_rows)
VALUES ('low-stock', 'SELECT item, quantity FROM stock WHERE quantity < 5', 'http://hooks.internal/stock', true);
SELECT cron.schedule('low-stock', '*/5 * * * *', $$SELECT eggdb_bi.run_report('low-stock')$$);
```

`run_report` queues `{"report", "rows", "count", "text"}` for the target; with `only_when_rows` it
queues nothing when the query finds no rows, which makes the report an alert. `eggdb serve` posts
the queue at least once, oldest first, to `http://` targets allowed by
`EGGDB_REPORT_TARGETS` (comma-separated URLs: the same host and port, and a path that starts with
the entry's). It sends nothing when that is unset, and drops a
message whose target is not allowed.

## Data catalog, quality rules and lineage (eggdb_catalog)

`CREATE EXTENSION eggdb_catalog` keeps a business glossary, data quality rules and lineage in
ordinary tables of the `eggdb_catalog` schema (D244):

```sql
CREATE EXTENSION eggdb_catalog;
INSERT INTO eggdb_catalog.terms VALUES ('PII', 'Personal data under GDPR', 'privacy-team');
INSERT INTO eggdb_catalog.tags VALUES ('public', 'customers', 'email', 'PII');
INSERT INTO eggdb_catalog.quality_rules (rule_name, schema_name, table_name, row_condition)
    VALUES ('email present', 'public', 'customers', 'email IS NOT NULL');
SELECT eggdb_catalog.check_quality();
INSERT INTO eggdb_catalog.lineage VALUES ('public.customers', 'mart.revenue', 'build_mart');
SELECT * FROM eggdb_catalog.upstream('mart.revenue');
SELECT eggdb_catalog.openlineage_event('build_mart', 'COMPLETE');
```

- `terms` is the glossary, each term with its definition and steward; `tags` attaches terms to a
  table (`column_name` left empty) or a column, and refuses a term the glossary doesn't have.
- `quality_rules` holds one boolean condition per rule, true for a good row. `check_quality()`
  checks every rule against its table, appends a row per rule to `quality_results` with the rows
  checked and the rows that failed (a NULL result counts as failed), and returns how many rules
  found failing rows, so a pg_cron job can alert on it.
- `lineage` records which asset feeds which, and through which job. `upstream(asset)` walks it
  back and returns every upstream asset with its distance in hops, and
  `openlineage_event(job, event_type)` returns that job's inputs and outputs as an OpenLineage
  `RunEvent`, ready to post to any OpenLineage collector.

A rule's condition runs as SQL with the privileges of whoever calls `check_quality()`, the way a
view's query does, so only roles trusted to write SQL should be granted `INSERT` on
`quality_rules`.

## Privacy: format-preserving tokens, noise and k-anonymity

Three tools for sharing data without exposing people (D245):

```sql
SELECT ff1_encrypt(card_number, :'key', '\x'::bytea) FROM payments;        -- same length, digits only
SELECT ff1_decrypt(token, :'key', '\x'::bytea) FROM payments_shared;
SELECT ff1_encrypt(code, :'key', '\x'::bytea, 36) FROM vouchers;            -- 0-9 and a-z
SELECT count(*) + laplace_noise(1, 0.5) FROM visits WHERE city = 'Hanoi';
CREATE EXTENSION eggdb_privacy;
SELECT eggdb_privacy.k_anonymity('public', 'patients', ARRAY['zip', 'age_band']);
```

- `ff1_encrypt(value, key, tweak [, radix])` and `ff1_decrypt` are FF1 format-preserving
  encryption (NIST SP 800-38G) over AES with a 16, 24 or 32-byte key: a value written with the
  first `radix` characters of `0-9a-z` (10 by default) becomes a token of the same length and
  alphabet, so it still fits the column and its checks, and only the key holder turns it back.
  The value must allow at least a million possibilities (six digits) and at most 72 digits.
- `laplace_noise(sensitivity, epsilon)` draws noise from the Laplace distribution with scale
  `sensitivity / epsilon`, from the cryptographically secure random source: adding it to a
  count (sensitivity 1) makes that count epsilon-differentially private.
- `eggdb_privacy.k_anonymity(schema, table, columns)` returns the size of the smallest group of
  rows sharing the same values of the quasi-identifier columns: the table is k-anonymous for
  that k.

## Retention, legal hold and erasure (eggdb_retention)

`CREATE EXTENSION eggdb_retention` adds write-once retention, legal holds and erasure requests
that wait for them (D246):

```sql
CREATE EXTENSION eggdb_retention;
SELECT eggdb_retention.retain('public', 'trades', 'booked_at', interval '7 years');
SELECT eggdb_retention.place_hold('case-42', 'public', 'trades', 'litigation');
SELECT eggdb_retention.request_erasure('public', 'trades', 'client = ''ada''');
SELECT eggdb_retention.lift_hold('case-42');
SELECT eggdb_retention.process_erasures();
```

- `retain(schema, table, time_column, period)` makes a table write-once: rows are never
  updated, and a row is deleted only when its time column plus the period has passed.
- `place_hold(name, schema, table, reason)` stops every update and delete on the table until
  `lift_hold(name)`; holds stay in `eggdb_retention.holds` with when they were placed and lifted.
- A guarded table also refuses `TRUNCATE`, `eggdb_retention.rules` cannot be changed or removed,
  and a hold can only be lifted (all `55000`). Dropping a table or a trigger, and turning a
  trigger off, need a superuser, also inside `DO` blocks, routines and triggers; a superuser can
  still get around retention. A cascading foreign key or a `REPLACE` that would change or
  remove rows of a guarded table is refused (`0A000`), as for any table with row triggers.
- `request_erasure(schema, table, condition)` records the request in
  `eggdb_retention.erasure_requests` and deletes the matching rows at once when nothing holds
  them; otherwise the request waits with `waiting_for` saying why (`legal hold <name>` or
  `retention`), and `process_erasures()`, run by hand or by a pg_cron job, carries it out later.

Refused changes fail with SQLSTATE `55000`. These are rules enforced by triggers, so a role that
may drop triggers or tables can still go around them; grant those rights only to administrators.

## Leases, leader election and feature flags (eggdb_coordination)

`CREATE EXTENSION eggdb_coordination` gives services locks with fencing tokens, leader election
and OpenFeature-style flags, kept in the database they already use (D247):

```sql
CREATE EXTENSION eggdb_coordination;
SELECT eggdb_coordination.acquire('scheduler', 'node-a', interval '30 seconds');  -- token or NULL
SELECT eggdb_coordination.leader('scheduler');
SELECT eggdb_coordination.release('scheduler', 'node-a');
INSERT INTO eggdb_coordination.flags (flag_key, variants, default_variant, targeting)
    VALUES ('new-checkout', '{"on": true, "off": false}', 'off',
            '[{"attribute": "country", "in": ["VN"], "variant": "on"}]');
SELECT eggdb_coordination.evaluate_flag('new-checkout', '{"country": "VN"}'::jsonb);
```

- `acquire(lease, holder, ttl)` returns a fencing token when the holder gets or renews the
  lease, and NULL while someone else holds it. Every new holder gets a larger token than any
  before, so a storage service that remembers the largest token it has seen can refuse writes
  from a holder whose lease has expired. Renew before the time to live runs out;
  `leader(lease)` is the current holder, and `release` gives the lease up.
- `evaluate_flag(key, context)` returns OpenFeature resolution details as `jsonb`: `value`,
  `variant` and `reason` (`TARGETING_MATCH`, `DEFAULT`, `STATIC`, `DISABLED`), or
  `reason: ERROR` with `errorCode: FLAG_NOT_FOUND`. Targeting rules are tried in order, and each
  matches when a context attribute is one of the listed values.

## Resumable copies with verified differences (eggdb_migration)

`CREATE EXTENSION eggdb_migration` moves a large table in batches that survive interruptions,
then proves the copy is complete (D251):

```sql
CREATE EXTENSION eggdb_migration;
SELECT eggdb_migration.copy_batch('accounts', 'legacy', 'accounts', 'public', 'accounts', 'id', 10000);
SELECT * FROM eggdb_migration.compare('legacy', 'accounts', 'public', 'accounts', 'id');
```

- `copy_batch(job, source_schema, source_table, target_schema, target_table, key, rows)` copies
  the next rows (1 to 1,000,000) in key order after the largest key already in the target, so
  a job can stop at any batch and continue later, from any session; it returns how many rows it
  copied (0 when done) and adds them to `eggdb_migration.jobs`. Run it in a loop or as a pg_cron
  job. Rows the source gains below the largest copied key are not copied, and `compare` reports
  them.
- `compare(source_schema, source_table, target_schema, target_table, key)` returns every key
  that is `missing in target`, `missing in source` or `different` (any column), so an empty
  result proves the two tables hold the same rows.

## Unicode normalization

`normalize(text [, form])` returns the text in a Unicode normalization form, NFC unless the form
(`'NFC'`, `'NFD'`, `'NFKC'` or `'NFKD'`, any case) says otherwise, and `is_normalized(text [, form])`
says whether it already is (D233). Both follow Unicode 16.0 and pass Unicode's own conformance
file. The form is written as a string for now; PostgreSQL's bare keyword (`normalize(x, NFKC)`)
and `x IS NFC NORMALIZED` come later.

## Foreign sources (eggdb_foreign)

`eggdb_foreign(source, query)` reads rows from outside the database, one `json` object per row
(column name to value), so the rest of the query filters, joins and aggregates them like any
rows (D229):

```sql
SELECT o->>'name' AS name, (o->>'total')::numeric AS total
FROM eggdb_foreign('postgresql://reports@10.0.0.7:5432/sales', 'SELECT name, total FROM orders') AS f(o);
SELECT count(*) FROM eggdb_foreign('file:/data/lake/events-2026-10.parquet', '*');
```

A `postgresql://` source is another EggDB server or a PostgreSQL server, asked the query as it is
(the password from `PGPASSWORD`); a `file:...parquet` source is read whole (`'*'`). Nothing is
reachable until the operator allows it: `eggdb sql` and `eggdb serve` read the comma-separated
entries of `EGGDB_FOREIGN_SOURCES` (for example `postgresql://reports@10.0.0.7,file:/data/lake/`).
A PostgreSQL entry allows exactly that user on that host and port (and only its database when it
names one, and a source's `sslrootcert` only when the entry gives the same one); a `file:` entry
allows the files under that directory once both paths are made absolute, so `..` cannot leave it.
A source outside them is refused with SQLSTATE `HV000`, and only a superuser may call the
function (`42501`). With none set, the function answers `0A000`.

## Scheduled jobs (pg_cron)

`CREATE EXTENSION pg_cron` installs the `cron` schema as the pg_cron extension has it (D200):
the tables `cron.job` and `cron.job_run_details`, and the functions `cron.schedule(schedule,
command)`, `cron.schedule(job_name, schedule, command)` (which replaces a job of that name)
and `cron.unschedule(job_id | job_name)`. While `eggdb serve` runs, it runs each active job
whose schedule fires in the minute (UTC), each in its own transaction in the PostgreSQL dialect,
and records the run in `cron.job_run_details` with `succeeded` or `failed` and the message.

```sql
CREATE EXTENSION pg_cron;
SELECT cron.schedule('nightly-cleanup', '0 3 * * *', $$DELETE FROM events WHERE at < now() - interval '30 days'$$);
SELECT * FROM cron.job_run_details ORDER BY start_time DESC LIMIT 5;
```

Schedules are cron's five fields (minute, hour, day of month, month, day of week) with `*`,
lists, ranges, `/` steps and month and weekday names, or `@hourly`, `@daily`, `@weekly`,
`@monthly` and `@yearly`; when both day fields are restricted, either one matching fires the
job. A job whose schedule does not read never runs. Every job runs with a superuser's rights,
whoever scheduled it, so scheduling needs writing `cron.job`: grant that to administrators only,
as pg_cron's superuser-only table is. Not yet: running a job as its `username`, pg_cron's
`'n seconds'`
schedules and `$` for the last day of the month, `cron.alter_job`, `cron.schedule_in_database`,
`cron.timezone`, checking a schedule when it is added, SQL Server Agent jobs, MySQL events and
Oracle's `DBMS_SCHEDULER`.

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
- Transaction control (D262): `COMMIT [AND CHAIN]` and `ROLLBACK [AND CHAIN]`
  inside a procedure, or a `DO` block, run by a top-level `CALL`/`DO` outside a transaction
  block (and in procedures it `CALL`s directly) end the transaction and start a new one: work
  committed before a later error stays committed, and `FOR` loops over queries keep their rows
  across the commit. As in PostgreSQL they are refused with `2D000` in a function, in a `CALL`
  inside `BEGIN ... COMMIT`, in a procedure with a `SET` clause, and in a block with an
  `EXCEPTION` handler ("cannot commit while a subtransaction is active"). Such a `COMMIT` or
  `ROLLBACK` closes the cursors other than `WITH HOLD` that are open, also those the procedure
  opened, so a later `FETCH` from one fails with `34000` (`cursor "c" does not exist`); cursors of
  `FOR` loops survive it (D280). It also releases the transaction-level advisory locks
  (`pg_advisory_xact_lock` and the rest) taken before it.
- Cursors as portals: a cursor a routine opens (`OPEN c FOR ...`, the cursor's name is the
  `refcursor` variable's value, or `<unnamed portal N>` when it is `NULL`) stays open after the
  routine returns until the transaction ends, so the caller reads a returned `refcursor` (also
  through an `INOUT` parameter) with `FETCH`, as Npgsql and psycopg do, and another routine can
  `FETCH` from it. SQL statements: `DECLARE name [BINARY] [INSENSITIVE] [[NO] SCROLL] CURSOR
  [{WITH | WITHOUT} HOLD] FOR query` (`25P01` outside a transaction block unless `WITH HOLD`),
  `FETCH` and `MOVE [NEXT | PRIOR | FIRST | LAST | ABSOLUTE n | RELATIVE n | n | ALL | FORWARD
  [n | ALL] | BACKWARD [n | ALL]] [FROM | IN] name`, and `CLOSE {name | ALL}`; cursors other
  than `WITH HOLD` close when their transaction ends, and `DISCARD ALL` closes them all. A
  cursor's rows are read when it is opened. `refcursor` is a type of its own holding the
  cursor's name (D280): `pg_typeof` names it, results send it with type OID 1790, it converts
  to and from `text`, and a parameter declared with OID 1790 reads as text. A table column
  declared `refcursor` is stored and shown as `text`. `refcursor[]` (OID 2201) is an array of
  cursor names (D301): `ARRAY[c1, c2]` of `refcursor` variables, `'{a,b}'::refcursor[]`,
  variables, parameters and results of that type; a table column declared `refcursor[]` is
  stored and shown as `text[]`.
- Settings in routines (D301): a function's or procedure's `SET name {TO | =} value` and `SET
  name FROM CURRENT` (the value the parameter has at `CREATE`, written into the definition as
  `SET name TO 'value'`) apply while it runs, `search_path` and `TimeZone` included, and the old
  value comes back when it returns, also on an error. `SET`, `SET LOCAL`, `RESET name` and
  `set_config(...)` inside a PL/pgSQL or `sql` routine apply to the statements after them and to
  the routines they call: a plain `SET` lasts for the session after the routine (also over a
  `SET` clause of the same parameter), `SET LOCAL` lasts to the end of the transaction, except
  for a parameter the routine's own `SET` clause names, which comes back at its exit. A
  procedure's `COMMIT` ends its `SET LOCAL` values and its `ROLLBACK` also undoes the plain `SET`s
  made since the last `COMMIT`; an exception handler that catches an error undoes the settings
  its block changed. `RESET ALL` inside a routine resets every parameter the way a `RESET` of
  each would, and `SET eggdb.dialect` inside one switches the session's dialect once the
  statement ends (the routine's own statements keep its language; inside a transaction block the
  switch is refused with a warning, as a top-level `SET eggdb.dialect` is) (D318).
  `current_setting(name)` reads the value a routine set earlier in the same statement, as in
  `SELECT set_tenant(), current_setting('app.tenant')`, and a `set_config` earlier in the same
  select list; an unset custom parameter is an error only when the call runs, and is titled
  `current_setting`. `ALTER FUNCTION | PROCEDURE | ROUTINE name [(args)] SET name {TO | =} value
  | SET name FROM CURRENT | RESET name | RESET ALL` changes the settings stored with the routine,
  several in one statement, as its `CREATE ... SET` clause does (D318). `set_config` takes only
  constants and `$n` parameters as its arguments (a PL/pgSQL variable or another expression, as
  in `set_config('app.tenant', p_tenant::text, true)`, is `0A000`).
- Polymorphic types (D264): parameters and results of `sql` and PL/pgSQL functions and
  procedures may be `anyelement`, `anyarray`, `anynonarray`, `anyenum`, `anycompatible`,
  `anycompatiblearray` and `anycompatiblenonarray`, also in `OUT` parameters, `RETURNS SETOF`,
  `RETURNS TABLE` and `VARIADIC`. Each call resolves them from its arguments: every `any*`
  argument of the `anyelement` family has the same type (an array's element for `anyarray`), the
  `anycompatible` family takes the arguments' common type (`integer` and `numeric` give
  `numeric`), and the routine runs with its parameters, variables declared `%TYPE` of them and
  its result typed accordingly. Untyped literals (a quoted string without a cast) take the type
  the other arguments give, or `text`; a typed `text` argument (`'5'::text`, a `text` column)
  sets the type like any other, so `same('5'::text, 5)` fails with `42883` as in PostgreSQL. Arguments that don't agree make the routine not a candidate, so the call fails with
  `42883` as in PostgreSQL (a concrete overload wins over a polymorphic one); a default that
  doesn't agree with the arguments is `42804`, and a polymorphic result without a polymorphic
  argument of its family is refused at `CREATE` (`42P13`). `pg_proc` and `\df` show the
  pseudo-types. `$0` is not declared yet, and an argument that is only `NULL` resolves to `text`.

Limits (D45): calls nest at most 32 deep (`54001`), and fewer when the session's thread runs
out of stack first (`54001`, "stack depth limit exceeded": the servers and the `eggdb` command
give each session 16 MiB, an embedding program that runs deep routines gives its thread `eggdb_sql::SESSION_STACK_BYTES`
and calls `eggdb_sql::set_thread_stack_size`), every loop and statement can be cancelled,
a body nests at most 64 levels and holds at most 100,000 statements, a routine's text is at most
1 MiB, and a routine or session holds at most 1000 open cursors. Not yet: `regprocedure`, functions that write when called once per row (`0A000`; call them on their
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
`ALTER TABLE t SET SCHEMA s` (`ALTER TABLE v RENAME TO w` renames a view too, as PostgreSQL
allows and dbt uses). `database.schema.name` works when the database is the one the
session is connected to (dbt writes names this way), and is `0A000` for another database; a
column may be written `schema.table.column`. `public` always exists and cannot be dropped or renamed;
`pg_catalog`, `information_schema` and names starting with `pg_` are reserved (`42939`). Errors:
`3F000 schema "x" does not exist`, `42P06 schema "x" already exists`, and messages show
qualified names as `app.t`.

Unqualified names follow `search_path`, a session setting (default `"$user", public`): `SET
search_path [TO | =] app, public`, `RESET search_path`, `SHOW search_path`, `SELECT
pg_catalog.set_config('search_path', '', false)` (constants or parameters; `true` for a
transaction's setting, D301), `current_schema()`
and `current_schemas(bool)` (shown as `{pg_catalog,app,public}`). A name is looked up in each
schema of the path that exists, in order; `pg_catalog` is searched first for functions
(`pg_catalog.nextval`, `pg_catalog.set_config` and the other functions that exist). New objects
are created in the first schema of the path that exists (`3F000 no schema has been selected to
create in` when there is none). The other dialects have one schema: SQLite's `main.t` (`temp.t`
is the session's temporary schema), Oracle's `USER.t` for the session's user's own tables and
`hr.t` for schema `hr` (D127), MySQL's `db.t` for the connected database.

**Views**, in every dialect: `CREATE [OR REPLACE] VIEW name [(columns)] AS query`, in PostgreSQL
`CREATE [OR REPLACE] [TEMP] RECURSIVE VIEW name (columns) AS query` (D299; the column list is
required, and the view is stored as `WITH RECURSIVE name (columns) AS (query) SELECT columns FROM
name`, as the manual defines it), `DROP VIEW
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
| `REJECT_LIMIT n` | with `ON_ERROR ignore` only (`22023` otherwise): the copy fails with `22P02` once more than `n` rows were skipped; `n` must be greater than zero (`22023`) |
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
`REJECT_LIMIT n` (PostgreSQL 18) bounds the skipping: the row that makes the count exceed `n`
fails the whole copy with `22P02` `skipped more than REJECT_LIMIT (n) rows due to data type
incompatibility` and a `CONTEXT` naming its line, column and value, and nothing is loaded
(D324).

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
(every trigger, of any dialect, with `tgtype`, `tgenabled`, `tgattr` and `tgargs`), `pg_policy`
and the view `pg_policies` (every row security policy, D326), and the empty `pg_inherits`, `pg_statistic_ext`, `pg_publication`,
`pg_publication_rel`, `pg_publication_namespace`, `pg_rewrite`,
`pg_auth_members`, `pg_opclass`, `pg_enum` and `pg_range`, `pg_proc` with the array
functions `array_in`, `array_out`, `array_recv` and `array_send` (D135) and every user function
and procedure (D158), and `pg_language` (`internal`, `c`, `sql`, `plpgsql`), the views `pg_tables`, `pg_views` and
`pg_matviews` (empty: no materialized views are listed yet), each with PostgreSQL 16's columns
and column types. An identity column shows its implicit sequence, `<table>_<column>_seq`, in
`pg_class` and `pg_sequence` and through `pg_get_serial_sequence`, so SQLAlchemy and other ORMs
reflect it as an identity. `setval` on that name succeeds when it leaves the column's next
value where it is (Django's and Rails' resets to the largest id), and `nextval`, `currval` and
any other `setval` are `0A000`, since the next value always follows the column's largest value.
Oracle's and SQL Server's dictionaries don't list these sequences. `pg_get_functiondef`, `pg_get_function_arguments`,
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

**Types and functions.** `oid`, `xid`, `name`, `pg_node_tree`, `aclitem` (PostgreSQL's `grantee=privileges/grantor` text, as `pg_class.relacl`
and `\dp` show a table's grants, and `pg_attribute.attacl` a column's; NULL where an object has no grants;
a cast to `aclitem` reads the same text),
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
`pg_relation_is_publishable`, `pg_partition_ancestors`, `pg_partition_tree` and `pg_partition_root` (tables and indexes, D365),
`pg_encoding_to_char`, `obj_description`, `col_description`, `pg_get_serial_sequence` (the
sequence a `serial` column owns, schema-qualified, or NULL), `to_regclass` and `to_regtype`
(NULL for a name that does not exist), `pg_typeof` (the `regtype` of its argument's type,
`unknown` for a quoted constant without a cast; D280), `generate_subscripts`, `pg_get_triggerdef` (the
trigger's `CREATE TRIGGER` as written), and `pg_get_ruledef`, `pg_get_statisticsobjdef_columns`
and `shobj_description`, which return NULL.
`regproc` takes and shows the name of the built-in functions EggDB names: `array_in`,
`array_out`, `array_recv` and `array_send`, the `typinput`, `typoutput`, `typreceive` and
`typsend` of every array type in `pg_type`; another name is `42883`. `regprocedure` (oid 2202)
takes `name(type, ...)`, finds the function with exactly those input argument types (built-in or
user routine), and shows it as `name(type,...)` (D390).

psql's `+` forms (`\d+`, `\dt+`, `\dv+`, `\ds+`, `\di+`, `\dn+`, `\df+`: owner, access
method, storage, size and descriptions), `\df`, `\dT`, `\dD`, `\dp` (grants as PostgreSQL's
`aclitem` text, the owner's entry first), `\dx`, `\dd`, `\db`, `\dC`, `\dy`, `\dRp` and `\dRs`
answer psql 16's own queries; the catalog tables answer `tableoid` as a hidden system column.

**Not yet:** system columns (`tableoid`, `ctid`, `xmin` and the rest) on user tables, rows in
`pg_cast` (so `\dC` lists nothing), the composite row type of each table, and a view column of a
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

Npgsql resets a connection that holds prepared statements (after `NpgsqlCommand.Prepare()`, or
with `Max Auto Prepare` set) with one query of `SET SESSION AUTHORIZATION DEFAULT`, `RESET ALL`,
`CLOSE ALL`, `UNLISTEN *`, `SELECT pg_advisory_unlock_all()`, `DISCARD SEQUENCES` and `DISCARD
TEMP`, which runs and keeps the prepared statements. `SET SESSION AUTHORIZATION` takes
`DEFAULT` or the session's own user (`0A000` for another). Array parameters bind in the text and
binary formats with PostgreSQL's array type OIDs, and `pg_sleep`, `pg_notify`, the waiting
advisory lock functions, `pg_advisory_unlock_all()` and functions declared `RETURNS void` report
the `void` type (OID 2278) (D265). As in PostgreSQL, a PL/pgSQL function returning `void`
yields an empty, non-NULL value and an SQL-language one yields NULL (D288).

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
  [__EFSavePoint]`), where rolling back to the save point (in SQL or as a TDS transaction manager
  request) undoes only what came after it. EF Core 9's migration lock works too: `DECLARE @result
  int; EXEC @result = sp_getapplock @Resource = '__EFMigrationsLock', @LockOwner = 'Session',
  @LockMode = 'Exclusive'; SELECT @result` and the matching `sp_releaseapplock` (see Application
  locks below).

**Advisory locks.** `pg_advisory_lock`, `pg_try_advisory_lock`, `pg_advisory_unlock` and their
`_shared` forms take one `bigint` or two `integer`s, as do the transaction-level
`pg_advisory_xact_lock` and `pg_try_advisory_xact_lock` (and `_shared`); `pg_advisory_unlock_all()`
releases the session's session-level locks. They are held by the session in one table for every
session over the same database: session-level locks count re-entries and last until unlocked or
until the session ends, transaction-level locks until the transaction ends (the statement, outside
one). The functions that wait and `pg_advisory_unlock_all()` return `void`.

**Application locks.** SQL Server's `sp_getapplock @Resource, @LockMode, @LockOwner, @LockTimeout,
@DbPrincipal` and `sp_releaseapplock @Resource, @LockOwner, @DbPrincipal` (named or positional
arguments) take and release a lock on a resource name in the same table as the advisory locks.
`@Resource` is compared byte for byte and cut to 255 characters; `@LockMode` is `Shared`,
`Update`, `IntentShared`, `IntentExclusive` or `Exclusive`, with SQL Server's compatibility
between them; `@LockOwner` is `Transaction` (the default, which needs an open transaction and
ends with it) or `Session` (until released or the session ends; each take needs its own
release); `@LockTimeout` is in milliseconds, `-1` or none waits as long as it takes and `0`
returns at once. `EXEC @result = sp_getapplock ...` sets `@result` to 0 (granted at once), 1
(granted after waiting), -1 (timed out), -2 (the wait was cancelled; the cancel still ends the
batch) or -3 (deadlock victim: the session it waits for waits, directly or through others, for
an application lock this session holds, so the request that closes the cycle is refused and
the others keep waiting) (D291); `sp_releaseapplock` returns 0. A bad argument or
releasing a lock the session does not hold returns -999 with SQL Server's error 1223 to 1228,
ending the statement and not the batch. `APPLOCK_MODE('public', resource, owner)` names the mode
the session holds (`NoLock` when none) and `APPLOCK_TEST('public', resource, mode, owner)` returns
1 when the lock could be granted now, 0 when not. `@DbPrincipal` is accepted and not checked.
A cycle through PostgreSQL's advisory locks or row locks is not seen by the application-lock
deadlock check.

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
it was created and the relations it names; an older release refuses it by name. A view with a
`citext` or `hstore` column is written as version 12 instead (D259,
`BASE_TYPES_VIEW_FORMAT_VERSION`), the same layout with those types allowed in its columns; every
other view keeps 8, and a version that does not match its columns is damage. A table with a
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

Version 11 (D259, `BASE_TYPES_FORMAT_VERSION`) is written only for a table with a `citext` or
`hstore` column: the version 10 layout, whose column types may now be `23` with kind `5`, a
built-in base type, and its OID (`9000` `citext`, `9002` `hstore`). Every other table keeps its
version and bytes, and a version 11 table without such a column, or such a column in an older
version, is damaged. In rows, a `citext` is tag `27` and its text, length-prefixed as `text`'s
(the text tag is not reused, because values inside composites and arrays are read by their tag
alone), and an `hstore` tag `28`: its pair count, then each pair's key (length-prefixed), a byte
`0` for a NULL value or `1` followed by the value (length-prefixed); pairs are stored sorted as
PostgreSQL sorts them (by key length, then bytes), and unsorted or duplicate keys are damage. In
keys a `citext` is its case-folded text, keyed as text is. An older release refuses a version 11
table by name. Table versions skip 8 and 12, which are view versions.

A SQLite column that holds every storage class is type tag `24` when its affinity is `BLOB`
(D153) and tag `25` followed by its affinity's code (`1` `TEXT`, `2` `NUMERIC`, `3` `REAL`)
otherwise. Version 13 (D305, `AFFINITY_FORMAT_VERSION`), the version 11 layout with tag `25`
allowed, is written for a table with a tag `25` column and for a table whose primary key or any
index (a `UNIQUE` constraint's included) covers a tag `24` or tag `25` column, since those keys
use the encoding below, which v0.2.0 does not know. A `BLOB` or untyped column that no key
covers stays tag `24` in version 10, byte for byte as v0.2.0 writes it, so `CREATE INDEX` on
such a column rewrites its table's definition as version 13. Every other table keeps its
version and bytes; a version 13 table with neither, a version 10 or 11 table keying a tag `24`
column, tag `25` in an older version, a code other than those three, or tag `25` in a view or
a user-defined type is damage, and an older release refuses a version 13 table by name. A view
records no type for such a column. Version 14 (D322, `GENERATED_COLUMNS_FORMAT_VERSION`) is written only for a
table with generated columns: every section of version 13, then a varint count (at least 1) and,
per generated column in increasing position, a varint position, a kind byte (`0` virtual, `1`
stored), a dialect byte and the expression's text (varint length, UTF-8). A table without
generated columns keeps its version and bytes; an empty list, a position out of range or out of
order, a generated column with a default or auto-increment, an unknown kind or dialect, or an
empty expression is damage, and an older release refuses a version 14 table by name. In rows these columns hold the tags of the four classes
(`3` integer, `5` real, `7` text, `18` blob) whatever their affinity. In keys and index entries
a value of such a column is `0x02` for NULL (after every value, as in other columns) or `0x01`
followed by its class and its body, so the classes sort as SQLite orders them:
- `0x01`, a number: the nearest 64-bit float in the order-preserving form other `double
  precision` keys use (`-0.0` and every NaN written as `0.0` and the one canonical NaN), then a
  16-bit big-endian offset with its sign bit flipped, the integer's distance from that float
  (at most 1024, nonzero only for integers beyond 2^53; `0` for a real). Integers and reals
  therefore order by exact value, and an integer and a real that are equal (`3`, `3.0`) have the
  same key;
- `0x02`, text: its UTF-8 bytes escaped and ended as `text` keys are (`0x00` written `0x00
  0xFF`, then `0x00 0x01`);
- `0x03`, a blob: its bytes, escaped and ended the same way.

A key is read back only in its canonical form: an offset that does not round to its float, a
non-canonical zero or NaN, an unknown class or text that is not UTF-8 is damage.

A user-defined type is a record under the catalog prefix, `0x0A` and the type's schema-qualified
name: record version `1`, its kind (as above), its 32-bit OID (16384 to 2^31-1, picked from a hash
of its name and never reused while it exists), then an enum's labels (count; each its 64-bit sort
order, strictly ascending, and its label of 1 to 63 bytes), a composite type's attributes (count;
each its name and type, never the type itself), a range's subtype, or a domain's base type, a `NOT
NULL` flag byte, an optional default text (`0`, or `1` and the text), and its checks (count; each
its name, its expression text over `VALUE`, and a byte `1` when validated). Every statement that
names a type loads them once, and a damaged record is an error naming it, never a panic. A type
whose attribute, subtype or base type is `citext` or `hstore` is written with record version `2`
(D259), the same layout with kind `5` allowed in those member types; every other type keeps
version `1`, and a version that does not match its members is damage. Kind `5` names a column's
or member's type only, never a record of its own.

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

A table made by SQLite's `CREATE TABLE` keeps that statement's text as written in one record
under the catalog prefix, `0x15` and the table's internal name (D273): record version `1`, the
generated `CREATE TABLE` text's length as a 32-bit little-endian number, that text, then the
original text to the end of the value, both UTF-8. `sqlite_schema` shows the original text while
the table's generated text still equals the saved one, and the generated text otherwise;
`DROP TABLE` deletes the record, `ALTER TABLE ... ADD COLUMN` and `RENAME TO` save it again
edited, and any other `ALTER TABLE` deletes it. A SQLite view's text is a record of the same
layout under the view's internal name, and an index's under its table's internal name, the
byte `0xFF` (never part of UTF-8, so never part of a name), `i` and the index's name (D294);
`DROP VIEW` and `DROP INDEX` delete them. A file written before these records shows the
generated text. A damaged record is an error naming it, never a panic.

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
there are no materialized views, renaming a table
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
the copy (PostgreSQL skips the row), and the per-row notices of `LOG_VERBOSITY verbose` are not
sent when `REJECT_LIMIT` fails the copy (PostgreSQL sends them before the error); the binary format
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
checks the PostgreSQL manual's documented behaviour and marks these as differing until they
are fixed:
- An identity column must be a column of the primary key (at any position, D364), where
  PostgreSQL allows one outside it.
- A table in `FROM` takes no column alias list (`FROM t AS x (a, b)`, or `FROM t partition (p1)`,
  where `partition` is an alias and `p1` renames the first column): `0A000`.

The other differences:

- One statement at a time is planned by simple rules without statistics ([Indexes](#indexes)):
  a `WHERE` that fixes every primary-key column with `=` reads one row (in a join, of the first
  table), conditions on a leading part of the primary key or an index read that range, and
  anything else reads the whole table. Each statement's result
  is built in memory before it prints (a `SELECT` without `ORDER BY` holds only the rows it
  returns; `ORDER BY` holds every matching row and sorts them, unless a key gives the order or
  a `LIMIT` lets it keep only the first rows, D341).
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
  and 16,383), input with more fails with `22003`. `Infinity`, `-Infinity` and `inf` are
  `numeric` values, as in PostgreSQL 14: they sort between the finite values and `NaN`, follow
  PostgreSQL's arithmetic (`Infinity - Infinity` is `NaN`, `x / Infinity` is 0), cast to and from
  `double precision`, fail with `0A000` when cast to an integer and with `22003` in a
  `numeric(p,s)` column (D290). `scale(x)` is the number of digits a value shows after the point,
  `min_scale(x)` the fewest that still give it exactly, and `trim_scale(x)` the value with its
  trailing fractional zeros dropped (`8.4100` gives 4, 2 and `8.41`); of `NaN` or an infinity the
  first two are NULL and `trim_scale` returns it unchanged (D302). `^` with a non-integer exponent, or an integer one whose exact result
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
  `json_object(text[])`, `json_populate_record` in a select list and `JSON_TABLE`'s `PLAN`
  clause are missing, and Oracle's `IS JSON` (lax syntax, `STRICT`), `JSON_SERIALIZE` options and
  `JSON_SCALAR` are not parsed in Oracle's dialect yet. `IS JSON` on a document nested deeper
  than the JSON parser's 256 levels is false (PostgreSQL says true).
- `interval` (D113): every `IntervalStyle` is printed (`postgres`, `postgres_verbose`,
  `sql_standard`, `iso_8601`, D389), as values become text, so casts to text follow it too, as in
  PostgreSQL; any other value is `22023`. PostgreSQL 17's `infinity`
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
  PostgreSQL's immediate `RESTRICT` refuses it. `ADD COLUMN` of an identity column answers
  `0A000` (a `serial` one is built, D387).
- A sequence's `CACHE` is remembered, not used: every value is saved. A value handed out by a
  transaction that rolled back can be handed out once more if another connection's writer starts
  between the rollback and the re-saving of the sequence (D114); a committed value never is.
  `serial` and identity columns have no visible sequence (`pg_get_serial_sequence` waits for
  item 37); `nextval` takes a name, not an OID.
- `\d`, `sqlite_schema` (for a table changed after `CREATE TABLE` or made in another dialect)
  and `.schema` outside SQLite's dialect do not show `CHECK` and `FOREIGN KEY` constraints yet;
  MySQL's `SHOW CREATE TABLE` shows them.
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
  MATERIALIZED` change nothing; a data-modifying `WITH` query that resolves conflicts, and `EXPLAIN` of a query with
  derived tables, or with `WITH` or set operations inside a subquery, answer `0A000`. A derived table, a `WITH` query, a `LATERAL` item and the operands of
  a set operation are held in memory, counted with the statement's other rows against the 1 GiB
  limit, and a correlated subquery keeps up to 32 MiB of results by outer value. A `LATERAL` item
  runs once per distinct set of the values it names, like a correlated subquery, and its join
  back to them compares with `IS NOT DISTINCT FROM` (no hash join) when one of those values is
  NULL. `LATERAL ... USING` and a `LATERAL` item after a comma and a `RIGHT` or `FULL` join answer
  `0A000`; `FULL JOIN ... USING` columns cannot be named unqualified from a subquery (`42702`). An `IN` list of constants is compared item by item (a list of
  more than 256 items is not used to choose a key range either), so a very long list against
  many rows is slow; `IN (SELECT ...)` looks values up in a hash set.
- `percentile_cont` and `percentile_disc` over an array of fractions and `percentile_cont`
  between two `interval` values fail with `0A000`; Oracle's two-variable statistics return
  `double precision` where Oracle returns `NUMBER` for `NUMBER` arguments (D340). Window functions (D151) do not take `DISTINCT`
  or `ORDER BY` inside an aggregate's parentheses (`0A000`, as in PostgreSQL); a window whose
  frame does not start at `UNBOUNDED PRECEDING`, or that excludes rows, recomputes its aggregate
  for each row, so a long sliding frame costs its length per row; a correlated subquery cannot
  hold a window function in its select list (`0A000`).
- Arrays and set-returning functions (D128): arrays cannot be stored in a table or a view, have
  one dimension, and take the element types above; `x[1:2]` and `ARRAY[[1, 2]]` answer
  `0A000`. Set-returning functions run in `FROM` and as select-list
  items; several of them in one select list run in lock-step, each input row giving as many rows
  as the longest one, the others padded with NULL, as in PostgreSQL 10 and later (D131); one
  inside an expression is `0A000`, and a function in `FROM` cannot name an item after it in the
  same `FROM`. `COLLATE` takes only the `C`-like collations.
  When the output stops being read (`eggdb sql ... | head`), the command stops before the next
  statement.
- `UPDATE` and `DELETE` collect the rows they change before changing them, in memory.
- A whole transaction must fit in the page cache (`--cache`), as for every write (D86).
- Inside a savepoint, each statement keeps the previous value of every key it writes (plus 64
  bytes per entry) against its 1 GiB work memory, and a statement that passes that fails with
  `53200` and is undone (D317); PostgreSQL has no such limit. The savepoint's undo log keeps
  those entries until `RELEASE`, `ROLLBACK TO` or `COMMIT`, in memory.
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
  must reach 1.0 before v0.6.0 (item 19e, D100).

## Known differences from MySQL
A view is read-only, `SHOW CREATE VIEW` shows the query as it was written (MySQL rewrites it),
a view that used `SELECT *` keeps the columns it had when it was created (as MySQL's do), and
`CREATE DATABASE`, `DROP DATABASE` and `USE` of another database are not supported yet: a
server serves one database.

The MySQL suite (`crates/eggdb-sql/correctness/mysql.test`, 242 cases from the MySQL 8.0
Reference Manual's documented behaviour) marks these as differing until they are fixed:
- `7 / 2` is `3.5` (a `double`) where MySQL gives the `DECIMAL` `3.5000`, and decimal constants
  outside a `DECIMAL` column are doubles (`0.1 + 0.2` is `0.30000000000000004`); stored into a
  `DECIMAL` column they are exact.
- `1 / 0` is error 1365 where MySQL's `SELECT` gives `NULL` with a warning.
- Text compares byte by byte (`'a' = 'A'` is false) where `utf8mb4_0900_ai_ci` ignores case and
  accents; collations come with D30.
- A view made with `ALGORITHM=TEMPTABLE` still takes `INSERT`, `UPDATE` and `DELETE` when its
  query could; MySQL refuses them (error 1288).
- An `AUTO_INCREMENT` column must be a column of the primary key that leads the primary key or
  an index (`PRIMARY KEY (store_id, id), KEY (id)` works since D364); InnoDB needs only the
  index, not the primary key.
- `FULLTEXT` and `SPATIAL` keys, in `CREATE TABLE` and `ALTER TABLE ... ADD`, answer 1235: InnoDB
  makes a `FULLTEXT` index on a `CHAR`, `VARCHAR` or `TEXT` column, and a `SPATIAL` one on a
  column that is no geometry is MySQL's 1687.
- A `TIME` outside `-838:59:59` to `838:59:59` is an error (`22007`, error 1292) where MySQL's
  non-strict modes clip it with a warning, and `HOUR()`, `TIME_TO_SEC()` and the other `TIME` functions are
  missing.

And more, not in the suite yet:
- Comparisons and `IS` tests are booleans, shown as `1` and `0`, but arithmetic on them (`TRUE +
  1`) and `IF(1, ...)` with an integer condition are type errors where MySQL treats booleans as
  integers. Strings are not converted to numbers implicitly (`'5' + 1` with a typed string).
- `AUTO_INCREMENT` continues from the largest value in the table, so the value of a deleted last
  row is given again, which MySQL 8 does not do; `AUTO_INCREMENT=n` table options are ignored.
- `INSERT IGNORE` skips duplicate keys, and rows of a partitioned table outside the named
  partitions or with no partition (D360); other errors still fail the statement.
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
  fraction, the `YEAR` type is not supported yet; `SHOW CREATE TABLE` leaves out `ON UPDATE CURRENT_TIMESTAMP` (D416). `FROM_DAYS`, `STR_TO_DATE` and `MAKEDATE` give `NULL`
  where MySQL gives a zero date, `STR_TO_DATE` doesn't read the week specifiers, and
  `UNIX_TIMESTAMP` gives whole seconds (D415). The date-part
  functions give no warning 1292 for a string that is no date, and read a date with a zero
  month or day other than `'0000-00-00'` (`'2024-00-00'`) as NULL.
- A value rounded to fit a `DECIMAL(p,s)`'s scale gives no note 1265.
- `EXTRACT` gives integers and accepts no `WEEK` mode but 0; `EXTRACT` of a negative compound
  value and of years before 1000 are not checked against MySQL.
- `AVG` of integers and `SUM`/`AVG` of `DECIMAL` use PostgreSQL's result scale (`AVG(2, 3)` is
  `2.5000000000000000`) where MySQL adds 4 digits to the argument's scale (`2.5000`).
- Indexes: a key on a column prefix (`KEY k (name(10))`) indexes the whole value, and a
  `UNIQUE` one answers 1235. `SHOW INDEX` gives
  `Cardinality` and `Sub_part` as `NULL`, and `EXPLAIN` gives `key_len`, `ref` and `rows` as
  `NULL`. Foreign keys are
  checked when the statement ends, not row by row, and
  creating one makes no index on the child's columns (InnoDB makes one).

## Known differences from SQLite
Views are read-only unless an `INSTEAD OF` trigger makes them writable, as in SQLite,
`CREATE TEMP VIEW`, `CREATE TEMP TRIGGER` and the `temp` schema are `0A000`, and
`ALTER TABLE ... RENAME COLUMN` and `DROP COLUMN` (and a table rename's references in views and
triggers) leave `sqlite_schema` showing generated text instead of SQLite's edited text.

The SQLite suite (`crates/eggdb-sql/correctness/sqlite.test`, 263 cases from sqlite.org's
documentation of SQLite 3.46, never SQLite's source) marks these as differing until they are
fixed:
- Indexes: `EXPLAIN` without `QUERY PLAN` (the bytecode listing), partial indexes and indexes
  on expressions answer an error; an `ON CONFLICT (columns)` that names no unique key answers
  `42P10` with PostgreSQL's message; `sqlite_schema.sql` of an index is regenerated from the
  catalog.
- A column of `INTEGER` affinity has one type, `bigint`: an `INTEGER` column given `'abc'`
  keeps the text in SQLite, where EggDB answers `datatype mismatch`. Columns of the other
  affinities hold every storage class (D153, D305).
- Dates and times: `date()`, `time()`, `datetime()`, `julianday()`, `strftime()`,
  `unixepoch()` and `CURRENT_TIMESTAMP` are not supported yet; `DATE` and `DATETIME` columns
  keep their text, as SQLite's do.

And more, not in the suite yet:
- A real given to an `INTEGER` column is rounded where SQLite keeps it as a real, and
  `CAST('abc' AS INTEGER)` is an error where SQLite gives 0. `CAST(x AS NUMERIC)` gives a
  real where SQLite gives an integer for a whole number. An integer compared with a `TEXT`
  column is not turned into text first (SQLite compares `t = 5` as `t = '5'`), and a column of
  a view has no affinity of its own.
- Tables made before D305 keep the types they were made with: their `TEXT`, untyped and `DATE`
  columns stay `text`, their `BOOLEAN` columns `bigint`, and their `REAL`, `NUMERIC` and
  `DECIMAL` columns `double precision`.
- A `NUMERIC` or `REAL` column compared with a `TEXT` or untyped column does not apply its
  affinity to the other column's values (SQLite compares `n = t` with `t` turned into a number
  when it looks like one); only constants take the affinity.
- Names fold to lower case: result columns and `PRAGMA table_info` show `id` for a column
  declared `Id`. `PRAGMA table_info` shows each column's type as the `CREATE TABLE` text declared it (`DATETIME`,
  `DECIMAL(10, 2)`, empty for an untyped column), as SQLite does; a table whose kept text no
  longer matches its catalog (after a change other than `ADD COLUMN`), or that was not made in
  the SQLite dialect, shows the affinity's type name (`INTEGER`, `TEXT`, `REAL`).
- An `INTEGER PRIMARY KEY` given `0` takes the next row id where SQLite stores 0, and
  `AUTOINCREMENT` reuses the row id of a deleted last row (SQLite's never does).
  `last_insert_rowid()` is not changed by inserts into tables whose key is not an `INTEGER
  PRIMARY KEY`.
- The `OR` forms of `INSERT` cover the primary key only (there are no `UNIQUE` constraints
  yet), and an `ON CONFLICT FAIL` clause written in a table's constraints acts as `ABORT`.
- `PRAGMA user_version`, `foreign_keys`, `journal_mode` and `synchronous` are per session, not
  kept in the file; foreign keys are never enforced.
- Not supported yet: `CREATE INDEX`, `UNIQUE` and `CHECK` constraints, `COLLATE NOCASE`,
  views, `ATTACH`, `VACUUM INTO`, and bare columns in aggregate
  queries. `group_concat(X [, Y])` (with `DISTINCT` and `ORDER BY`) and `total(X)` are built
  (D376), and so are SQLite's math functions (`acos` ... `trunc`, `log(B, X)`, `pi()`, NULL for
  an argument that does not look like a number or is out of the function's domain) and `unhex`,
  `zeroblob` and `randomblob` (D377), and the JSON functions `json`, `json_valid`, `json_type`,
  `json_extract` (one path gives an SQL value, several a JSON array), `json_array`,
  `json_object`, `json_array_length` and `json_quote`, with SQLite's paths (`$.key`, `$."key"`,
  `$[N]`, `$[#-N]`) (D378), and `json_set`, `json_insert`, `json_replace` (`$[#]` appends,
  missing objects on the way are created), `json_remove` and `json_patch` (RFC 7396) (D379),
  and `X -> P` (JSON text) and `X ->> P` (SQL value), where `P` is a path, an object label or an
  array index (negative from the end) (D380), and the aggregates `json_group_array(X)` and
  `json_group_object(NAME, VALUE)` (`[]` and `{}` over no rows) (D381), and the table-valued
  `json_each(X [, P])` and `json_tree(X [, P])` with SQLite's columns `key`, `value`, `type`,
  `atom`, `id`, `parent`, `fullkey` and `path` (D382). `CREATE TEMP TRIGGER` defines a trigger,
  on a temporary or a main table, that only its session sees and that ends with it (D383).
  `VACUUM [schema]` and `REINDEX [name]` are accepted and change nothing: EggDB's file reuses
  freed pages, `eggdb compact` shrinks it, and its indexes are kept exact; `VACUUM INTO` is
  `0A000` (D384).

## Known differences from Oracle
The Oracle suite (`crates/eggdb-sql/correctness/oracle.test`, 773 cases from Oracle's SQL
Language Reference and PL/SQL Language Reference, never Oracle's software) marks these as differing until they are fixed:
- `EXPLAIN PLAN` and `DBMS_XPLAN.DISPLAY` give no costs, cardinalities or predicates and only
  the `Id`, `Operation` and `Name` columns (D312); bitmap, function-based and reverse-key
  indexes, and the primary key's index in `USER_INDEXES` (EggDB keeps the primary key as the table's own order).
- `ROWNUM` in the select list, `ORDER BY` or anywhere but `WHERE` conditions joined by `AND`.
- Division that does not end keeps `numeric`'s scale (about 20 digits; Oracle has 38), so `2 /
  3 * 3` is not exactly 2, `date - date` of 8 hours shows fewer 3s and `MONTHS_BETWEEN` fewer
  digits.
- An interval prints its leading field with at least two digits and its type's fractional
  digits (six by default): `TIMESTAMP - TIMESTAMP` and `NUMTODSINTERVAL` show `+01 12:00:00.000000`
  where Oracle shows their nine-digit precisions (`+000000001 12:00:00.000000000`). A column's
  declared leading precision (`DAY(5)`) is accepted but not kept, and fractional seconds keep
  six digits (D149).
- An identity column outside the primary key.

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
  table shows. Languages other than the seven above, `E`/`EE` (other calendars),
  `TO_DATE(... DEFAULT ... ON CONVERSION ERROR)`, `LISTAGG (DISTINCT ...) OVER` and `AT TIME ZONE` are not
  supported yet. Other string functions given a date (`SUBSTR(d, ...)`) write it by
  `NLS_DATE_FORMAT` whatever its type; a time stamp assigned to a character column takes the
  timestamp format only when it has a fractional second. `NEXTVAL` in a `MERGE`'s `VALUES` draws
  only for the rows inserted. An Oracle view read from another dialect converts text as that
  dialect does.
- `VARCHAR2(n BYTE)` counts characters, and `ORA-12899` gives the maximum but not the column's
  name or the actual length. A `CHAR` compared with a `VARCHAR2` value ignores the `CHAR`'s
  trailing blanks (PostgreSQL's rule) where Oracle pads the shorter value only between two
  `CHAR`s. A string given to a `RAW` or `BLOB` is read as hexadecimal digits, as `HEXTORAW`
  does (an odd count gets a leading zero, another character is `ORA-01465`). `HEXTORAW` and
  `RAWTOHEX` work; the packages `UTL_RAW` and `DBMS_LOB` are missing.
- `GENERATED ALWAYS` refuses a given value with `ORA-32795` and an `UPDATE` of the column with
  `ORA-32796` (D150), but an identity column reuses the number of a deleted last row (Oracle's
  identity is a sequence that never goes back).
- Result columns of expressions are named after the expression in upper case with spaces
  removed outside quotes, close to SQL*Plus's headings; `SELECT *` from `USER_TABLES` shows the
  twenty columns apps read (`TABLE_NAME`, `TABLESPACE_NAME`, `STATUS`, `NUM_ROWS`, ...), not
  Oracle's fifty, and the other dictionary views likewise show their commonly read columns.
- The data dictionary (D134): each `NOT NULL` column outside the primary key is listed in
  `USER_CONSTRAINTS` and `USER_CONS_COLUMNS` as a type `C` constraint with Oracle's
  `"COLUMN" IS NOT NULL` condition under a generated `SYS_C` name (made from the table's and
  column's numbers), even when the DDL named it, since EggDB does not keep that name; `CREATED` and `LAST_DDL_TIME` are 2000-01-01 and the statistics (`NUM_ROWS`, `BLOCKS`, ...)
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
- `SEQUENCE.NEXTVAL` in a multitable insert's `VALUES` advances once per use, where Oracle
  advances it once per source row; rows go into an `INSERT ALL`'s targets one `INTO` clause at a
  time, so identity values follow that order (D304).
- Not supported yet: outer joins with `(+)`, `RETURNING ... INTO`, synonyms, database links,
  `ROWID` and the XML functions.
- Regular expressions (D332): without `n`, a bracket expression that excludes characters
  (`[^a]`) does not match a newline either, where Oracle's does; `x` also treats `#` as the
  start of a comment; `\Z` is the end of the text only, and `\z` and the equivalence and
  collation classes (`[=e=]`, `[.ch.]`) are not read. The analytic functions take no `DISTINCT`,
  `LISTAGG (DISTINCT ...) OVER` included, and `KEEP` is not available on `LISTAGG` or with
  `DISTINCT`.
  `SAVEPOINT` works as in every dialect (D173), and views `WITH CHECK OPTION` refuse rows they
  would not show ([Schemas, views and comments](#schemas-views-and-comments)).
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
- `value AT TIME ZONE 'Pacific Standard Time'` reads the 142 Windows time zone names SQL Server
  lists in `sys.time_zone_info` (in any letter case), each following the IANA region the Unicode
  CLDR mapping gives it; a `datetime2`, `datetime` or `smalldatetime` is read as a local time
  there (a skipped time with the offset before the change, a repeated one as the earlier
  instant, as SQL Server does) and a `datetimeoffset` keeps its instant. The result is a
  `datetimeoffset`, shown in UTC like every other. An unknown name is error 9820, and a `date`
  or string argument error 8116 (D290).
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
- Not yet: `deleted.` in `UPDATE`, global `##temporary` tables (`##name` is the session's own
  table, like `#name`), and `OUTPUT ... INTO` refusing a target with triggers, checks or foreign
  keys, as SQL Server does (EggDB fires and checks them). A `MERGE`'s deletes do not fire `DELETE`
  triggers, and `SELECT ... INTO` does not keep an identity column's identity property: the new
  column is a plain `NOT NULL` one, so an `INSERT` without a column list takes a value for it where
  SQL Server answers error 8101, and one that leaves it out is error 515 (D342).
- An identity column outside the primary key. The next identity number
  follows the largest one in the table (the smallest for a negative increment), so a deleted
  largest value is used again and an emptied table starts again at its seed, where SQL Server
  keeps a counter.
- An unknown column of an existing table is found when the statement runs, after
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
- Batch logic (D137): DDL
  triggers, `INSERT ... EXEC` of dynamic SQL, `SET eggdb.dialect` inside a block or dynamic SQL, and a `DECLARE` whose value
  reads a variable the same `DECLARE` declares are 40517. A `SELECT` whose value reads another
  variable the same `SELECT` assigns is 40517. Every Transact-SQL cursor is a static snapshot
  taken at `OPEN` (`DYNAMIC` and `KEYSET` cursors do not see later changes). A table variable's columns take `NOT NULL`, `PRIMARY KEY` and
  `UNIQUE` (on a column or over several), checked on every `INSERT` and `UPDATE` (515, 2627), but
  no defaults, checks or identity. `EXEC` of anything but the catalog procedures, `sp_executesql`
  and `sp_unprepare`; rolling back to a save point (`SAVE TRANSACTION` is recorded);
  `QUOTED_IDENTIFIER OFF` and `ANSI_PADDING OFF`; three-part names and other databases.
- Errors inside dynamic SQL report the line of the `EXEC` statement, not their line in
  the dynamic text. `RAISERROR ... WITH LOG` writes no log.
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
- SqlClient's `GetSchema("Databases")` is answered (D291): its `IF OBJECT_ID(...) ... EXEC
  sp_executesql` batch reads the compatibility view `sysdatabases` (`sys.sysdatabases`, also as
  `dbo.sysdatabases` and `master..sysdatabases`: `name`, `dbid`, `sid`, `mode`, `status`,
  `status2`, `crdate`, `reserved`, `category`, `cmptlevel`, `filename`, `version`).
  mssql-jdbc's `getSchemas()` (it names `sys.schemas.name` with three parts) is not answered
  yet.
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
- `JSON_TABLE`'s `PLAN` clause, and Oracle's `IS JSON`, `JSON_SERIALIZE` and `JSON_SCALAR`.
- Point `SELECT`s as fast as SQLite's everywhere: an AST that borrows the SQL text and a plan
  cache; table statistics for choosing between key lookups and hash joins.
- `DISTINCT ON` over any expression in a grouped query and beside window functions; `LATERAL ... NATURAL JOIN`.
- Then the other dialects (item 23), each with its own correctness suite; for the PostgreSQL protocol,
  TLS, `information_schema` and the catalog queries of GUI tools beyond psql's `\d` family,
  and `LISTEN`/`NOTIFY` (M6a).
