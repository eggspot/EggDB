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
at the end.

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
- **Several connections**, one thread and one session each, up to `--max-connections` (64);
  readers run side by side, writers one at a time (a second waits 5 s, then `55P03`).

Not yet (each fails readably or is ignored harmlessly): TLS, psql's `\d` commands and the
catalog queries of GUI tools (`pg_catalog`), `COPY`, `LISTEN`/`NOTIFY`, cancelling a running
query, the implicit transaction around a multi-statement `Query` or an extended-protocol
pipeline (each statement commits on its own outside `BEGIN`), stored users and roles, and
non-ASCII passwords.

## What runs today

**Statements**
- `CREATE TABLE [IF NOT EXISTS] name (column type [NOT NULL | NULL | PRIMARY KEY | DEFAULT expr] ..., [PRIMARY KEY (columns)])`.
  A table without a primary key keeps its rows in insertion order under a hidden row number.
  `CONSTRAINT name` before a constraint is accepted. At most 1,600 columns, as in PostgreSQL.
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
`RETURNING`, `DEFAULT`, `LIMIT`), `42809` `sum(*)` and `count()`, `42P07` a table that exists, `23505` a duplicate primary key (with
`Key (id)=(1) already exists.`), `23502` a NULL in a `NOT NULL` column, `22P02` text that is
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

## How SQL is stored

SQL lives in the database's one ordered key space, under keys that begin with the five bytes
`0xff 'S' 'Q' 'L' 0x00` (D98). Below that prefix:

| Key | Value |
|-----|-------|
| prefix, `0x01` | the catalog's format version and the next table number |
| prefix, `0x02`, table name | the table's definition: its number, columns, types, `NOT NULL`, defaults and primary key |
| prefix, `0x03`, table number (4 bytes, big-endian), primary key | the row |

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

## Limits of this part
- One statement at a time is planned by simple rules: a `WHERE` that fixes every primary-key
  column with `=` reads one row (in a join, of the first table); anything else reads the whole
  table. Each statement's result
  is built in memory before it prints (a `SELECT` without `ORDER BY` holds only the rows it
  returns; `ORDER BY` holds every matching row and sorts them, with no top-N for `LIMIT` yet).
- Joins read the first table row by row and every later table once. A table whose whole
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

## Next parts (roadmap)
- Secondary indexes (`CREATE INDEX`, `UNIQUE`), used by `WHERE` and `ORDER BY`.
- The `numeric`, `timestamp` and `json`/`jsonb` types, and `serial`/identity columns.
- Point `SELECT`s as fast as SQLite's everywhere: an AST that borrows the SQL text and a plan
  cache; table statistics for choosing between key lookups and hash joins.
- `RIGHT` and `FULL` joins, subqueries, and the other aggregates.
- Then `eggdb shell` and the other dialects (items 20, 22 to 25); for the PostgreSQL protocol,
  TLS, the `pg_catalog` queries of psql's `\d` and GUI tools, `COPY` and query cancellation
  (M6a).
