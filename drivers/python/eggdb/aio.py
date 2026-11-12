"""asyncio support for EggDB's Python driver: ``await eggdb.aio.connect(path)`` gives an
``AsyncConnection`` whose calls run on one worker thread of its own, so the event loop never
waits on the database and the DB-API connection, which is not shared between threads
(``threadsafety = 1``), is only ever used from that thread.
"""

import asyncio
from concurrent.futures import ThreadPoolExecutor

import eggdb


class AsyncCursor:
    """The rows of one statement run through an ``AsyncConnection``."""

    def __init__(self, connection, cursor):
        self._connection = connection
        self._cursor = cursor

    @property
    def description(self):
        return self._cursor.description

    @property
    def rowcount(self):
        return self._cursor.rowcount

    async def execute(self, sql, parameters=()):
        await self._connection._run(self._cursor.execute, sql, parameters)
        return self

    async def executemany(self, sql, sequence):
        await self._connection._run(self._cursor.executemany, sql, list(sequence))
        return self

    async def fetchone(self):
        return await self._connection._run(self._cursor.fetchone)

    async def fetchmany(self, size=None):
        return await self._connection._run(self._cursor.fetchmany, size)

    async def fetchall(self):
        return await self._connection._run(self._cursor.fetchall)

    def __aiter__(self):
        return self

    async def __anext__(self):
        row = await self.fetchone()
        if row is None:
            raise StopAsyncIteration
        return row

    async def close(self):
        await self._connection._run(self._cursor.close)


class AsyncConnection:
    """An EggDB connection used from asyncio code."""

    def __init__(self, executor, connection):
        self._executor = executor
        self._connection = connection

    async def _run(self, call, *arguments):
        loop = asyncio.get_running_loop()
        return await loop.run_in_executor(self._executor, lambda: call(*arguments))

    @property
    def autocommit(self):
        return self._connection.autocommit

    async def set_autocommit(self, enabled):
        def switch():
            self._connection.autocommit = enabled

        await self._run(switch)

    async def cursor(self):
        """A new cursor."""
        cursor = await self._run(self._connection.cursor)
        return AsyncCursor(self, cursor)

    async def execute(self, sql, parameters=()):
        """Runs `sql` on a new cursor and returns it."""
        cursor = await self.cursor()
        return await cursor.execute(sql, parameters)

    async def commit(self):
        await self._run(self._connection.commit)

    async def rollback(self):
        await self._run(self._connection.rollback)

    async def close(self):
        """Rolls back an open transaction, closes the connection and stops its thread."""
        try:
            await self._run(self._connection.close)
        finally:
            self._executor.shutdown(wait=False)

    async def __aenter__(self):
        return self

    async def __aexit__(self, kind, value, traceback):
        if kind is None:
            await self.commit()
        else:
            await self.rollback()
        await self.close()
        return False


async def connect(path, dialect="sqlite", read_only=False):
    """Opens the database at `path` as ``eggdb.connect`` does, on a worker thread of its own."""
    executor = ThreadPoolExecutor(max_workers=1, thread_name_prefix="eggdb-aio")
    loop = asyncio.get_running_loop()
    try:
        connection = await loop.run_in_executor(
            executor, lambda: eggdb.connect(path, dialect=dialect, read_only=read_only)
        )
    except BaseException:
        executor.shutdown(wait=False)
        raise
    return AsyncConnection(executor, connection)
