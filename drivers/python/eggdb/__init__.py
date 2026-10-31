"""EggDB's Python driver: DB-API 2.0 (PEP 249) on EggDB's C API, embedded in the process.

``connect(path, dialect="sqlite")`` opens a database file; ``":memory:"`` opens one in memory.
Placeholders are ``?`` (``paramstyle = "qmark"``) in every dialect. The shared library is
found through ``EGGDB_LIBRARY``, next to this package, or on the system's library path.
"""

import ctypes
import ctypes.util
import datetime
import os
import sys
from ctypes import POINTER, Structure, byref, c_char_p, c_double, c_int, c_int32, c_int64, c_size_t, c_uint8, c_uint32, c_uint64, c_void_p
from pathlib import Path

apilevel = "2.0"
threadsafety = 1
paramstyle = "qmark"

_NULL, _INTEGER, _REAL, _TEXT, _BLOB = 0, 1, 2, 3, 4
_OPEN_READ_ONLY = 1


class Error(Exception):
    """The base of every error the driver raises."""

    def __init__(self, message, sqlstate=None):
        super().__init__(message)
        self.sqlstate = sqlstate


class Warning(Exception):
    """A warning, as PEP 249 defines it."""


class InterfaceError(Error):
    """A misuse of the driver."""


class DatabaseError(Error):
    """An error the database reported."""


class DataError(DatabaseError):
    """A value the database refused."""


class OperationalError(DatabaseError):
    """The database could not carry out the operation."""


class IntegrityError(DatabaseError):
    """A constraint was violated."""


class InternalError(DatabaseError):
    """The database met an internal error."""


class ProgrammingError(DatabaseError):
    """A mistake in the SQL or its parameters."""


class NotSupportedError(DatabaseError):
    """A feature the database does not support."""


class _Options(Structure):
    _fields_ = [("flags", c_uint32), ("durability", c_uint32), ("cache_bytes", c_uint64)]


class _Value(Structure):
    _fields_ = [
        ("kind", c_int32),
        ("integer", c_int64),
        ("real", c_double),
        ("data", POINTER(c_uint8)),
        ("len", c_size_t),
    ]


class _Parameter(Structure):
    _fields_ = [("text", c_char_p), ("len", c_size_t)]


class _Result(Structure):
    _fields_ = [
        ("tag", c_char_p),
        ("column_count", c_size_t),
        ("column_names", POINTER(c_char_p)),
        ("row_count", c_size_t),
        ("values", POINTER(_Value)),
    ]


def _library_path():
    given = os.environ.get("EGGDB_LIBRARY")
    if given:
        return given
    names = {"darwin": "libeggdb_capi.dylib", "win32": "eggdb_capi.dll"}
    beside = Path(__file__).resolve().parent / names.get(sys.platform, "libeggdb_capi.so")
    if beside.exists():
        return str(beside)
    found = ctypes.util.find_library("eggdb_capi")
    if found:
        return found
    raise InterfaceError("cannot find the EggDB C library; set EGGDB_LIBRARY to its path")


_library = None


def _api():
    global _library
    if _library is not None:
        return _library
    library = ctypes.CDLL(_library_path())
    prototypes = {
        "eggdb_last_error": (c_char_p, []),
        "eggdb_last_sqlstate": (c_char_p, []),
        "eggdb_open": (c_int, [c_char_p, POINTER(_Options), POINTER(c_void_p)]),
        "eggdb_close": (c_int, [c_void_p]),
        "eggdb_sql_open": (c_int, [c_void_p, c_char_p, POINTER(c_void_p)]),
        "eggdb_sql_execute_with_parameters": (
            c_int,
            [c_void_p, c_char_p, POINTER(_Parameter), c_size_t, POINTER(POINTER(_Result)), POINTER(c_size_t)],
        ),
        "eggdb_sql_close": (c_int, [c_void_p]),
        "eggdb_sql_connect": (c_int, [c_char_p, POINTER(c_void_p)]),
    }
    for name, (restype, argtypes) in prototypes.items():
        function = getattr(library, name)
        function.restype = restype
        function.argtypes = argtypes
    _library = library
    return library


