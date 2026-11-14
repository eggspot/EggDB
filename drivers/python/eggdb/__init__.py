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
_library_file = None
_native = None
_native_checked = False


def _api():
    global _library, _library_file
    if _library is not None:
        return _library
    _library_file = _library_path()
    library = ctypes.CDLL(_library_file)
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
        "eggdb_sql_prepare": (c_int, [c_void_p, c_char_p, c_size_t, POINTER(c_void_p)]),
        "eggdb_stmt_parameter_count": (c_int, [c_void_p, POINTER(c_size_t)]),
        "eggdb_stmt_bind_null": (c_int, [c_void_p, c_size_t]),
        "eggdb_stmt_bind_int64": (c_int, [c_void_p, c_size_t, c_int64]),
        "eggdb_stmt_bind_double": (c_int, [c_void_p, c_size_t, c_double]),
        "eggdb_stmt_bind_bool": (c_int, [c_void_p, c_size_t, c_int]),
        "eggdb_stmt_bind_text": (c_int, [c_void_p, c_size_t, c_char_p, c_size_t]),
        "eggdb_stmt_bind_blob": (c_int, [c_void_p, c_size_t, c_char_p, c_size_t]),
        "eggdb_stmt_clear_bindings": (c_int, [c_void_p]),
        "eggdb_stmt_execute": (c_int, [c_void_p, POINTER(POINTER(_Result)), POINTER(c_size_t)]),
        "eggdb_stmt_reset": (c_int, [c_void_p]),
        "eggdb_stmt_column_count": (c_int, [c_void_p, POINTER(c_size_t)]),
        "eggdb_stmt_column_name": (c_int, [c_void_p, c_size_t, POINTER(c_char_p)]),
        "eggdb_stmt_step": (c_int, [c_void_p, POINTER(c_int)]),
        "eggdb_stmt_row": (c_int, [c_void_p, POINTER(POINTER(_Value)), POINTER(c_size_t)]),
        "eggdb_stmt_finalize": (c_int, [c_void_p]),
        "eggdb_sql_copy_in_begin": (c_int, [c_void_p, c_char_p, POINTER(c_void_p)]),
        "eggdb_sql_copy_out_begin": (c_int, [c_void_p, c_char_p, POINTER(c_void_p)]),
        "eggdb_copy_put": (c_int, [c_void_p, c_char_p, c_size_t]),
        "eggdb_copy_read": (c_int, [c_void_p, POINTER(POINTER(c_uint8)), POINTER(c_size_t)]),
        "eggdb_copy_end": (c_int, [c_void_p, POINTER(POINTER(_Result)), POINTER(c_size_t)]),
        "eggdb_copy_free": (None, [c_void_p]),
    }
    for name, (restype, argtypes) in prototypes.items():
        function = getattr(library, name)
        function.restype = restype
        function.argtypes = argtypes
    _library = library
    return library


def _native_part():
    """The C extension `eggdb._native` loaded on the same C library, or None when it is not built.

    `EGGDB_PYTHON_NATIVE=0` turns it off; `EGGDB_PYTHON_NATIVE=1` makes a missing one an error."""
    global _native, _native_checked
    if _native_checked:
        return _native
    _api()
    _native_checked = True
    wanted = os.environ.get("EGGDB_PYTHON_NATIVE", "")
    if wanted == "0":
        return None
    try:
        from . import _native as native
    except ImportError:
        if wanted == "1":
            raise
        return None
    native.load(_library_file, _native_error)
    _native = native
    return native


def _native_error(status, message, sqlstate):
    return _error_for(status, message, sqlstate or None)


def _raise_last(status):
    api = _api()
    message = (api.eggdb_last_error() or b"").decode("utf-8", "replace")
    sqlstate = (api.eggdb_last_sqlstate() or b"").decode("ascii", "replace") or None
    raise _error_for(status, message, sqlstate)


def _error_for(status, message, sqlstate):
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
    return kind(message or f"EggDB status {status}", sqlstate)


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


def _numbered(sql, dialect):
    return _placeholders(sql, dialect, [])[0]


_INT64_MIN, _INT64_MAX = -(2**63), 2**63 - 1


def _bind(api, statement, index, value, dialect):
    if value is None:
        return api.eggdb_stmt_bind_null(statement, index)
    if isinstance(value, bool):
        return api.eggdb_stmt_bind_bool(statement, index, 1 if value else 0)
    if isinstance(value, int) and _INT64_MIN <= value <= _INT64_MAX:
        return api.eggdb_stmt_bind_int64(statement, index, value)
    if isinstance(value, float):
        return api.eggdb_stmt_bind_double(statement, index, value)
    if _is_bytes(value):
        data = bytes(value)
        return api.eggdb_stmt_bind_blob(statement, index, data, len(data))
    text = _parameter_text(value, dialect).encode("utf-8")
    return api.eggdb_stmt_bind_text(statement, index, text, len(text))


