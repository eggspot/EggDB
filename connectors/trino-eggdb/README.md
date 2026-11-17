# Trino connector for EggDB

A Trino plugin that adds the `eggdb` connector, on Trino's PostgreSQL connector (EggDB speaks the
PostgreSQL protocol), so catalogs read and write EggDB with Trino's pushdown of filters,
projections, aggregates, joins and limits.

`etc/catalog/shop.properties`:

```properties
connector.name=eggdb
connection-url=jdbc:postgresql://eggdb.internal:5432/shop
connection-user=analyst
connection-password=${ENV:EGGDB_PASSWORD}
```

Build with `mvn package` and copy the jar with its dependencies into `plugin/eggdb` on every
Trino node.

Licensed under MIT OR Apache-2.0, like EggDB.
