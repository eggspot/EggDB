"""EggDB's SQLAlchemy dialect: ``create_engine("eggdb:///path/to/app.db")`` runs SQLAlchemy Core
and the ORM on an embedded EggDB file through this package's DB-API driver, in EggDB's SQLite
dialect, so SQLAlchemy's SQLite SQL compiler, types and reflection apply unchanged.

``eggdb:///:memory:`` opens a database in memory, ``?mode=ro`` opens the file read-only, and
``import eggdb.sqlalchemy`` registers the ``eggdb`` name with SQLAlchemy.
"""

import types

import eggdb
from sqlalchemy import pool
from sqlalchemy.dialects import registry
from sqlalchemy.dialects.sqlite.base import SQLiteDialect

SQLITE_LANGUAGE_VERSION = (3, 45, 0)


def _dbapi_module():
    module = types.ModuleType("eggdb_dbapi")
    module.__dict__.update(
        {name: value for name, value in vars(eggdb).items() if not name.startswith("__")}
    )
    module.sqlite_version_info = SQLITE_LANGUAGE_VERSION
    module.sqlite_version = ".".join(str(part) for part in SQLITE_LANGUAGE_VERSION)
    return module


class EggDBDialect(SQLiteDialect):
    """SQLAlchemy's SQLite dialect, connecting through EggDB's DB-API driver."""

    name = "eggdb"
    driver = "eggdb"
    supports_statement_cache = True
    default_paramstyle = "qmark"
    supports_native_decimal = False

    @classmethod
    def import_dbapi(cls):
        return _dbapi_module()

    @classmethod
    def dbapi(cls):
        return _dbapi_module()

    @classmethod
    def get_pool_class(cls, url):
        if (url.database or ":memory:") == ":memory:":
            return pool.SingletonThreadPool
        return pool.QueuePool

    def create_connect_args(self, url):
        path = url.database or ":memory:"
        read_only = url.query.get("mode") == "ro"
        return [path], {"dialect": "sqlite", "read_only": read_only}

    def _get_server_version_info(self, connection):
        return SQLITE_LANGUAGE_VERSION

    def get_isolation_level(self, dbapi_connection):
        return "AUTOCOMMIT" if dbapi_connection.autocommit else "SERIALIZABLE"

    def set_isolation_level(self, dbapi_connection, level):
        if level == "AUTOCOMMIT":
            dbapi_connection.commit()
            dbapi_connection.autocommit = True
        elif level == "SERIALIZABLE":
            dbapi_connection.autocommit = False
        else:
            raise eggdb.NotSupportedError(f"EggDB runs every transaction SERIALIZABLE, not {level}")

    def get_default_isolation_level(self, dbapi_connection):
        return "SERIALIZABLE"

    def on_connect(self):
        return None


registry.register("eggdb", "eggdb.sqlalchemy", "EggDBDialect")

dialect = EggDBDialect
