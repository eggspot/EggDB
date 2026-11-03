from sqlalchemy import text
import sqlalchemy.dialects.postgresql.provision
from sqlalchemy.testing.provision import post_configure_engine

EXTENSIONS = (("citext", (13,)), ("hstore", (13,)))
SUITE_SCHEMAS = ("test_schema", "test_schema_2")


@post_configure_engine.for_db("postgresql")
def create_suite_schemas_and_available_extensions(url, engine, follower_ident):
    with engine.connect() as connection:
        for schema in SUITE_SCHEMAS:
            connection.execute(text(f"CREATE SCHEMA IF NOT EXISTS {schema}"))
            connection.commit()
        for extension, least_version in EXTENSIONS:
            if connection.dialect.server_version_info < least_version:
                continue
            connection.execute(text(f"CREATE EXTENSION IF NOT EXISTS {extension}"))
            connection.commit()
