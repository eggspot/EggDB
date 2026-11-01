from sqlalchemy import exc, text
import sqlalchemy.dialects.postgresql.provision
from sqlalchemy.testing.provision import post_configure_engine

FEATURE_NOT_SUPPORTED = "0A000"
EXTENSIONS = (("citext", (13,)), ("hstore", (13,)))


def is_unsupported(error):
    return getattr(error.orig, "sqlstate", None) == FEATURE_NOT_SUPPORTED


@post_configure_engine.for_db("postgresql")
def create_available_extensions(url, engine, follower_ident):
    with engine.connect() as connection:
        for extension, least_version in EXTENSIONS:
            if connection.dialect.server_version_info < least_version:
                continue
            try:
                connection.execute(text(f"CREATE EXTENSION IF NOT EXISTS {extension}"))
                connection.commit()
            except exc.DBAPIError as error:
                if not is_unsupported(error):
                    raise
                connection.rollback()

