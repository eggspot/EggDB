import ctypes
import os
import sys
import tempfile
from collections import namedtuple
from ctypes import (
    POINTER,
    Structure,
    byref,
    c_char_p,
    c_double,
    c_int,
    c_int32,
    c_int64,
    c_size_t,
    c_uint8,
    c_uint32,
    c_uint64,
    c_void_p,
)
from pathlib import Path

EGGDB_OK = 0
EGGDB_ERR_IN_USE = 101
EGGDB_SCAN_REVERSE = 1
EGGDB_SCAN_END_INCLUSIVE = 2
EGGDB_SCAN_PREFIX = 4
EGGDB_NULL = 0
EGGDB_INTEGER = 1
EGGDB_REAL = 2
EGGDB_TEXT = 3
EGGDB_BLOB = 4

ByteBuffer = POINTER(c_uint8)


class Options(Structure):
    _fields_ = [("flags", c_uint32), ("durability", c_uint32), ("cache_bytes", c_uint64)]


class BackupReport(Structure):
    _fields_ = [
        ("stamp", c_uint64),
        ("entries", c_uint64),
        ("key_bytes", c_uint64),
        ("value_bytes", c_uint64),
        ("page_count", c_uint64),
        ("file_bytes", c_uint64),
    ]


class CompactReport(Structure):
    _fields_ = [
        ("stamp", c_uint64),
        ("entries", c_uint64),
        ("pages_before", c_uint64),
        ("free_pages_before", c_uint64),
        ("leaked_pages_before", c_uint64),
        ("pages_after", c_uint64),
        ("bytes_before", c_uint64),
        ("bytes_after", c_uint64),
    ]


class Entry(Structure):
    _fields_ = [
        ("key", ByteBuffer),
        ("key_len", c_size_t),
        ("value", ByteBuffer),
        ("value_len", c_size_t),
    ]


class Value(Structure):
    _fields_ = [
        ("kind", c_int32),
        ("integer", c_int64),
        ("real", c_double),
        ("data", ByteBuffer),
        ("len", c_size_t),
    ]


class Parameter(Structure):
    _fields_ = [("text", c_char_p), ("len", c_size_t)]


class Result(Structure):
    _fields_ = [
        ("tag", c_char_p),
        ("column_count", c_size_t),
        ("column_names", POINTER(c_char_p)),
        ("row_count", c_size_t),
        ("values", POINTER(Value)),
    ]


SqlResult = namedtuple("SqlResult", ["tag", "columns", "rows"])

PROTOTYPES = {
    "eggdb_version": (c_char_p, []),
    "eggdb_last_error": (c_char_p, []),
    "eggdb_last_sqlstate": (c_char_p, []),
    "eggdb_status_name": (c_char_p, [c_int]),
    "eggdb_free": (None, [ByteBuffer, c_size_t]),
    "eggdb_open": (c_int, [c_char_p, POINTER(Options), POINTER(c_void_p)]),
    "eggdb_close": (c_int, [c_void_p]),
    "eggdb_backup": (c_int, [c_void_p, c_char_p, POINTER(BackupReport)]),
    "eggdb_compact": (c_int, [c_void_p, POINTER(CompactReport)]),
    "eggdb_compact_file": (c_int, [c_char_p, POINTER(CompactReport)]),
    "eggdb_remove_expired": (c_int, [c_void_p, POINTER(c_uint64)]),
    "eggdb_begin_read": (c_int, [c_void_p, POINTER(c_void_p)]),
    "eggdb_begin_write": (c_int, [c_void_p, POINTER(c_void_p)]),
    "eggdb_commit": (c_int, [c_void_p, POINTER(c_uint64)]),
    "eggdb_rollback": (c_int, [c_void_p]),
    "eggdb_get": (
        c_int,
        [c_void_p, c_char_p, c_char_p, c_size_t, POINTER(ByteBuffer), POINTER(c_size_t)],
    ),
    "eggdb_put": (c_int, [c_void_p, c_char_p, c_char_p, c_size_t, c_char_p, c_size_t]),
    "eggdb_put_with_expiry": (
        c_int,
        [c_void_p, c_char_p, c_char_p, c_size_t, c_char_p, c_size_t, c_uint64],
    ),
    "eggdb_delete": (c_int, [c_void_p, c_char_p, c_char_p, c_size_t, POINTER(c_int)]),
    "eggdb_time_to_live": (c_int, [c_void_p, c_char_p, c_char_p, c_size_t, POINTER(c_int64)]),
    "eggdb_increment": (
        c_int,
        [c_void_p, c_char_p, c_char_p, c_size_t, c_int64, POINTER(c_int64)],
    ),
    "eggdb_create_collection": (c_int, [c_void_p, c_char_p, POINTER(c_int)]),
    "eggdb_drop_collection": (c_int, [c_void_p, c_char_p, POINTER(c_int)]),
    "eggdb_collection_names": (
        c_int,
        [c_void_p, POINTER(ByteBuffer), POINTER(c_size_t), POINTER(c_size_t)],
    ),
    "eggdb_scan_open": (
        c_int,
        [c_void_p, c_char_p, c_char_p, c_size_t, c_char_p, c_size_t, c_uint32, POINTER(c_void_p)],
    ),
    "eggdb_scan_next": (
        c_int,
        [c_void_p, c_void_p, c_size_t, POINTER(POINTER(Entry)), POINTER(c_size_t)],
    ),
    "eggdb_scan_free": (None, [c_void_p]),
    "eggdb_sql_open": (c_int, [c_void_p, c_char_p, POINTER(c_void_p)]),
    "eggdb_sql_execute": (
        c_int,
        [c_void_p, c_char_p, POINTER(POINTER(Result)), POINTER(c_size_t)],
    ),
    "eggdb_sql_execute_with_parameters": (
        c_int,
        [
            c_void_p,
            c_char_p,
            POINTER(Parameter),
            c_size_t,
            POINTER(POINTER(Result)),
            POINTER(c_size_t),
        ],
    ),
    "eggdb_sql_close": (c_int, [c_void_p]),
}


