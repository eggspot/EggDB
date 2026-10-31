# Connecting tools to EggDB

Most tools reach EggDB today through a protocol it already speaks, so they need no EggDB plugin.
Native connectors for each tool's own plugin system follow (D46, D239). The `compatibility` CI
job runs the tools listed as checked against `eggdb serve` and reports each result in the run's
summary. It starts the compatibility matrix D44 publishes.

| Tool | How it connects today | Checked |
|------|-----------------------|---------|
| dbt | `dbt-postgres` adapter over the PostgreSQL protocol (`eggdb serve --listen`) | `dbt debug`, `dbt run`, `dbt test` in CI |
| Grafana | PostgreSQL data source; Prometheus data source on `--observability-listen` (PromQL, D220) | PromQL through influxdb-client and HTTP in CI |
| Power BI, Tableau, Metabase | their PostgreSQL connectors | not yet |
| Airbyte | PostgreSQL source and destination connectors | not yet |
| Spark, Trino | JDBC with pgjdbc (Spark), the PostgreSQL connector (Trino) | pgjdbc itself in CI |
| Kafka Connect | JDBC source and sink connectors with pgjdbc; or EggDB's own Kafka protocol (D213) | Kafka clients in CI |
| Any tool with S3 support | EggDB's S3-compatible endpoint (D219) | boto3 in CI |

EggDB's own pipelines need no extra tool: `eggdb_foreign` reads other servers and Parquet files
(D229), `pg_cron` schedules SQL (D200), and change capture streams changes out (D210).
