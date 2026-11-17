# Airbyte connectors for EggDB

An Airbyte **source** and **destination** for EggDB, speaking the Airbyte protocol (`spec`,
`check`, `discover`, `read`, `write`) over EggDB's PostgreSQL protocol.

- The source lists a schema's tables as streams with their columns' JSON types and primary keys,
  and reads them in full or incrementally by a cursor column, a server-side cursor at a time.
- The destination writes each stream into `_airbyte_raw_<stream>` (the record as `jsonb` and its
  time), appending or overwriting, and commits at each state message.

```sh
docker build -t eggspot/source-eggdb --build-arg CONNECTOR=source .
docker build -t eggspot/destination-eggdb --build-arg CONNECTOR=destination .
```

Add them to Airbyte as custom connectors (Settings → Sources/Destinations → New connector).

Licensed under MIT OR Apache-2.0, like EggDB.
