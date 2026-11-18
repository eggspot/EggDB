import asyncio
import unittest

import eggdb.aio


class AsyncioTest(unittest.TestCase):
    def test_statements_run_without_blocking_the_loop(self):
        async def scenario():
            async with await eggdb.aio.connect(":memory:") as connection:
                await connection.execute("CREATE TABLE eggs (id INTEGER PRIMARY KEY, colour TEXT)")
                cursor = await connection.cursor()
                await cursor.executemany("INSERT INTO eggs (colour) VALUES (?)", [("brown",), ("white",)])
                await connection.commit()
                cursor = await connection.execute("SELECT colour FROM eggs ORDER BY id")
                colours = [row[0] async for row in cursor]
                ticks = await asyncio.gather(asyncio.sleep(0, result="tick"), connection.execute("SELECT 1"))
                return colours, ticks[0]

        colours, tick = asyncio.run(scenario())
        self.assertEqual(colours, ["brown", "white"])
        self.assertEqual(tick, "tick")

    def test_functions_scripts_and_row_factories_from_asyncio(self):
        async def scenario():
            async with await eggdb.aio.connect(":memory:") as connection:
                await connection.create_function("twice", 1, lambda value: value * 2)
                await connection.executescript("CREATE TABLE eggs (id INTEGER PRIMARY KEY); INSERT INTO eggs VALUES (21);")
                connection.row_factory = eggdb.Row
                cursor = await connection.execute("SELECT twice(id) AS doubled FROM eggs")
                row = await cursor.fetchone()
                return row["doubled"]

        self.assertEqual(asyncio.run(scenario()), 42)


if __name__ == "__main__":
    unittest.main()
