import os
import tempfile
import unittest

import eggdb


class DbApiTest(unittest.TestCase):
    def test_rows_go_in_and_come_back_with_their_types(self):
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "app.db")
            connection = eggdb.connect(path)
            cursor = connection.cursor()
            cursor.execute("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL, photo BLOB)")
            cursor.executemany(
                "INSERT INTO people (name, score, photo) VALUES (?, ?, ?)",
                [("Ada", 1.5, b"\x00\xff"), ("Grace", None, None)],
            )
            self.assertEqual(cursor.rowcount, 2)
            connection.commit()
            cursor.execute("SELECT id, name, score, photo FROM people ORDER BY id")
            self.assertEqual([column[0] for column in cursor.description], ["id", "name", "score", "photo"])
            self.assertEqual(cursor.fetchone(), (1, "Ada", 1.5, b"\x00\xff"))
            self.assertEqual(cursor.fetchall(), [(2, "Grace", None, None)])
            connection.close()

    def test_rollback_and_errors(self):
        connection = eggdb.connect(":memory:")
        connection.execute("CREATE TABLE t (v INTEGER PRIMARY KEY)")
        connection.commit()
        connection.execute("INSERT INTO t VALUES (?)", (1,))
        connection.rollback()
        self.assertEqual(connection.execute("SELECT count(*) FROM t").fetchone(), (0,))
        connection.execute("INSERT INTO t VALUES (?)", (1,))
        with self.assertRaises(eggdb.IntegrityError):
            connection.execute("INSERT INTO t VALUES (?)", (1,))
        with self.assertRaises(eggdb.ProgrammingError):
            connection.execute("SELECT * FROM nowhere")
        connection.close()

    def test_postgresql_dialect_with_qmark_parameters(self):
        connection = eggdb.connect(":memory:", dialect="postgresql")
        connection.execute("CREATE TABLE t (v integer, w text)")
        connection.execute("INSERT INTO t VALUES (?, ?)", (7, "it's"))
        self.assertEqual(connection.execute("SELECT w FROM t WHERE v = ?", (7,)).fetchone(), ("it's",))
        connection.close()


if __name__ == "__main__":
    unittest.main()
