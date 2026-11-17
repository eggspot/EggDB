from dataclasses import dataclass

from dbt.adapters.postgres import PostgresConnectionManager, PostgresCredentials


@dataclass
class EggdbCredentials(PostgresCredentials):
    dialect: str = "postgresql"

    @property
    def type(self):
        return "eggdb"

    @property
    def unique_field(self):
        return self.host

    def _connection_keys(self):
        return super()._connection_keys() + ("dialect",)


class EggdbConnectionManager(PostgresConnectionManager):
    TYPE = "eggdb"

    @classmethod
    def open(cls, connection):
        opened = super().open(connection)
        dialect = connection.credentials.dialect
        if dialect and dialect != "postgresql":
            cursor = opened.handle.cursor()
            cursor.execute("SET eggdb.dialect = %s", (dialect,))
            cursor.close()
        return opened
