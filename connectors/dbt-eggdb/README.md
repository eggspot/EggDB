# dbt-eggdb

The dbt adapter for EggDB. It speaks to `eggdb serve` over EggDB's PostgreSQL protocol, on top of
`dbt-postgres`, and lists relations from `information_schema` the way EggDB answers it.

```sh
pip install dbt-eggdb
```

`profiles.yml`:

```yaml
my_project:
  target: dev
  outputs:
    dev:
      type: eggdb
      host: localhost
      port: 5432
      user: eggdb
      password: "{{ env_var('EGGDB_PASSWORD') }}"
      dbname: eggdb
      schema: analytics
      threads: 4
      sslmode: prefer
      dialect: postgresql
```

`dialect` picks the SQL dialect models are written in (`postgresql`, `mysql`, `sqlserver`, `oracle`
or `sqlite`); every dialect sees the same tables. Incremental models, snapshots, seeds and tests
work as with `dbt-postgres`.

Licensed under MIT OR Apache-2.0, like EggDB.
