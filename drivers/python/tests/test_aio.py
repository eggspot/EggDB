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


if __name__ == "__main__":
    unittest.main()