def _gathered(results, count):
    gathered = []
    for index in range(count):
        result = results[index]
        names = [result.column_names[column].decode("utf-8") for column in range(result.column_count)]
        rows = []
        for row in range(result.row_count):
            start = row * result.column_count
            rows.append(tuple(_converted(result.values[start + column]) for column in range(result.column_count)))
        gathered.append(((result.tag or b"").decode("utf-8"), names, rows))
    return gathered


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
        native = _native_part()
        if native is not None:
            return native.execute(self._session.value, sql.encode("utf-8"), encoded)
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
        return _gathered(results, count.value)

    def _live_session(self):
        if self._session is None:
            raise InterfaceError("the connection is closed")
        return self._session

    def prepare(self, sql):
        """Checks one statement once and returns a `Statement` that runs it as often as needed,
        with `?` placeholders bound to typed values."""
        return Statement(self, sql)

    def iterate(self, sql, parameters=()):
        """Runs one statement and yields its rows one at a time, stepping through them: from a
        server each row is read from the connection as it is needed."""
        self._begin_if_needed()
        statement = self.prepare(sql)
        try:
            yield from statement.rows(parameters)
        finally:
            statement.close()

    def copy_from(self, sql, data):
        """Runs `COPY ... FROM STDIN` with `data`: bytes, str, or an iterable of bytes or str pieces
        cut anywhere. Returns the number of rows loaded."""
        self._begin_if_needed()
        native = _native_part()
        if native is not None:
            pieces = [data] if isinstance(data, (bytes, bytearray, memoryview, str)) else data
            return _copied_rows(native.copy_in(self._live_session().value, sql.encode("utf-8"), pieces))
        api = _api()
        copy = c_void_p()
        _checked(api.eggdb_sql_copy_in_begin(self._live_session(), sql.encode("utf-8"), byref(copy)))
        try:
            pieces = [data] if isinstance(data, (bytes, bytearray, memoryview, str)) else data
            for piece in pieces:
                chunk = piece.encode("utf-8") if isinstance(piece, str) else bytes(piece)
                if chunk:
                    _checked(api.eggdb_copy_put(copy, chunk, len(chunk)))
            return _copied_rows(_copy_end(api, copy))
        finally:
            api.eggdb_copy_free(copy)

    def copy_to(self, sql):
        """Runs `COPY ... TO STDOUT` and yields its data in pieces of bytes as they are read."""
        self._begin_if_needed()
        native = _native_part()
        if native is not None:
            pieces = []
            native.copy_out(self._live_session().value, sql.encode("utf-8"), pieces.append)
            yield from pieces
            return
        api = _api()
        copy = c_void_p()
        _checked(api.eggdb_sql_copy_out_begin(self._live_session(), sql.encode("utf-8"), byref(copy)))
        try:
            while True:
                data = POINTER(c_uint8)()
                length = c_size_t()
                _checked(api.eggdb_copy_read(copy, byref(data), byref(length)))
                if length.value == 0:
                    break
                yield ctypes.string_at(data, length.value)
            _copy_end(api, copy)
        finally:
            api.eggdb_copy_free(copy)

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
                self.rowcount = _affected(tag)
        return self

    def executemany(self, sql, sequence):
        """Runs one statement for each set of parameters, prepared once."""
        self.connection._begin_if_needed()
        statement = self.connection.prepare(sql)
        try:
            total = statement.executemany(sequence)
        finally:
            statement.close()
        self.description = None
        self._rows = []
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


def _affected(tag):
    words = tag.split()
    if words and words[0] in ("INSERT", "UPDATE", "DELETE", "MERGE") and words[-1].isdigit():
        return int(words[-1])
    return -1


def _copy_end(api, copy):
    results = POINTER(_Result)()
    count = c_size_t()
    _checked(api.eggdb_copy_end(copy, byref(results), byref(count)))
    return _gathered(results, count.value)


def _copied_rows(gathered):
    for tag, _names, _rows in reversed(gathered):
        words = tag.split()
        if words and words[0] == "COPY" and words[-1].isdigit():
            return int(words[-1])
    return -1


