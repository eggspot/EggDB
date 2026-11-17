from dbt.adapters.postgres import PostgresAdapter

from dbt.adapters.eggdb.connections import EggdbConnectionManager


class EggdbAdapter(PostgresAdapter):
    ConnectionManager = EggdbConnectionManager

    @classmethod
    def date_function(cls):
        return "now()"

    def debug_query(self):
        self.execute("select 1 as id")
