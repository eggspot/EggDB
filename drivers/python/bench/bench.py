import os
import sqlite3
import sys
import tempfile
import time

import eggdb

ROWS = int(os.environ.get("DRIVER_BENCH_ROWS", "20000"))
LOOKUPS = int(os.environ.get("DRIVER_BENCH_LOOKUPS", "5000"))


def timed(work):
    started = time.perf_counter()
    work()
    return (time.perf_counter() - started) * 1000


def run(connection, prepare_insert, prepare_lookup, scan):
    connection.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)")
    rows = [(at, f"name {at}", at * 0.25) for at in range(1, ROWS + 1)]
    insert = timed(lambda: prepare_insert(rows))
    keys = [(at * 7 % ROWS + 1,) for at in range(1, LOOKUPS + 1)]
    lookup = timed(lambda: prepare_lookup(keys))
    scanned = timed(scan)
    return insert, lookup, scanned


def eggdb_side(path):
    connection = eggdb.connect(path)

    def insert(rows):
        connection.cursor().executemany("INSERT INTO t VALUES (?, ?, ?)", rows)
        connection.commit()

    def lookup(keys):
        with connection.prepare("SELECT name FROM t WHERE id = ?") as statement:
            for key in keys:
                for _row in statement.rows(key):
                    pass

    def scan():
        total = 0
        for row in connection.iterate("SELECT name FROM t"):
            total += len(row[0])

    try:
        return run(connection, insert, lookup, scan)
    finally:
        connection.close()


def sqlite_side(path):
    connection = sqlite3.connect(path)

    def insert(rows):
        connection.executemany("INSERT INTO t VALUES (?, ?, ?)", rows)
        connection.commit()

    def lookup(keys):
        for key in keys:
            connection.execute("SELECT name FROM t WHERE id = ?", key).fetchall()

    def scan():
        total = 0
        for row in connection.execute("SELECT name FROM t"):
            total += len(row[0])

    try:
        return run(connection, insert, lookup, scan)
    finally:
        connection.close()


def main():
    with tempfile.TemporaryDirectory() as directory:
        egg = eggdb_side(os.path.join(directory, "bench.eggdb"))
        lite = sqlite_side(os.path.join(directory, "bench.sqlite"))
    for workload, mine, theirs in zip(("insert", "lookup", "scan"), egg, lite):
        print(f"python {workload} {mine:.2f} {theirs:.2f}")


if __name__ == "__main__":
    sys.exit(main())
