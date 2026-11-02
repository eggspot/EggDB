## DNEW-R-44 Oracle's dialect over other protocols: `pg_catalog.` statements run as PostgreSQL, `@name` TDS parameters bind in Oracle SQL, NOT NULL listed with generated names (2026-11-02); refines D134

D134 left three known differences for apps that read Oracle SQL through PostgreSQL and TDS
drivers: psql's backslash commands and pgjdbc's `DatabaseMetaData` failed in Oracle's dialect,
`sp_executesql` parameters were read only in Transact-SQL, and `USER_CONSTRAINTS` left out the
`NOT NULL` constraints.

- **A statement that names `pg_catalog.` runs in the connection's dialect.** On a PostgreSQL
  connection that chose another dialect, a simple query, Parse, Describe or Execute whose text
  contains `pg_catalog.` (any case) runs in PostgreSQL's dialect. psql's and pgjdbc's catalog
  queries always qualify their relations, and Oracle SQL never names that schema, so the rule
  needs no list of client queries. The text is checked whole, so a `SET eggdb.dialect` in the
  same query string as such a statement is undone after it (known difference).
- **`@name` is a TDS parameter in Oracle SQL.** Oracle's lexer reads `@` followed by a letter
  or `_` as Transact-SQL's variable token, and Oracle's parser binds it by name from the
  parameters `sp_executesql`, `sp_prepare` and `sp_prepexec` pass, as Transact-SQL does. An
  `@name` without a parameter is the same error as in Transact-SQL. Database links (`t@link`)
  are not supported, so the token takes nothing Oracle SQL used.
- **RPC calls of stored procedures run as Transact-SQL.** The call is turned into Transact-SQL's
  `EXEC`, so it runs in the connection's dialect, like the transaction manager requests (D134),
  with parameters by position or by name in any order and defaults for the omitted ones.
- **`NOT NULL` constraints get generated names.** EggDB keeps only whether a column is
  `NOT NULL`, not a constraint name, so each `NOT NULL` column outside the primary key is listed
  as a type `C` constraint named `SYS_C` followed by the table's id times 10,000 plus the
  column's position (at least seven digits), with `"COLUMN" IS NOT NULL` as its condition. A
  name given in the DDL (`CONSTRAINT emp_last_name_nn NOT NULL`) is not shown (known
  difference); keeping it would change the stored table definition (D20).