class Statement:
    """One statement checked once, run many times with typed bindings or stepped a row at a time."""

    def __init__(self, connection, sql):
        api = _api()
        text = _numbered(sql, connection._dialect).encode("utf-8")
        self.connection = connection
        self._native = None
        native = _native_part()
        if native is not None:
            self._native = native.Statement(connection._live_session().value, text)
            self._handle = None
            self.parameter_count = self._native.parameter_count
            return
        handle = c_void_p()
        _checked(api.eggdb_sql_prepare(connection._live_session(), text, len(text), byref(handle)))
        self.connection = connection
        self._handle = handle
        count = c_size_t()
        _checked(api.eggdb_stmt_parameter_count(handle, byref(count)))
        self.parameter_count = count.value

    def _live(self):
        if self._handle is None:
            raise InterfaceError("the statement is closed")
        return self._handle

    def _bind_all(self, parameters):
        api = _api()
        handle = self._live()
        _checked(api.eggdb_stmt_reset(handle))
        _checked(api.eggdb_stmt_clear_bindings(handle))
        values = list(parameters)
        if len(values) > self.parameter_count:
            raise ProgrammingError(f"{len(values)} parameters given for {self.parameter_count} placeholders")
        for index, value in enumerate(values, start=1):
            _checked(_bind(api, handle, index, value, self.connection._dialect))

    def _open_native(self):
        if self._native is None:
            raise InterfaceError("the statement is closed")
        return self._native

    def execute(self, parameters=()):
        """Runs the statement with `parameters` and returns its results as (tag, column names, rows)."""
        if self._native is not None or self._handle is None:
            return self._open_native().execute(list(parameters))
        self._bind_all(parameters)
        results = POINTER(_Result)()
        count = c_size_t()
        _checked(_api().eggdb_stmt_execute(self._handle, byref(results), byref(count)))
        return _gathered(results, count.value)

    def columns(self):
        """The names of the columns the statement returns, when they are known."""
        if self._native is not None or self._handle is None:
            return self._open_native().columns()
        api = _api()
        count = c_size_t()
        _checked(api.eggdb_stmt_column_count(self._live(), byref(count)))
        names = []
        for column in range(count.value):
            name = c_char_p()
            _checked(api.eggdb_stmt_column_name(self._handle, column, byref(name)))
            names.append((name.value or b"").decode("utf-8"))
        return names

    def rows(self, parameters=()):
        """Runs the statement with `parameters` and yields its rows one at a time."""
        if self._native is not None or self._handle is None:
            native = self._open_native()
            try:
                yield from native.start(list(parameters))
            finally:
                if self._native is not None:
                    native.reset()
            return
        self._bind_all(parameters)
        api = _api()
        handle = self._handle
        try:
            while True:
                has_row = c_int()
                _checked(api.eggdb_stmt_step(handle, byref(has_row)))
                if not has_row.value:
                    return
                values = POINTER(_Value)()
                count = c_size_t()
                _checked(api.eggdb_stmt_row(handle, byref(values), byref(count)))
                yield tuple(_converted(values[column]) for column in range(count.value))
        finally:
            if self._handle is not None:
                api.eggdb_stmt_reset(self._handle)

    def executemany(self, rows):
        """Runs the statement once per row of parameters and returns the number of rows changed;
        with the C extension the whole loop runs in C."""
        if self._native is not None or self._handle is None:
            return self._open_native().executemany(rows)
        total = 0
        for parameters in rows:
            gathered = self.execute(parameters)
            if gathered:
                total += max(_affected(gathered[-1][0]), 0)
        return total

    def execute_columns(self, columns):
        """Runs the statement once per row of parameter columns, one column per placeholder: each a
        one-dimensional buffer of numbers (`array.array`, a NumPy array, a `memoryview`) or a
        sequence. Returns the number of rows changed. With the C extension the buffers are read in
        place, without a Python object per value."""
        if self._native is not None or self._handle is None:
            return self._open_native().execute_columns(list(columns))
        columns = [memoryview(column).tolist() if not isinstance(column, (list, tuple)) else column for column in columns]
        return self.executemany(zip(*columns))

    def fetch_columns(self, parameters=()):
        """Runs the statement and returns its result by column name: an `array.array('q')` or
        `array.array('d')` for a column that holds only integers or only reals, a list otherwise."""
        if self._native is not None or self._handle is None:
            return self._open_native().fetch_columns(list(parameters))
        import array

        names = self.columns()
        values = [[] for _ in names]
        for row in self.rows(parameters):
            for column, value in enumerate(row):
                values[column].append(value)
        out = {}
        for name, column in zip(names, values):
            if column and all(type(value) is int for value in column):
                out[name] = array.array("q", column)
            elif column and all(type(value) is float for value in column):
                out[name] = array.array("d", column)
            else:
                out[name] = column
        return out

    def close(self):
        """Frees the statement."""
        if self._native is not None:
            self._native.close()
            self._native = None
        if self._handle is not None:
            _api().eggdb_stmt_finalize(self._handle)
            self._handle = None

    def __enter__(self):
        return self

    def __exit__(self, kind, value, traceback):
        self.close()
        return False


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
