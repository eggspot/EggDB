# Placement rules and data residency

`CREATE EXTENSION eggdb_placement` adds rules for where a table and its rows may live (D1105).

```sql
CREATE EXTENSION eggdb_placement;
-- table, tier (hot, warm, cold, any), on edge copies?, allowed regions, home-region column
SELECT eggdb_placement.place('public', 'prices',    'hot',  true,  NULL,    NULL);
SELECT eggdb_placement.place('public', 'customers', 'warm', true,  NULL,    'home_region');
SELECT eggdb_placement.place('public', 'audit',     'cold', false, 'eu,us', NULL);
-- rows whose home region is 'ch' may be stored in ch and eu; others only in their own region
INSERT INTO eggdb_placement.residency VALUES ('ch', 'ch,eu');
```

- **Tiers.** `hot` and `warm` tables are read through once a minute by every server, so their
  pages stay in the page cache and, for a page store, in its local chunk files (D1103), while
  `cold` tables are left to fall back to object storage.
- **Edges.** An edge server (`eggdb serve --edge-of`) serves a copy without the tables marked
  `false` for edges.
- **Regions.** A server started with `--region eu` refuses to store a table whose allowed regions
  leave `eu` out, and a row whose home region's residency leaves `eu` out. A row from another
  server (`--multi-primary`) that it may not hold is dropped instead, and the refusal is logged in
  `eggdb_placement.transfers`, the record of cross-border transfers refused. An edge server with
  `--region` serves a copy without those tables and rows.

A server without `--region` holds everything, as before. Residency is enforced where rows are
stored. Which law requires which residency is the deploying organization's call (D53).
