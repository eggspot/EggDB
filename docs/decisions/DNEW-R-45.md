## DNEW-R-45 Npgsql's reset of a connection holding prepared statements, array parameters as array text cast to their declared type, and `void` as a catalog type — Decided by the build loop (2026-11-02); roadmap item 45, completes D135, applies D1, D9, D104, D128

D135 left three gaps in running .NET apps through Npgsql: the reset Npgsql sends for a pooled
connection that holds prepared statements, array parameters, and PostgreSQL's `void` type. Each
is written from PostgreSQL's public documentation (`SET SESSION AUTHORIZATION`, "Binary
transfer" of arrays in the protocol chapter, the pseudo-types table of the data types chapter).

**The reset runs as one `Query` message.** Npgsql sends `SET SESSION AUTHORIZATION DEFAULT;
RESET ALL; CLOSE ALL; UNLISTEN *; SELECT pg_advisory_unlock_all(); DISCARD SEQUENCES; DISCARD
TEMP` instead of `DISCARD ALL` when the connection holds prepared statements, so they survive.
`RESET ALL`, `CLOSE ALL`, `UNLISTEN *`, `pg_advisory_unlock_all()` and the `DISCARD` forms
already ran; `SET SESSION AUTHORIZATION DEFAULT` and `RESET SESSION AUTHORIZATION` are now
accepted and change nothing, since a session's authorization never leaves its login user, and
`SET SESSION AUTHORIZATION name` is accepted for the session's own user and refused with
`0A000` for any other, as `SET ROLE` is.

**An array parameter becomes the text of the array, cast to its declared type.** Parameters
reach the SQL layer as text with a declared type (D104). A parameter declared with one of
PostgreSQL's array OIDs (`int4[]` 1007, `text[]` 1009 and every other array a table column can
hold, `ArrayElement::of_array_oid`) is declared as that array type; in the text format its text
passes unchanged, and in the binary format its elements are decoded by the same reader `COPY
... BINARY` uses (element OID checked, `42804` on a mismatch, `22P03` on malformed bytes, the
2^27 element limit) and written back as PostgreSQL's array text. The cast then reads it like a
literal, so a malformed element answers `22P02`. Arrays of catalog types (`oid[]`, `name[]`,
`int2vector`, ...) stay refused with `0A000`.

**`void` is a catalog type.** `CatalogType::Void` (OID 2278, a pseudo-type row in `pg_type`) is
the result type of `pg_sleep`, `pg_notify`, the waiting advisory lock functions,
`pg_advisory_unlock_all()` and user functions declared `RETURNS void`. Its value is the empty
text, sent as zero bytes in both formats, and `''::void` reads any text as it. A user function
returning `void` still yields NULL as before; only its type changed. The alternative, a new
`SqlType` variant, was rejected: every exhaustive match over the engine's types would change for a
type no column can hold.
