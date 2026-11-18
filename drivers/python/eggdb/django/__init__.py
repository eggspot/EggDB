"""EggDB's Django backend: ``"ENGINE": "eggdb.django"`` runs Django's ORM, migrations and admin on
an embedded EggDB file through this package's DB-API driver, in EggDB's SQLite dialect, so
Django's own SQLite backend (its SQL compiler, schema editor and introspection) applies unchanged.
"""