class EggDBError(Exception):
    def __init__(self, status, name, message, sqlstate):
        super().__init__(f"{name}: {message}")
        self.status = status
        self.name = name
        self.message = message
        self.sqlstate = sqlstate


def library_file_name():
    if sys.platform == "win32":
        return "eggdb_capi.dll"
    if sys.platform == "darwin":
        return "libeggdb_capi.dylib"
    return "libeggdb_capi.so"


def find_library(given=None):
    if given:
        return given
    if os.environ.get("EGGDB_LIBRARY"):
        return os.environ["EGGDB_LIBRARY"]
    repository = Path(__file__).resolve().parents[2]
    for profile in ("release", "debug"):
        candidate = repository / "target" / profile / library_file_name()
        if candidate.exists():
            return str(candidate)
    raise SystemExit(
        f"no {library_file_name()} found: build it with `cargo build --release -p eggdb-capi`, "
        "or name it with --library or EGGDB_LIBRARY"
    )


def load_library(path):
    library = ctypes.CDLL(path)
    for name, (restype, argtypes) in PROTOTYPES.items():
        function = getattr(library, name)
        function.restype = restype
        function.argtypes = argtypes
    return library


def optional_text(text):
    return None if text is None else text.encode()


def as_bytes(data):
    return data.encode() if isinstance(data, str) else bytes(data)


def parameter_text(value):
    if value is None:
        return None
    if isinstance(value, bool):
        return b"true" if value else b"false"
    if isinstance(value, (bytes, bytearray)):
        return b"\\x" + bytes(value).hex().encode()
    return str(value).encode()


def python_value(value):
    if value.kind == EGGDB_NULL:
        return None
    if value.kind == EGGDB_INTEGER:
        return value.integer
    if value.kind == EGGDB_REAL:
        return value.real
    data = ctypes.string_at(value.data, value.len)
    if value.kind == EGGDB_BLOB:
        return data
    return data.decode()


class EggDB:
    def __init__(self, path, library=None, read_only=False):
        self.library = load_library(find_library(library))
        options = Options(1 if read_only else 0, 0, 0)
        self.handle = c_void_p()
        self.check(self.library.eggdb_open(os.fsencode(path), byref(options), byref(self.handle)))

    def check(self, status):
        if status == EGGDB_OK:
            return
        library = self.library
        raise EggDBError(
            status,
            library.eggdb_status_name(status).decode(),
            library.eggdb_last_error().decode(errors="replace"),
            library.eggdb_last_sqlstate().decode(),
        )

    def __enter__(self):
        return self

    def __exit__(self, *exception):
        self.close()

    def close(self):
        if self.handle:
            self.check(self.library.eggdb_close(self.handle))
            self.handle = c_void_p()

    def read(self):
        return self.begin(self.library.eggdb_begin_read)

    def write(self):
        return self.begin(self.library.eggdb_begin_write)

    def begin(self, start):
        handle = c_void_p()
        self.check(start(self.handle, byref(handle)))
        return Transaction(self, handle)

    def sql(self, dialect="postgresql"):
        handle = c_void_p()
        self.check(self.library.eggdb_sql_open(self.handle, dialect.encode(), byref(handle)))
        return SqlSession(self, handle)

    def backup(self, path):
        report = BackupReport()
        self.check(self.library.eggdb_backup(self.handle, os.fsencode(path), byref(report)))
        return report

    def compact(self):
        report = CompactReport()
        status = self.library.eggdb_compact(self.handle, byref(report))
        if status != EGGDB_ERR_IN_USE:
            self.handle = c_void_p()
        self.check(status)
        return report


