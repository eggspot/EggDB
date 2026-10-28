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

## Today: v0.2.0, an embedded key-value store in one file

Everything above is the goal; the first release people can use is smaller. v0.2.0 is an
embedded, crash-safe, transactional key-value store: one file, a Rust API, the `eggdb` command
and `eggdb ui`, a full-screen terminal UI, and `eggdb sql` runs the first parts of the PostgreSQL
dialect on the same file: tables, joins, `GROUP BY` with aggregates ([docs/sql.md](docs/sql.md)).
`eggdb serve` serves the file to psql and PostgreSQL drivers, with `--mysql-listen` to the
`mysql` client and MySQL drivers in MySQL's SQL, and with `--tds-listen` to Microsoft's
`sqlcmd` in Transact-SQL, on loopback only until EggDB has TLS, with one
writer at a time. `eggdb sql --dialect sqlite` and the embedded API run SQLite's SQL on the same
file, and `--dialect oracle` Oracle's (no Oracle wire protocol). `eggdb mcp` lets AI agents
such as Claude Code read the file over MCP, read-only, every call in an audit log
([docs/mcp.md](docs/mcp.md)). What is in it and what
is not: [docs/releases/v0.2.0.md](docs/releases/v0.2.0.md);
what is tested and what is not: [docs/correctness.md](docs/correctness.md); the plan:
[docs/roadmap.md](docs/roadmap.md) and [docs/decisions.md](docs/decisions.md).

### Install: download one file

Each release on the [releases page](https://github.com/eggspot/EggDB/releases) has one archive
per platform holding the `eggdb` binary, which needs nothing else (the Linux `musl` builds are
static and run on any distribution). On Linux on x86-64:

```sh
curl -fsSL https://github.com/eggspot/EggDB/releases/download/v0.2.0/eggdb-v0.2.0-x86_64-unknown-linux-musl.tar.gz | tar -xz
sudo mv eggdb-v0.2.0-x86_64-unknown-linux-musl/eggdb /usr/local/bin/
eggdb version
```

Other platforms swap the target: `aarch64-unknown-linux-musl`, `aarch64-apple-darwin` (Apple
silicon), `x86_64-apple-darwin`, and `x86_64-pc-windows-msvc` or `aarch64-pc-windows-msvc` as a
`.zip`. `SHA256SUMS` and `gh attestation verify <archive> -R eggspot/EggDB` check a download. To
build it instead: `cargo build --release -p eggdb`, and the binary is `target/release/eggdb`.

### Use it from a shell

```sh
eggdb create app.db
eggdb put app.db user:1 Ada
eggdb get app.db user:1                      # Ada
eggdb scan app.db --prefix user:             # user:1  Ada
cat photo.jpg | eggdb put app.db photo       # any bytes, up to 1 GiB
eggdb scan app.db --format json | jq .key    # for scripts and agents
eggdb check app.db                           # "ok: every page passed its checksum …"
eggdb export app.db backup.jsonl             # every entry as JSON lines; import loads it
eggdb ui app.db                              # browse and edit it full screen; q quits
eggdb sql shop.db --create -c "CREATE TABLE items (id integer PRIMARY KEY, name text)"
EGGDB_PASSWORD=choose-one eggdb serve shop.db --mysql-listen 3306 --tds-listen 1433  # PostgreSQL on :5432, MySQL on :3306, SQL Server on :1433
PGPASSWORD=choose-one psql -h 127.0.0.1 -U eggdb shop -c "SELECT * FROM items"
MYSQL_PWD=choose-one mysql -h 127.0.0.1 -P 3306 -u eggdb shop -e "SHOW TABLES"
SQLCMDPASSWORD=choose-one sqlcmd -S 127.0.0.1,1433 -U eggdb -d shop -Q "SELECT TOP 1 * FROM items"
```

Or work in one session, with history and tab completion:

```text
$ eggdb shell shop.db
EggDB 0.2.0: shop.db, open read-write (1 table)
Type .help for help. SQL ends with ; and .quit or Ctrl-D quits.
shop=> insert into items values (1, 'egg'), (2, 'bread');
INSERT 0 2
shop=> select name from items order by id;
 name
-------
 egg
 bread
(2 rows)

shop=> put greeting hello
shop=> get greeting
hello
shop=> .quit
```

Every command, the output formats, exit codes and the UI's keys: [docs/cli.md](docs/cli.md).

### Use it from Rust

```toml
[dependencies]
eggdb = { git = "https://github.com/eggspot/EggDB", tag = "v0.2.0" }
```

```rust,no_run
use eggdb::Database;

fn main() -> eggdb::Result<()> {
    let db = Database::open("app.db")?;
    db.write(|tx| {
        tx.put("user:1", "Ada")?;
        tx.put("user:2", "Grace")?;
        Ok(())
    })?;
    let tx = db.begin_read()?;
    for entry in tx.scan_prefix("user:") {
        let (key, value) = entry?;
        println!("{} = {}", String::from_utf8_lossy(&key), String::from_utf8_lossy(&value));
    }
    drop(tx);
    db.close()
}
```

Transactions, scans, durability, errors and limits: [docs/embedded.md](docs/embedded.md).

Licensed under either the [MIT License](LICENSE-MIT) or the [Apache License 2.0](LICENSE-APACHE), at your option.
