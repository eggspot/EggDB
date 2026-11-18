"""The ``DatabaseWrapper`` Django loads for ``"ENGINE": "eggdb.django"``: Django's SQLite backend,
connecting through EggDB's DB-API driver and defining Django's SQL functions with
``Connection.create_function`` (D1266).

``NAME`` is the database file's path, or ``":memory:"``; Django's in-memory test databases
(``file:memorydb_...?mode=memory``) open in memory. The ``STDDEV``/``VAR`` aggregates Django's
SQLite backend would define are EggDB's own.
"""

import re
import types

import eggdb
from django.db.backends.sqlite3 import base as sqlite_base
from django.db.backends.sqlite3 import features as sqlite_features
from django.db.backends.sqlite3 import operations as sqlite_operations

try:
    from django.db.backends.sqlite3._functions import register as _register_functions
except ImportError:
    _register_functions = None

SQLITE_LANGUAGE_VERSION = (3, 45, 0)
_NAMED_PARAMETER = re.compile(r"%\((\w+)\)s")
_POSITIONAL_PARAMETER = re.compile(r"(?<!%)%s")


def _dbapi_module():
    module = types.ModuleType("eggdb_dbapi")
    module.__dict__.update(
        {name: value for name, value in vars(eggdb).items() if not name.startswith("__")}
    )
    module.sqlite_version_info = SQLITE_LANGUAGE_VERSION
    module.sqlite_version = ".".join(str(part) for part in SQLITE_LANGUAGE_VERSION)
    return module


Database = _dbapi_module()


def _qmark(query, parameters):
    if isinstance(parameters, dict):
        ordered = []

        def placed(match):
            ordered.append(parameters[match.group(1)])
            return "?"

        return _NAMED_PARAMETER.sub(placed, query).replace("%%", "%"), ordered
    if parameters is None:
        return query, ()
    return _POSITIONAL_PARAMETER.sub("?", query).replace("%%", "%"), list(parameters)


class CursorWrapper(eggdb.Cursor):
    """A cursor taking Django's ``%s`` and ``%(name)s`` placeholders."""

    lastrowid = None

    def execute(self, query, params=None):
        if params is None:
            return super().execute(query)
        query, ordered = _qmark(query, params)
        return super().execute(query, ordered)

    def executemany(self, query, param_list):
        rows = list(param_list)
        if not rows:
            self.rowcount = 0
            return self
        names = _NAMED_PARAMETER.findall(query)
        converted, _ = _qmark(query, rows[0])
        ordered = [[row[name] for name in names] if isinstance(row, dict) else list(row) for row in rows]
        return super().executemany(converted, ordered)


class _FunctionRegistrar:
    """What Django's function registration needs of a connection: functions go to EggDB, the
    aggregates are EggDB's own, and EggDB has the math functions SQLite builds optionally."""

    def __init__(self, connection):
        self._connection = connection

    def create_function(self, name, narg, func, *, deterministic=False):
        self._connection.create_function(name, narg, func, deterministic=deterministic)

    def create_aggregate(self, name, narg, aggregate):
        return None

    def execute(self, sql, parameters=()):
        if "sqlite_compileoption_used" in sql:
            return _OneRow((1,))
        return self._connection.execute(sql, parameters)


class _OneRow:
    def __init__(self, row):
        self._row = row

    def fetchone(self):
        return self._row


class DatabaseFeatures(sqlite_features.DatabaseFeatures):
    autocommits_when_autocommit_is_off = False


class DatabaseOperations(sqlite_operations.DatabaseOperations):
    def last_executed_query(self, cursor, sql, params):
        if not params:
            return sql
        return f"{sql} -- {params!r}"


class DatabaseWrapper(sqlite_base.DatabaseWrapper):
    vendor = "sqlite"
    display_name = "EggDB"
    Database = Database
    features_class = DatabaseFeatures
    ops_class = DatabaseOperations

    def get_connection_params(self):
        name = str(self.settings_dict["NAME"] or ":memory:")
        if name.startswith("file:") and "mode=memory" in name:
            name = ":memory:"
        options = dict(self.settings_dict.get("OPTIONS") or {})
        self.init_commands = []
        self.transaction_mode = None
        return {"path": name, "read_only": bool(options.get("read_only", False))}

    def get_new_connection(self, conn_params):
        connection = eggdb.connect(conn_params["path"], dialect="sqlite", read_only=conn_params["read_only"])
        connection.autocommit = True
        if _register_functions is not None:
            _register_functions(_FunctionRegistrar(connection))
        connection.execute("PRAGMA foreign_keys = ON")
        return connection

    def create_cursor(self, name=None):
        return CursorWrapper(self.connection)

    def _set_autocommit(self, autocommit):
        with self.wrap_database_errors:
            self.connection.autocommit = autocommit

    def is_usable(self):
        return self.connection is not None and self.connection._session is not None
