from airflow.providers.postgres.hooks.postgres import PostgresHook

DIALECTS = ("postgresql", "mysql", "sqlserver", "oracle", "sqlite")


class EggDBHook(PostgresHook):
    default_conn_name = "eggdb_default"
    conn_type = "eggdb"
    hook_name = "EggDB"

    def __init__(self, *args, dialect=None, **kwargs):
        if "eggdb_conn_id" in kwargs:
            kwargs["postgres_conn_id"] = kwargs.pop("eggdb_conn_id")
        super().__init__(*args, **kwargs)
        self.dialect = dialect

    def _chosen_dialect(self):
        if self.dialect:
            return self.dialect
        connection = self.get_connection(getattr(self, self.conn_name_attr))
        return connection.extra_dejson.get("dialect", "postgresql")

    def get_conn(self):
        connection = super().get_conn()
        dialect = self._chosen_dialect()
        if dialect not in DIALECTS:
            raise ValueError(f"EggDB dialect {dialect!r} is not one of {', '.join(DIALECTS)}")
        if dialect != "postgresql":
            with connection.cursor() as cursor:
                cursor.execute("SET eggdb.dialect = %s", (dialect,))
        return connection

    def backup(self, path, clone=False):
        statement = "BACKUP TO %s CLONE" if clone else "BACKUP TO %s"
        self.run(statement, parameters=(path,))

    @classmethod
    def get_ui_field_behaviour(cls):
        return {
            "hidden_fields": [],
            "relabeling": {"schema": "Database"},
            "placeholders": {"port": "5432", "extra": '{"dialect": "postgresql", "sslmode": "require"}'},
        }
