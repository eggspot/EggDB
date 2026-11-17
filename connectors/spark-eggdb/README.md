# Spark data source for EggDB

`spark.read.format("eggdb")` reads and `df.write.format("eggdb")` writes EggDB tables from
Apache Spark, over EggDB's PostgreSQL protocol, with Spark's JDBC source doing partitioned
reads, predicate pushdown and batched writes.

```scala
val orders = spark.read.format("eggdb")
  .option("host", "eggdb.internal").option("database", "shop")
  .option("user", "analyst").option("password", sys.env("EGGDB_PASSWORD"))
  .option("dbtable", "public.orders")
  .option("partitionColumn", "id").option("lowerBound", "1").option("upperBound", "1000000")
  .option("numPartitions", "8")
  .load()
```

Options: `host`, `port` (5432), `database` (eggdb), `sslmode` (prefer), `dialect`
(`postgresql`, `mysql`, `sqlserver`, `oracle` or `sqlite` for `query` text), or a whole `url`;
every other option is Spark's JDBC source's. Build with `sbt package` and add the jar and the
PostgreSQL JDBC driver to `spark.jars`.

Licensed under MIT OR Apache-2.0, like EggDB.
