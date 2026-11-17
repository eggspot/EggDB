# airflow-provider-eggdb

Apache Airflow hooks and operators for EggDB, over its PostgreSQL protocol (`eggdb serve`).

```sh
pip install airflow-provider-eggdb
```

Add a connection of type **EggDB** (`eggdb_default`): host, port (5432), database, login and
password, and in Extra an optional `{"dialect": "mysql"}` for the SQL dialect statements are
written in (`postgresql`, `mysql`, `sqlserver`, `oracle` or `sqlite`).

```python
from airflow_provider_eggdb.operators.eggdb import EggDBBackupOperator, EggDBOperator

load = EggDBOperator(task_id="load", sql="INSERT INTO daily SELECT * FROM staging")
backup = EggDBBackupOperator(task_id="backup", path="/backups/{{ ds }}.eggdb")
load >> backup
```

`EggDBHook` is a `PostgresHook`, so `get_records`, `get_pandas_df`, `insert_rows` and
`copy_expert` work as with PostgreSQL; `backup(path)` runs `BACKUP TO`, which needs a superuser.

Licensed under MIT OR Apache-2.0, like EggDB.
