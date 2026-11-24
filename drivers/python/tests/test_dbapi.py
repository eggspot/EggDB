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

    def test_prepared_statements_bind_typed_values_and_step_through_rows(self):
        connection = eggdb.connect(":memory:")
        connection.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL, photo BLOB)")
        with connection.prepare("INSERT INTO t VALUES (?, ?, ?, ?)") as insert:
            self.assertEqual(insert.parameter_count, 4)
            for number in range(1, 4):
                insert.execute((number, f"n{number}", number / 2, bytes([number])))
        self.assertEqual(
            list(connection.iterate("SELECT id, name, score, photo FROM t WHERE id >= ? ORDER BY id", (2,))),
            [(2, "n2", 1.0, b"\x02"), (3, "n3", 1.5, b"\x03")],
        )
        with connection.prepare("SELECT name FROM t WHERE id = ?") as select:
            self.assertEqual(select.columns(), ["name"])
            self.assertEqual(list(select.rows((1,))), [("n1",)])
            self.assertEqual(list(select.rows((3,))), [("n3",)])
        connection.close()

    def test_columns_of_parameters_and_results_move_as_buffers(self):
        import array

        connection = eggdb.connect(":memory:")
        connection.execute("CREATE TABLE m (id INTEGER PRIMARY KEY, reading REAL, label TEXT)")
        with connection.prepare("INSERT INTO m VALUES (?, ?, ?)") as insert:
            changed = insert.execute_columns(
                [array.array("q", [1, 2, 3]), array.array("d", [0.5, 1.5, 2.5]), ["a", None, "c"]]
            )
            self.assertEqual(changed, 3)
            self.assertEqual(insert.executemany([(4, 3.5, "d"), (5, 4.5, "e")]), 2)
        with connection.prepare("SELECT id, reading, label FROM m ORDER BY id") as select:
            columns = select.fetch_columns()
        self.assertEqual(columns["id"], array.array("q", [1, 2, 3, 4, 5]))
        self.assertEqual(columns["reading"], array.array("d", [0.5, 1.5, 2.5, 3.5, 4.5]))
        self.assertEqual(columns["label"], ["a", None, "c", "d", "e"])
        connection.close()

    def test_copy_loads_and_reads_rows_in_pieces(self):
        connection = eggdb.connect(":memory:", dialect="postgresql")
        connection.execute("CREATE TABLE t (id integer, name text)")
        self.assertEqual(connection.copy_from("COPY t FROM STDIN", ["1\tad", "a\n2\tgr", "ace\n"]), 2)
        self.assertEqual(b"".join(connection.copy_to("COPY t TO STDOUT")), b"1\tada\n2\tgrace\n")
        connection.close()

    def test_functions_defined_in_python_run_in_sql(self):
        connection = eggdb.connect(":memory:")
        connection.create_function("twice", 1, lambda value: None if value is None else value * 2, deterministic=True)
        connection.create_function("fail", 0, lambda: 1 / 0)
        cursor = connection.cursor()
        cursor.execute("SELECT twice(21), twice('ab'), twice(NULL), twice(1.5)")
        self.assertEqual(cursor.fetchone(), (42, "abab", None, 3.0))
        with self.assertRaises(eggdb.Error) as raised:
            cursor.execute("SELECT fail()")
        self.assertIn("division by zero", str(raised.exception))
        connection.create_function("twice", 1, None)
        with self.assertRaises(eggdb.Error):
            cursor.execute("SELECT twice(1)")
        connection.close()

    def test_aggregates_defined_in_python_run_per_group(self):
        class Longest:
            def __init__(self):
                self.longest = None

            def step(self, word):
                if not isinstance(word, str):
                    raise ValueError("longest takes text")
                if self.longest is None or len(word) > len(self.longest):
                    self.longest = word

            def finalize(self):
                return self.longest

        connection = eggdb.connect(":memory:")
        connection.create_aggregate("longest", 1, Longest)
        cursor = connection.cursor()
        cursor.execute("CREATE TABLE words (team TEXT, word TEXT)")
        cursor.executemany("INSERT INTO words VALUES (?, ?)", [("a", "egg"), ("a", "omelette"), ("b", "yolk")])
        cursor.execute("SELECT team, longest(word) FROM words GROUP BY team ORDER BY team")
        self.assertEqual(cursor.fetchall(), [("a", "omelette"), ("b", "yolk")])
        cursor.execute("SELECT longest(word) FROM words WHERE team = 'none'")
        self.assertEqual(cursor.fetchone(), (None,))
        with self.assertRaises(eggdb.Error) as raised:
            cursor.execute("SELECT longest(1) FROM words")
        self.assertIn("longest takes text", str(raised.exception))
        connection.create_aggregate("longest", 1, None)
        with self.assertRaises(eggdb.Error):
            cursor.execute("SELECT longest(word) FROM words")
        connection.close()

    def test_sqlite_conveniences_scripts_rowids_and_change_counts(self):
        connection = eggdb.connect(":memory:")
        connection.executescript("CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes (body) VALUES ('a');")
        cursor = connection.cursor()
        cursor.execute("INSERT INTO notes (body) VALUES (?)", ("b",))
        self.assertEqual(cursor.lastrowid, 2)
        self.assertTrue(connection.in_transaction)
        connection.commit()
        self.assertFalse(connection.in_transaction)
        self.assertEqual(connection.total_changes, 2)
        cursor.execute("SELECT count(*) FROM notes")
        self.assertIsNone(cursor.lastrowid)
        connection.close()

    def test_row_factories_make_rows_read_by_name(self):
        connection = eggdb.connect(":memory:")
        connection.row_factory = eggdb.Row
        cursor = connection.cursor()
        cursor.execute("SELECT 1 AS id, 'egg' AS Name")
        row = cursor.fetchone()
        self.assertEqual((row[0], row["name"], row["NAME"], row.keys()), (1, "egg", "egg", ["id", "Name"]))
        cursor.row_factory = lambda cursor, values: values[1]
        cursor.execute("SELECT 1, 'two'")
        self.assertEqual(cursor.fetchall(), ["two"])
        connection.close()


class KeyValueTest(unittest.TestCase):
    def test_collections_hold_keys_apart_and_rename_and_count(self):
        with tempfile.TemporaryDirectory() as directory:
            with eggdb.open_key_value(os.path.join(directory, "kv.db")) as store:
                with store.write() as transaction:
                    self.assertTrue(transaction.create_collection("sessions"))
                    transaction.put("s:1", b"ada", collection="sessions")
                    transaction.put("s:2", "grace", collection="sessions")
                    transaction.put("plain", b"default")
                with store.read() as transaction:
                    self.assertEqual(transaction.get("s:1", collection="sessions"), b"ada")
                    self.assertIsNone(transaction.get("s:1"))
                    self.assertEqual([key for key, _ in transaction.scan(prefix="s:", collection="sessions")], [b"s:1", b"s:2"])
                    self.assertEqual(transaction.collection_stats("sessions")["entries"], 2)
                with store.write() as transaction:
                    transaction.rename_collection("sessions", "logins")
                    self.assertTrue(transaction.delete("s:2", collection="logins"))
                with store.read() as transaction:
                    self.assertEqual(transaction.collections(), ["logins"])
                    self.assertEqual(transaction.get("s:1", collection="logins"), b"ada")


if __name__ == "__main__":
    unittest.main()
