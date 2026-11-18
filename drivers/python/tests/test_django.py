import importlib.util
import unittest

DJANGO = importlib.util.find_spec("django") is not None


@unittest.skipUnless(DJANGO, "Django is not installed")
class DjangoBackendTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        import django
        from django.conf import settings

        if not settings.configured:
            settings.configure(
                DATABASES={"default": {"ENGINE": "eggdb.django", "NAME": ":memory:"}},
                INSTALLED_APPS=[],
                USE_TZ=False,
            )
        django.setup()

    def test_queries_functions_and_transactions_run_on_eggdb(self):
        from django.db import connection, transaction

        with connection.cursor() as cursor:
            cursor.execute("SELECT %s + 1", [1])
            self.assertEqual(cursor.fetchone(), (2,))
            cursor.execute("SELECT django_date_extract(%s, %s)", ["year", "2024-05-06"])
            self.assertEqual(cursor.fetchone(), (2024,))
            cursor.execute("CREATE TABLE eggs (id INTEGER PRIMARY KEY, colour TEXT)")
        try:
            with transaction.atomic():
                with connection.cursor() as cursor:
                    cursor.execute("INSERT INTO eggs (colour) VALUES (%(colour)s)", {"colour": "brown"})
                raise RuntimeError("undo")
        except RuntimeError:
            pass
        with transaction.atomic():
            with connection.cursor() as cursor:
                cursor.executemany("INSERT INTO eggs (colour) VALUES (%s)", [["white"], ["blue"]])
        with connection.cursor() as cursor:
            cursor.execute("SELECT colour FROM eggs ORDER BY id")
            self.assertEqual(cursor.fetchall(), [("white",), ("blue",)])
        self.assertEqual(connection.display_name, "EggDB")


if __name__ == "__main__":
    unittest.main()
