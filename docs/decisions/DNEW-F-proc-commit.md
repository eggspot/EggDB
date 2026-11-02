## DNEW-F-proc-commit PostgreSQL procedures end transactions through a non-atomic writer, and cursors are session portals (2026-11-02); extends D158

D158 left `COMMIT`/`ROLLBACK` inside procedures and refcursors returned to the caller as "not
yet". Both need state that outlives one routine call: a transaction that ends and restarts while
the procedure keeps running, and cursors that stay open after the routine that opened them.

- **Non-atomic calls.** PostgreSQL lets a procedure end its transaction only when the `CALL` (or
  `DO`) is top-level outside a transaction block, or is `CALL`ed directly by such a procedure,
  outside any block with an exception handler. The session runs exactly those calls through a
  writer that owns the store's write transaction and can commit or roll it back and begin the
  next one (`TransactionControl`), with the same steps the session takes at commit: deferred
  constraint checks, sequences, temporary objects, `NOTIFY` delivery. Every other call (functions,
  triggers, `EXECUTE`, calls inside `BEGIN ... COMMIT` or under an exception handler) gets the
  ordinary atomic writer, so `COMMIT` there fails with `2D000` as in PostgreSQL. The writer stays
  out of the public `KeyValueWrite` trait: transaction control is a session concept, not a
  storage one.
- **Cursors are portals of the session.** A cursor a PL/pgSQL routine opens is handed to the
  session when the routine returns and stays open until its transaction ends (unless `WITH HOLD`),
  so the caller and other routines `FETCH` from it by name, which is how Npgsql and psycopg read
  a returned `refcursor`. SQL `DECLARE`, `FETCH`, `MOVE` and `CLOSE` act on the same portals.
  Unnamed cursors are `<unnamed portal N>` with a per-session counter.
- **Rows are read when the cursor opens.** Cursors were already materialized; this keeps them so,
  which also makes cursors in `FOR` loops survive a `COMMIT`, the effect PostgreSQL gets by making
  them holdable. A session keeps at most 1000 open cursors (D45).
- **`refcursor` is text.** The type name is accepted and holds the portal's name as text, rather
  than adding a type of its own; a PL/pgSQL `text` variable or parameter can name a cursor.

Not yet: closing explicitly opened cursors at a `COMMIT` inside a procedure (PostgreSQL closes
non-holdable ones; EggDB keeps them), `SET LOCAL` values and advisory locks across such a commit,
and a `refcursor` type distinct from `text` in `pg_typeof` and the wire protocol.
