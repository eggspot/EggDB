# EggDB

One database for every kind of data, written from scratch in Rust.

- **All in one:** caching, relational SQL, full-text search, documents, time series, analytics,
  graphs, vectors, geospatial, wide-column and objects, in one engine with one transaction system.
- **Speaks what you already use:** PostgreSQL, MySQL, SQLite, Redis, MongoDB, Elasticsearch and
  more, so applications switch without code changes.
- **Messaging and queues built in:** RabbitMQ (AMQP), MQTT, NATS, SQS and Kafka, in the same
  transactions as your data.
- **Every query language:** PostgreSQL, SQL Server (T-SQL), MySQL, Oracle and SQLite SQL, each as
  itself, plus Redis, MongoDB, Elasticsearch, CQL and graph queries, all on the same data.
- **Works with your tools:** drivers, ORMs, GUI clients and BI tools made for other databases.
- **Migrating is restoring a backup:** point EggDB at your SQL dumps, SQLite files or Redis and
  MongoDB backups, or let it follow your running database and switch over.
- **About a second to download and a second to install:** one small file, no dependencies.
- **A client people love:** a desktop, browser and command-line client designed for EggDB, with
  familiar layouts for people coming from other database clients.
- **Drivers for every language:** C#, Java, Node.js, Python, Go, Rust, PHP, Ruby and more, working
  with each language's ORMs.
- **BI and pipelines built in:** our own dashboards, reports and ETL, plus Spark, Iceberg and Trino
  compatibility for big data.
- **AI-native:** a built-in MCP server lets any AI agent discover, query and manage data safely.
- **Single:** one binary, which is both the server and an embeddable library.
- **Lightweight and the fastest:** measured against the best database for each workload, every
  release.
- **Every scale, every way of scaling:** from a single small file on a microcontroller to a cluster
  holding billions of billions of rows. Replicas, multi-primary, sharding, distributed SQL,
  serverless, tiered storage, edge copies and offline sync.
- **Every data type:** every type of PostgreSQL, SQL Server, MySQL, Oracle, SQLite, Redis, MongoDB and
  more, with each one's exact behavior.
- **Everywhere:** every OS, every CPU and every cloud.

Status: early. See [docs/roadmap.md](docs/roadmap.md) and [docs/decisions.md](docs/decisions.md).
To use EggDB as an embedded key-value store from Rust, see [docs/embedded.md](docs/embedded.md);
from a shell or a script, the `eggdb` command ([docs/cli.md](docs/cli.md)):

```sh
eggdb create app.db
eggdb put app.db user:1 Ada
eggdb get app.db user:1                      # Ada
eggdb scan app.db --prefix user:             # user:1  Ada
cat photo.jpg | eggdb put app.db photo       # any bytes, up to 1 GiB
eggdb scan app.db --format json | jq .key    # for scripts and agents
eggdb check app.db                           # "ok: every page passed its checksum …"
```

Build it from source with `cargo build --release`; the binary is `target/release/eggdb`.

Licensed under either the [MIT License](LICENSE-MIT) or the [Apache License 2.0](LICENSE-APACHE), at your option.