def _raise_last(status):
    api = _api()
    message = (api.eggdb_last_error() or b"").decode("utf-8", "replace")
    sqlstate = (api.eggdb_last_sqlstate() or b"").decode("ascii", "replace") or None
    kind = DatabaseError
    if sqlstate and sqlstate.startswith("23"):
        kind = IntegrityError
    elif sqlstate and sqlstate.startswith("42"):
        kind = ProgrammingError
    elif sqlstate and sqlstate.startswith("22"):
        kind = DataError
    elif sqlstate and sqlstate.startswith("0A"):
        kind = NotSupportedError
    elif status != 103:
        kind = OperationalError
    raise kind(message or f"EggDB status {status}", sqlstate)


def _parameter_text(value, dialect):
    if value is None:
        return None
    if isinstance(value, bool):
        return ("true" if value else "false") if dialect == "postgresql" else ("1" if value else "0")
    if isinstance(value, (int, float)):
        return repr(value)
    if isinstance(value, (bytes, bytearray, memoryview)):
        return "\\x" + bytes(value).hex()
    if isinstance(value, (datetime.datetime, datetime.date, datetime.time)):
        return value.isoformat(sep=" ") if isinstance(value, datetime.datetime) else value.isoformat()
    return str(value)


def _placeholders(sql, dialect):
    if dialect != "postgresql":
        return sql
    out = []
    quote = None
    number = 0
    for character in sql:
        if quote:
            if character == quote:
                quote = None
            out.append(character)
        elif character in "'\"":
            quote = character
            out.append(character)
        elif character == "?":
            number += 1
            out.append(f"${number}")
        else:
            out.append(character)
    return "".join(out)


def _converted(value):
    if value.kind == _NULL:
        return None
    if value.kind == _INTEGER:
        return value.integer
    if value.kind == _REAL:
        return value.real
    data = ctypes.string_at(value.data, value.len)
    if value.kind == _BLOB:
        return data
    return data.decode("utf-8")


class Connection:
    """A connection to an embedded EggDB database."""

    def __init__(self, path, dialect, read_only):
        api = _api()
        if _is_server_url(path):
            session = c_void_p()
            status = api.eggdb_sql_connect(str(path).encode("utf-8"), byref(session))
            if status != 0:
                _raise_last(status)
            self._database = None
            self._session = session
            self._dialect = "postgresql"
            self._in_transaction = False
            self.autocommit = False
            return
        options = _Options(_OPEN_READ_ONLY if read_only else 0, 0, 0)
        database = c_void_p()
        status = api.eggdb_open(os.fsencode(os.fspath(path)), byref(options), byref(database))
        if status != 0:
            _raise_last(status)
        session = c_void_p()
        status = api.eggdb_sql_open(database, dialect.encode(), byref(session))
        if status != 0:
            api.eggdb_close(database)
            _raise_last(status)
        self._database = database
        self._session = session
        self._dialect = dialect
        self._in_transaction = False
        self.autocommit = False

    def _execute(self, sql, parameters=()):
        if self._session is None:
            raise InterfaceError("the connection is closed")
        api = _api()
        texts = [_parameter_text(value, self._dialect) for value in parameters]
        encoded = [None if text is None else text.encode("utf-8") for text in texts]
        array = (_Parameter * max(len(encoded), 1))()
        for index, text in enumerate(encoded):
            array[index] = _Parameter(text, 0 if text is None else len(text))
        results = POINTER(_Result)()
        count = c_size_t()
        status = api.eggdb_sql_execute_with_parameters(
            self._session,
            _placeholders(sql, self._dialect).encode("utf-8"),
            array,
            len(encoded),
            byref(results),
            byref(count),
        )
        if status != 0:
            _raise_last(status)
        gathered = []
        for index in range(count.value):
            result = results[index]
            names = [result.column_names[column].decode("utf-8") for column in range(result.column_count)]
            rows = []
            for row in range(result.row_count):
                start = row * result.column_count
                rows.append(tuple(_converted(result.values[start + column]) for column in range(result.column_count)))
            gathered.append(((result.tag or b"").decode("utf-8"), names, rows))
        return gathered

    def _begin_if_needed(self):
        if not self.autocommit and not self._in_transaction:
            self._execute("BEGIN")
            self._in_transaction = True

    def cursor(self):
        """A new cursor."""
        return Cursor(self)

    def execute(self, sql, parameters=()):
        """Runs `sql` on a new cursor and returns it."""
        cursor = self.cursor()
        cursor.execute(sql, parameters)
        return cursor

    def commit(self):
        """Commits the open transaction, when there is one."""
        if self._in_transaction:
            self._execute("COMMIT")
            self._in_transaction = False

    def rollback(self):
        """Rolls the open transaction back, when there is one."""
        if self._in_transaction:
            self._execute("ROLLBACK")
            self._in_transaction = False

    def close(self):
        """Rolls back an open transaction and closes the connection."""
        if self._session is None:
            return
        api = _api()
        api.eggdb_sql_close(self._session)
        if self._database is not None:
            api.eggdb_close(self._database)
        self._session = None
        self._database = None

    def __enter__(self):
        return self

    def __exit__(self, kind, value, traceback):
        if kind is None:
            self.commit()
        else:
            self.rollback()
        return False


