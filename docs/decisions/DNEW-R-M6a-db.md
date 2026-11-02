## DNEW-R-M6a-db CREATE DATABASE and DROP DATABASE run through the server's databases, one file each — Decided by the build loop (by judgement, 2026-11-02); refines D184

D184 left creating and dropping databases while serving. The SQL layer now parses them in
PostgreSQL's, MySQL's (`CREATE`/`DROP DATABASE` and `SCHEMA`, which are the same thing there) and
SQL Server's dialects and runs them through `ServedDatabases`, a trait the server implements.

- **A database is a file.** `CREATE DATABASE shop` makes `shop.eggdb` in the served directory;
  `DROP DATABASE shop` closes and removes it. A name must be a safe file name on every
  platform: 1 to 63 letters, digits, `_`, `-` or `$`, not starting with `-` and not a Windows
  device name; anything else is `42602`. PostgreSQL's `WITH` options (owner, encoding, template,
  locale), MySQL's character set and collation and SQL Server's file options are accepted and
  ignored, since one engine stores every database the same way.
- **Who may.** A superuser (the server's own user, or a stored role with `SUPERUSER`) creates
  and drops; a role with `CREATEDB` (now stored in the role's flags, and shown in
  `pg_authid.rolcreatedb`) may create. Without ownership of databases, only a superuser drops.
- **In use.** `DROP DATABASE` of the database the session is connected to, or of one any other
  session is connected to, is `55006` (SQL Server's error 3702); PostgreSQL's `WITH (FORCE)`
  is accepted but does not end other sessions. Not inside a transaction block (`25001`), except
  in MySQL's dialect, where it commits the open transaction as other DDL does.
- **Outside a server** (embedded, `eggdb sql`) both are `0A000` with a hint, as before.
- **Listing.** A session with the server's databases refreshes `pg_database` (and every catalog
  reading the database names) at each statement, so a database another session created shows
  in `\l` at once.
