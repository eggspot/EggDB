DROP TABLE IF EXISTS psql_copies;
DROP TABLE IF EXISTS psql_copies_back;
CREATE TABLE psql_copies (id integer PRIMARY KEY, name text, amount numeric(8,2), day date, at timestamptz, tags integer[]);
CREATE TABLE psql_copies_back (id integer PRIMARY KEY, name text, amount numeric(8,2), day date, at timestamptz, tags integer[]);
INSERT INTO psql_copies VALUES
  (1, 'a', 1.50, '2026-10-01', '2026-10-01 08:30:00+00', ARRAY[1, 2]),
  (2, NULL, -3.25, NULL, NULL, NULL),
  (3, E'tab\tand \\ slash é', 0, '1999-12-31', '2000-01-01 00:00:00.000001+00', '{}');
\copy (SELECT * FROM psql_copies ORDER BY id) TO PROGRAM 'od -An -tx1 -v' WITH (FORMAT binary)
\copy psql_copies TO '/tmp/eggdb-psql-copies.bin' WITH (FORMAT binary)
\copy psql_copies_back FROM '/tmp/eggdb-psql-copies.bin' WITH (FORMAT binary)
SELECT * FROM psql_copies_back ORDER BY id;
SELECT count(*) FROM psql_copies a JOIN psql_copies_back b ON a.id = b.id
  WHERE a.name IS NOT DISTINCT FROM b.name AND a.amount IS NOT DISTINCT FROM b.amount
    AND a.day IS NOT DISTINCT FROM b.day AND a.at IS NOT DISTINCT FROM b.at
    AND a.tags IS NOT DISTINCT FROM b.tags;
DROP TABLE psql_copies;
DROP TABLE psql_copies_back;