class Cursor:
    """A DB-API cursor: runs statements and hands out their rows."""

    arraysize = 1

    def __init__(self, connection):
        self.connection = connection
        self.description = None
        self.rowcount = -1
        self._rows = []

    def execute(self, sql, parameters=()):
        """Runs one statement with its parameters."""
        self.connection._begin_if_needed()
        gathered = self.connection._execute(sql, parameters)
        self.description = None
        self.rowcount = -1
        self._rows = []
        if gathered:
            tag, names, rows = gathered[-1]
            if names:
                self.description = [(name, None, None, None, None, None, None) for name in names]
                self._rows = rows
                self.rowcount = len(rows)
            else:
                words = tag.split()
                if words and words[0] in ("INSERT", "UPDATE", "DELETE") and words[-1].isdigit():
                    self.rowcount = int(words[-1])
        return self

    def executemany(self, sql, sequence):
        """Runs one statement for each set of parameters."""
        total = 0
        for parameters in sequence:
            self.execute(sql, parameters)
            total += max(self.rowcount, 0)
        self.rowcount = total
        return self

    def fetchone(self):
        """The next row, or None."""
        return self._rows.pop(0) if self._rows else None

    def fetchmany(self, size=None):
        """Up to `size` rows (the cursor's arraysize by default)."""
        size = self.arraysize if size is None else size
        taken, self._rows = self._rows[:size], self._rows[size:]
        return taken

    def fetchall(self):
        """Every row left."""
        taken, self._rows = self._rows, []
        return taken

    def close(self):
        """Forgets the rows left."""
        self._rows = []

    def setinputsizes(self, sizes):
        """Accepted and ignored, as PEP 249 allows."""

    def setoutputsize(self, size, column=None):
        """Accepted and ignored, as PEP 249 allows."""

    def __iter__(self):
        while self._rows:
            yield self._rows.pop(0)


def _is_server_url(path):
    return isinstance(path, str) and path.startswith(("postgresql://", "postgres://"))


def connect(path, dialect="sqlite", read_only=False):
    """Opens the database at `path` (":memory:" for one in memory) in `dialect`, or connects to
    an EggDB server when `path` is a `postgresql://` URL (its dialect is then PostgreSQL's)."""
    if dialect not in ("sqlite", "postgresql", "mysql", "oracle", "sqlserver"):
        raise InterfaceError(f"unknown dialect {dialect!r}")
    return Connection(path, dialect, read_only)
