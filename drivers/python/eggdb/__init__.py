"""EggDB's Python driver: DB-API 2.0 (PEP 249) on EggDB's C API, embedded in the process.

``connect(path, dialect="sqlite")`` opens a database file; ``":memory:"`` opens one in memory.
``open_key_value(path)`` opens the same file's key-value store: transactions over the default
key space and its named collections.
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


class _Entry(Structure):
    _fields_ = [
        ("key", POINTER(c_uint8)),
        ("key_len", c_size_t),
        ("value", POINTER(c_uint8)),
        ("value_len", c_size_t),
    ]


class _CollectionStats(Structure):
    _fields_ = [("entries", c_uint64), ("key_bytes", c_uint64), ("value_bytes", c_uint64)]


_SCAN_REVERSE, _SCAN_END_INCLUSIVE, _SCAN_PREFIX = 1, 2, 4


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
        "eggdb_free": (None, [POINTER(c_uint8), c_size_t]),
        "eggdb_begin_read": (c_int, [c_void_p, POINTER(c_void_p)]),
        "eggdb_begin_write": (c_int, [c_void_p, POINTER(c_void_p)]),
        "eggdb_commit": (c_int, [c_void_p, POINTER(c_uint64)]),
        "eggdb_rollback": (c_int, [c_void_p]),
        "eggdb_get": (c_int, [c_void_p, c_char_p, c_char_p, c_size_t, POINTER(POINTER(c_uint8)), POINTER(c_size_t)]),
        "eggdb_put": (c_int, [c_void_p, c_char_p, c_char_p, c_size_t, c_char_p, c_size_t]),
        "eggdb_put_with_expiry": (c_int, [c_void_p, c_char_p, c_char_p, c_size_t, c_char_p, c_size_t, c_uint64]),
        "eggdb_delete": (c_int, [c_void_p, c_char_p, c_char_p, c_size_t, POINTER(c_int)]),
        "eggdb_create_collection": (c_int, [c_void_p, c_char_p, POINTER(c_int)]),
        "eggdb_drop_collection": (c_int, [c_void_p, c_char_p, POINTER(c_int)]),
        "eggdb_rename_collection": (c_int, [c_void_p, c_char_p, c_char_p]),
        "eggdb_collection_names": (c_int, [c_void_p, POINTER(POINTER(c_uint8)), POINTER(c_size_t), POINTER(c_size_t)]),
        "eggdb_collection_stats": (c_int, [c_void_p, c_char_p, POINTER(_CollectionStats)]),
        "eggdb_scan_open": (c_int, [c_void_p, c_char_p, c_char_p, c_size_t, c_char_p, c_size_t, c_uint32, POINTER(c_void_p)]),
        "eggdb_scan_next": (c_int, [c_void_p, c_void_p, c_size_t, POINTER(POINTER(_Entry)), POINTER(c_size_t)]),
        "eggdb_scan_free": (None, [c_void_p]),
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


def _is_bytes(value):
    return isinstance(value, (bytes, bytearray, memoryview))


def _placeholders(sql, dialect, parameters):
    out = []
    kept = []
    quote = None
    given = 0
    for character in sql:
        if quote:
            if character == quote:
                quote = None
            out.append(character)
        elif character in "'\"":
            quote = character
            out.append(character)
        elif character == "?":
            value = parameters[given] if given < len(parameters) else None
            given += 1
            if dialect != "postgresql" and _is_bytes(value):
                out.append("X'" + bytes(value).hex() + "'")
                continue
            kept.append(value)
            out.append(f"${len(kept)}" if dialect == "postgresql" else "?")
        else:
            out.append(character)
    kept.extend(parameters[given:])
    return "".join(out), kept


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
        sql, parameters = _placeholders(sql, self._dialect, list(parameters))
        texts = [_parameter_text(value, self._dialect) for value in parameters]
        encoded = [None if text is None else text.encode("utf-8") for text in texts]
        array = (_Parameter * max(len(encoded), 1))()
        for index, text in enumerate(encoded):
            array[index] = _Parameter(text, 0 if text is None else len(text))
        results = POINTER(_Result)()
        count = c_size_t()
        status = api.eggdb_sql_execute_with_parameters(
            self._session,
            sql.encode("utf-8"),
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


def _key_bytes(value):
    if isinstance(value, str):
        return value.encode("utf-8")
    if _is_bytes(value):
        return bytes(value)
    raise InterfaceError("keys and values are bytes or str")


def _name(collection):
    return None if collection is None else str(collection).encode("utf-8")


def _checked(status):
    if status != 0:
        _raise_last(status)


class Transaction:
    """One key-value transaction: a snapshot to read, or a write that commits atomically.

    Every call takes `collection`, a collection's name, or None for the default key space."""

    def __init__(self, handle, writing):
        self._handle = handle
        self.writing = writing

    def _live(self):
        if self._handle is None:
            raise InterfaceError("the transaction has ended")
        return self._handle

    def get(self, key, collection=None):
        """The value of `key`, or None when it is absent or has expired."""
        api = _api()
        key = _key_bytes(key)
        data = POINTER(c_uint8)()
        length = c_size_t()
        _checked(api.eggdb_get(self._live(), _name(collection), key, len(key), byref(data), byref(length)))
        if not data:
            return None
        try:
            return ctypes.string_at(data, length.value)
        finally:
            api.eggdb_free(data, length.value)

    def put(self, key, value, collection=None, ttl_ms=None):
        """Stores `value` under `key`, with a time to live in milliseconds when `ttl_ms` is given."""
        api = _api()
        key, value = _key_bytes(key), _key_bytes(value)
        if ttl_ms is None:
            _checked(api.eggdb_put(self._live(), _name(collection), key, len(key), value, len(value)))
        else:
            _checked(api.eggdb_put_with_expiry(self._live(), _name(collection), key, len(key), value, len(value), int(ttl_ms)))

    def delete(self, key, collection=None):
        """Removes `key`; True when it was there."""
        key = _key_bytes(key)
        deleted = c_int()
        _checked(_api().eggdb_delete(self._live(), _name(collection), key, len(key), byref(deleted)))
        return bool(deleted.value)

    def scan(self, start=None, end=None, prefix=None, reverse=False, end_inclusive=False, collection=None):
        """The (key, value) pairs from `start` up to `end`, or those whose key begins with
        `prefix`, in key order (the other way with `reverse`), read in batches."""
        api = _api()
        flags = (_SCAN_REVERSE if reverse else 0) | (_SCAN_END_INCLUSIVE if end_inclusive else 0)
        if prefix is not None:
            flags |= _SCAN_PREFIX
            start, end = prefix, None
        start = None if start is None else _key_bytes(start)
        end = None if end is None else _key_bytes(end)
        scan = c_void_p()
        _checked(
            api.eggdb_scan_open(
                self._live(),
                _name(collection),
                start,
                0 if start is None else len(start),
                end,
                0 if end is None else len(end),
                flags,
                byref(scan),
            )
        )
        try:
            while True:
                entries = POINTER(_Entry)()
                count = c_size_t()
                _checked(api.eggdb_scan_next(self._live(), scan, 0, byref(entries), byref(count)))
                if count.value == 0:
                    return
                batch = [
                    (
                        ctypes.string_at(entries[index].key, entries[index].key_len),
                        ctypes.string_at(entries[index].value, entries[index].value_len),
                    )
                    for index in range(count.value)
                ]
                yield from batch
        finally:
            api.eggdb_scan_free(scan)

    def create_collection(self, name):
        """Creates the collection `name`; True when it was made, False when it was there."""
        created = c_int()
        _checked(_api().eggdb_create_collection(self._live(), _name(name), byref(created)))
        return bool(created.value)

    def drop_collection(self, name):
        """Drops the collection `name` with its keys; True when it was there."""
        dropped = c_int()
        _checked(_api().eggdb_drop_collection(self._live(), _name(name), byref(dropped)))
        return bool(dropped.value)

    def rename_collection(self, name, new_name):
        """Renames the collection `name` to `new_name`, keeping its keys."""
        _checked(_api().eggdb_rename_collection(self._live(), _name(name), _name(new_name)))

    def collections(self):
        """The collections' names, in name order."""
        api = _api()
        data = POINTER(c_uint8)()
        length = c_size_t()
        count = c_size_t()
        _checked(api.eggdb_collection_names(self._live(), byref(data), byref(length), byref(count)))
        if not data:
            return []
        try:
            joined = ctypes.string_at(data, length.value)
        finally:
            api.eggdb_free(data, length.value)
        return [name.decode("utf-8") for name in joined.split(b"\0")[: count.value]]

    def collection_stats(self, name):
        """The entries, key bytes and value bytes the collection `name` holds (read transactions)."""
        stats = _CollectionStats()
        _checked(_api().eggdb_collection_stats(self._live(), _name(name), byref(stats)))
        return {"entries": stats.entries, "key_bytes": stats.key_bytes, "value_bytes": stats.value_bytes}

    def commit(self):
        """Ends the transaction, committing its writes; returns the commit's stamp."""
        stamp = c_uint64()
        handle, self._handle = self._live(), None
        _checked(_api().eggdb_commit(handle, byref(stamp)))
        return stamp.value

    def rollback(self):
        """Ends the transaction without committing its writes."""
        if self._handle is not None:
            handle, self._handle = self._handle, None
            _checked(_api().eggdb_rollback(handle))

    def __enter__(self):
        return self

    def __exit__(self, kind, value, traceback):
        if kind is None and self._handle is not None:
            self.commit()
        else:
            self.rollback()
        return False


