from superset.db_engine_specs.postgres import PostgresEngineSpec


class EggDBEngineSpec(PostgresEngineSpec):
    engine = "eggdb"
    engine_name = "EggDB"
    default_driver = "psycopg2"
    sqlalchemy_uri_placeholder = "eggdb+psycopg2://user:password@host:5432/database"
    supports_dynamic_schema = True
    supports_catalog = False