class Transaction:
    def __init__(self, db, handle):
        self.db = db
        self.library = db.library
        self.handle = handle

    def __enter__(self):
        return self

    def __exit__(self, kind, error, trace):
        if not self.handle:
            return
        if kind is None:
            self.commit()
        else:
            self.rollback()

    def commit(self):
        stamp = c_uint64()
        handle, self.handle = self.handle, c_void_p()
        self.db.check(self.library.eggdb_commit(handle, byref(stamp)))
        return stamp.value

    def rollback(self):
        handle, self.handle = self.handle, c_void_p()
        self.db.check(self.library.eggdb_rollback(handle))

    def get(self, key, collection=None):
        key = as_bytes(key)
        value = ByteBuffer()
        length = c_size_t()
        self.db.check(
            self.library.eggdb_get(
                self.handle, optional_text(collection), key, len(key), byref(value), byref(length)
            )
        )
        if not value:
            return None
        try:
            return ctypes.string_at(value, length.value)
        finally:
            self.library.eggdb_free(value, length)

    def put(self, key, value, collection=None, ttl_ms=None):
        key, value = as_bytes(key), as_bytes(value)
        name = optional_text(collection)
        if ttl_ms is None:
            status = self.library.eggdb_put(self.handle, name, key, len(key), value, len(value))
        else:
            status = self.library.eggdb_put_with_expiry(
                self.handle, name, key, len(key), value, len(value), ttl_ms
            )
        self.db.check(status)

    def delete(self, key, collection=None):
        key = as_bytes(key)
        deleted = c_int()
        self.db.check(
            self.library.eggdb_delete(
                self.handle, optional_text(collection), key, len(key), byref(deleted)
            )
        )
        return bool(deleted.value)

    def time_to_live(self, key, collection=None):
        key = as_bytes(key)
        left = c_int64()
        self.db.check(
            self.library.eggdb_time_to_live(
                self.handle, optional_text(collection), key, len(key), byref(left)
            )
        )
        return None if left.value < 0 else left.value

    def increment(self, key, by=1, collection=None):
        key = as_bytes(key)
        count = c_int64()
        self.db.check(
            self.library.eggdb_increment(
                self.handle, optional_text(collection), key, len(key), by, byref(count)
            )
        )
        return count.value

    def create_collection(self, name):
        created = c_int()
        self.db.check(
            self.library.eggdb_create_collection(self.handle, name.encode(), byref(created))
        )
        return bool(created.value)

    def collection_names(self):
        names = ByteBuffer()
        length = c_size_t()
        count = c_size_t()
        self.db.check(
            self.library.eggdb_collection_names(
                self.handle, byref(names), byref(length), byref(count)
            )
        )
        try:
            joined = ctypes.string_at(names, length.value)
        finally:
            self.library.eggdb_free(names, length)
        return [name.decode() for name in joined.split(b"\0")[: count.value]]

    def scan(self, start=None, end=None, prefix=None, reverse=False, collection=None, batch=256):
        flags = EGGDB_SCAN_REVERSE if reverse else 0
        if prefix is not None:
            flags |= EGGDB_SCAN_PREFIX
            start = prefix
        start = None if start is None else as_bytes(start)
        end = None if end is None else as_bytes(end)
        scan = c_void_p()
        self.db.check(
            self.library.eggdb_scan_open(
                self.handle,
                optional_text(collection),
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
                entries = POINTER(Entry)()
                count = c_size_t()
                self.db.check(
                    self.library.eggdb_scan_next(
                        self.handle, scan, batch, byref(entries), byref(count)
                    )
                )
                if count.value == 0:
                    return
                copied = [
                    (
                        ctypes.string_at(entries[index].key, entries[index].key_len),
                        ctypes.string_at(entries[index].value, entries[index].value_len),
                    )
                    for index in range(count.value)
                ]
                yield from copied
        finally:
            self.library.eggdb_scan_free(scan)


class SqlSession:
    def __init__(self, db, handle):
        self.db = db
        self.library = db.library
        self.handle = handle

    def __enter__(self):
        return self

    def __exit__(self, *exception):
        self.close()

    def close(self):
        handle, self.handle = self.handle, c_void_p()
        if handle:
            self.db.check(self.library.eggdb_sql_close(handle))

    def execute(self, text, parameters=None):
        results = POINTER(Result)()
        count = c_size_t()
        if parameters is None:
            status = self.library.eggdb_sql_execute(
                self.handle, text.encode(), byref(results), byref(count)
            )
        else:
            texts = [parameter_text(value) for value in parameters]
            given = (Parameter * len(texts))(
                *[Parameter(text, 0 if text is None else len(text)) for text in texts]
            )
            status = self.library.eggdb_sql_execute_with_parameters(
                self.handle, text.encode(), given, len(texts), byref(results), byref(count)
            )
        self.db.check(status)
        return [self.read_result(results[index]) for index in range(count.value)]

    @staticmethod
    def read_result(result):
        columns = [result.column_names[index].decode() for index in range(result.column_count)]
        rows = []
        for row in range(result.row_count):
            first = row * result.column_count
            rows.append(
                [python_value(result.values[first + column]) for column in range(len(columns))]
            )
        return SqlResult(result.tag.decode(), columns, rows)


def expect(actual, expected):
    if actual != expected:
        raise SystemExit(f"expected {expected!r}, got {actual!r}")


def demo(library, directory):
    path = Path(directory) / "example.db"
    with EggDB(path, library) as db:
        print("EggDB", db.library.eggdb_version().decode(), "at", path)

        with db.write() as txn:
            txn.put("greeting", "hello")
            for number in range(5):
                txn.put(f"user:{number}", f"name {number}")
            txn.create_collection("sessions")
            txn.put("abc123", "alice", collection="sessions", ttl_ms=60_000)
            txn.increment("visits", 41)
            txn.increment("visits")

        with db.read() as txn:
            expect(txn.get("greeting"), b"hello")
            expect(txn.get("missing"), None)
            users = list(txn.scan(prefix="user:", batch=2))
            expect([key for key, _ in users], [f"user:{n}".encode() for n in range(5)])
            print("users:", [value.decode() for _, value in users])
            last = next(txn.scan(reverse=True))
            print("last key:", last[0].decode())
            expect(txn.get("visits"), b"42")
            expect(txn.collection_names(), ["sessions"])
            left = txn.time_to_live("abc123", collection="sessions")
            expect(left is not None and 0 < left <= 60_000, True)
            print("session expires in", left, "ms")

        with db.write() as txn:
            expect(txn.delete("user:0"), True)
            expect(txn.delete("user:0"), False)

        try:
            with db.write() as txn:
                txn.increment("greeting")
        except EggDBError as error:
            expect(error.name, "EGGDB_ERR_NOT_AN_INTEGER")
            print("refused:", error.message)

        with db.sql("postgresql") as sql:
            sql.execute("CREATE TABLE items (id integer PRIMARY KEY, name text, price numeric)")
            for item in [(1, "tea", "2.50"), (2, "O'Brien's pie", "4.25"), (3, "water", None)]:
                sql.execute("INSERT INTO items VALUES ($1, $2, $3)", list(item))
            (result,) = sql.execute(
                "SELECT id, name, price FROM items WHERE id <= $1 ORDER BY id", [2]
            )
            expect(result.columns, ["id", "name", "price"])
            expect(result.rows, [[1, "tea", "2.50"], [2, "O'Brien's pie", "4.25"]])
            print(result.tag + ":", result.rows)
            try:
                sql.execute("SELECT * FROM no_such_table")
            except EggDBError as error:
                expect(error.name, "EGGDB_ERR_SQL")
                print("SQLSTATE", error.sqlstate + ":", error.message)

        with db.sql("sqlite") as sql:
            (result,) = sql.execute("SELECT count(*) FROM items")
            expect(result.rows, [[3]])

        report = db.backup(Path(directory) / "example-copy.db")
        print("backup:", report.entries, "entries,", report.file_bytes, "bytes")
        compacted = db.compact()
        print("compacted:", compacted.pages_before, "->", compacted.pages_after, "pages")

    with EggDB(Path(directory) / "example-copy.db", library, read_only=True) as copy:
        with copy.read() as txn:
            expect(txn.get("greeting"), b"hello")
    print("example: everything worked")


def main(arguments):
    library = None
    if len(arguments) == 2 and arguments[0] == "--library":
        library = arguments[1]
    elif arguments:
        raise SystemExit("usage: eggdb_ctypes.py [--library <path to the EggDB C library>]")
    with tempfile.TemporaryDirectory() as directory:
        demo(library, directory)


if __name__ == "__main__":
    main(sys.argv[1:])