class KeyValue:
    """An embedded database file's key-value store."""

    def __init__(self, path, read_only):
        options = _Options(_OPEN_READ_ONLY if read_only else 0, 0, 0)
        database = c_void_p()
        _checked(_api().eggdb_open(os.fsencode(os.fspath(path)), byref(options), byref(database)))
        self._database = database

    def _begin(self, writing):
        if self._database is None:
            raise InterfaceError("the store is closed")
        handle = c_void_p()
        begin = _api().eggdb_begin_write if writing else _api().eggdb_begin_read
        _checked(begin(self._database, byref(handle)))
        return Transaction(handle, writing)

    def read(self):
        """A read transaction over one snapshot."""
        return self._begin(False)

    def write(self):
        """A write transaction; as a context manager it commits when the block ends normally."""
        return self._begin(True)

    def close(self):
        """Closes the file."""
        if self._database is not None:
            _api().eggdb_close(self._database)
            self._database = None

    def __enter__(self):
        return self

    def __exit__(self, kind, value, traceback):
        self.close()
        return False


def open_key_value(path, read_only=False):
    """Opens the key-value store of the database file at `path`."""
    return KeyValue(path, read_only)


def _is_server_url(path):
    return isinstance(path, str) and path.startswith(("postgresql://", "postgres://"))


def connect(path, dialect="sqlite", read_only=False):
    """Opens the database at `path` (":memory:" for one in memory) in `dialect`, or connects to
    an EggDB server when `path` is a `postgresql://` URL (its dialect is then PostgreSQL's)."""
    if dialect not in ("sqlite", "postgresql", "mysql", "oracle", "sqlserver"):
        raise InterfaceError(f"unknown dialect {dialect!r}")
    return Connection(path, dialect, read_only)
