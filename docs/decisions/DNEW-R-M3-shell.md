## DNEW-R-M3-shell Key-value commands inside a SQL block in `eggdb shell` join the block's write transaction through `eggdb::sql::SharedWriteStore` — Decided by the build loop (2026-11-02); roadmap item 20 (M3), applies D2, D3, D86, D105

`eggdb shell` mixes key-value commands and SQL at one prompt (D105), but a `BEGIN` block held the
database's one write transaction (D86) inside the SQL session, so a `put` typed in the block
asked for a second writer and failed busy. The engine's promise (D2) is one transaction system
for every model, and `WriteTransaction::sql` already runs SQL inside a key-value transaction; the
shell needed the other direction, key-value work inside a SQL block that spans prompts.

**The store keeps the transaction, the session borrows it.** `eggdb::sql::SharedWriteStore` is a
store for SQL sessions over a `Database` whose write transaction lives in the store (a
`RefCell<Option<WriteTransaction>>`), not in the session's writer. The writer it hands the session
(`SharedWriter`) reaches the transaction on each read and write and takes it out to commit or roll
back; dropping a writer that did neither rolls back. Between statements,
`SharedWriteStore::with_open_transaction` runs code on the same transaction, so a block's
`COMMIT` makes the key-value writes and the SQL changes durable together and its `ROLLBACK` drops
both. A read or write that finds the transaction in use answers the busy error instead of
panicking. Outside a block every statement still begins and commits its own transaction, as with
`eggdb::sql::Store`.

**The shell runs the key-value writes of a block itself.** Inside a block, `put` (with `-C`,
`--create` and `--ttl`), `get`, `del`, `incr` and `decr` run on the block's transaction, so `get`
sees the block's writes; in a failed block they are refused until `ROLLBACK` (the transaction is
already rolled back), and in a `BEGIN READ ONLY` block writes are refused while `get` reads the
last commit. `expire`, `persist` and `cas` are refused inside a block for now, and `scan`, `ttl`,
`check`, `stats` and `backup` read the last commit, as they did. The alternative, routing every
key-value command of the CLI through an optional open transaction, was left for later: it changes
every command's code for a case only the shell has.
