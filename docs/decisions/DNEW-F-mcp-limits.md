## DNEW-F-mcp-limits Writers wait for a busy file, and an agent's calls are bounded inside the engine: a session deadline, a result row cap and value checks in every growing builder — Decided by the build loop (by judgement, 2026-11-02); follows up D119, D121 and D156

D119 and D156 left three follow-ups for `eggdb mcp`: the user's writers failed at once while an
agent's call held the file, the time limit and `max_rows` were enforced around the engine (the
call store's clock checks, the watchdog, cutting the built result), and some value builders
did not check the value limit.

**Writers that wait for the lock.** `Options::busy_timeout(Duration)` makes `Database::open`
try again while another program has the file (`ErrorKind::Locked`), at intervals growing from
1 ms to 50 ms, until the file is free or the timeout has passed, as SQLite's busy timeout does.
The library default stays `Duration::ZERO` (fail at once), so the documented behaviour of the
Rust API and of existing embedders does not change; the clock is read only after a first
refused open, so the default path never calls `Instant::now` (WebAssembly has no clock). Every
`eggdb` command waits five seconds by default, the same as `Options::write_timeout`'s wait for
another writer and `eggdb mcp`'s default time limit, so a command started during an agent's
call finishes after the call instead of exiting 5; `--busy-timeout <ms>` (0 to 3,600,000) on
every command changes it, and `--busy-timeout 0` gives the old behaviour. The MCP server's own
opens keep D119's one-second retry and `database_busy`.

**A session deadline.** `Session::set_deadline(Option<Instant>)` stops every statement still
running at that instant with SQLSTATE `57014`, combined with `statement_timeout` (the earlier
wins) in the same per-statement deadline the engine already checks. The join loop now calls the
cancel check for every row combination it tries, so a cross join of in-memory rows, which reads
nothing from the store, stops at the deadline (or at a `CancelRequest`) instead of only through
the watchdog's exit. `eggdb mcp` sets the call's deadline on its session (lifted before the
`COMMIT` of `write_sql`, which D121 never interrupts), and a `57014` after the deadline is
reported as `time_limit`. The watchdog stays as the backstop for work inside one function call.

**A result row cap.** `Session::set_max_result_rows(Option<u64>)` lowers the `LIMIT` of a
top-level `SELECT` or query to the cap when it has none, `LIMIT NULL`/`ALL`, or a constant
larger one, so the executor's streaming and top-N paths stop at the cap without building the
rest. It is an AST rewrite in `result_row_cap.rs` rather than a check in the executor, because
the executor's select path also runs subqueries, CTEs and derived tables, which must not be
cut. Never cut: statements that write (`INSERT`/`UPDATE`/`DELETE ... RETURNING`), a `SELECT`
that may call a writing function (`routines::select_may_write`), statements using sequences,
and Oracle's `WITH TIES`/`PERCENT` forms. `run_sql` and `write_sql` set `max_rows + 1`, so the
cut is still detected; `row_count` then counts at most `max_rows + 1` rows, not every row of
the result.

**Value checks in growing builders.** `translate`, `upper`, `lower` and `initcap` can make text
longer than their input (multi-byte replacements, case mappings that grow), so they check the
value limit first, with a cheap bound and an exact count only when the bound is over the limit
(`memory::check_grown_value_bytes`). `array_agg` checks its array's size as it grows, not only
when it finishes. The other builders D119 listed already checked.

**Not done here.** The `2026-07-28` MCP protocol era (no `initialize`, per-request `_meta`,
`server/discover`) is still a follow-up: it needs that revision's text checked, which this
change did not do. Set-returning functions in `FROM` still build their rows before an outer
`LIMIT` applies (bounded by work memory and the deadline).
