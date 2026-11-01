## DNEW-I80 An agent's statement that can change or delete many rows needs confirmation: only a `WHERE` pinning a whole primary or unique key to values counts as few rows, and upserts and `MERGE` always ask — Decided by the build loop (issue #80, security review of 2026-10-29); supersedes D121's rule for `DELETE`, `UPDATE` and `INSERT` and D126's rule for `MERGE` in the MCP classifier, applies D9, D11 and D45

D121 counted a `DELETE` or `UPDATE` as harmless when its `WHERE` named any column of its table,
and every form of `INSERT` as harmless. The security review found that `DELETE FROM t WHERE a =
a`, `... WHERE a IS NOT NULL`, `... WHERE a >= 0 OR a < 0` and `UPDATE t SET b = NULL WHERE a =
a` emptied or rewrote a whole table with no confirmation, and that MySQL's `REPLACE INTO`,
SQLite's `INSERT OR REPLACE` and `ON DUPLICATE KEY UPDATE` overwrite rows unclassified. D126 let a
`MERGE` through when its `ON` equated a target column with the source, which still updates every
row the source matches.

**The rule.** A `DELETE` or `UPDATE` is destructive unless all of these hold:

- the table has a primary key or a unique index, and every column of one of them is pinned by a
  part of the `WHERE`'s top-level `AND`: `column = value` (either side), or `column IN (values)`;
  the product of the pinned values' counts over the key's columns is at most 100. Other `AND`
  parts are free, since an `AND` can only narrow the rows; an `OR`, a `NOT`, a range, a subquery
  or a function in the pinning part does not pin;
- a value is a literal, `NULL`, a folded constant, a sign before a value, or a `CAST` of a value.
  A column of a character type is pinned only by a string (or a cast to a character type), since
  MySQL, SQL Server and Oracle compare a string column with a number as a number and match
  `'1'`, `'01'` and `' 1'` together;
- column names match the table's exactly, or by letter case when only one column matches;
- the change reaches no other rows: no foreign key of another table points at it with `ON
  DELETE` (for a `DELETE`) or `ON UPDATE` on an assigned column (for an `UPDATE`) set to
  `CASCADE`, `SET NULL` or `SET DEFAULT`, and no enabled trigger fires on it for that statement
  (D121: a statement that can run other statements is destructive).

A `WHERE` that names no column of the target (or none at all) keeps D121's kinds, shown as a
`DELETE` or `UPDATE` of the whole table; the rest are `DELETE that can remove many rows` and
`UPDATE that can change many rows`. `INSERT` with `REPLACE` (MySQL and SQLite), `INSERT OR
REPLACE`, `ON DUPLICATE KEY UPDATE` or `ON CONFLICT ... DO UPDATE` is always destructive (`INSERT
that overwrites existing rows`), whatever its source, since the rows it overwrites are chosen by
the data, not by the statement. `ON CONFLICT DO NOTHING`, `INSERT IGNORE`, `INSERT OR IGNORE` and
plain `INSERT` (also from a `SELECT`) add rows and change none, so they stay writes. A `MERGE`
that updates or deletes is always destructive; one that only inserts is a write.

**Where the keys come from.** The classifier needs the table's keys, foreign keys and triggers,
so `Session::destructive_statement` and `run_one_statement_if_confirmed` read them through the
session's table cache. `eggdb_sql::destructive_statement`, which has no database, knows no keys
and so counts every `DELETE` and `UPDATE` that names a column as one that can change many rows.
`eggdb mcp` classifies without the file first; only when that says destructive does it open the
file read-only, classify again with the keys and close it, before it asks the user, so nothing
is open while the user is asked (D121). When that read fails (the file busy, a close failing)
the answer without keys stands, which asks. The run re-classifies with the keys as before, and
refuses a destructive statement that is not exactly the one confirmed.

**What it does not change.** `confirm: true` remains the agent's word, not the user's (D121): it
guards against accidents, not against an agent that means harm or was misled. docs/mcp.md, the
README and the v0.2.0 notes now say that without a client that asks the user, `--allow-write`
lets the agent change anything, and that with `--allow-write` there is no per-table grant
(`--tables` serves read-only servers only).
