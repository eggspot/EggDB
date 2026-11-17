import json
import sys
from datetime import datetime, timezone

import psycopg2

TYPES = {
    "smallint": {"type": "integer"},
    "integer": {"type": "integer"},
    "bigint": {"type": "integer"},
    "numeric": {"type": "number"},
    "real": {"type": "number"},
    "double precision": {"type": "number"},
    "boolean": {"type": "boolean"},
    "date": {"type": "string", "format": "date"},
    "timestamp without time zone": {"type": "string", "format": "date-time", "airbyte_type": "timestamp_without_timezone"},
    "timestamp with time zone": {"type": "string", "format": "date-time", "airbyte_type": "timestamp_with_timezone"},
    "json": {"type": "object"},
    "jsonb": {"type": "object"},
}

CONNECTION_SPEC = {
    "type": "object",
    "required": ["host", "port", "database", "username"],
    "properties": {
        "host": {"type": "string", "title": "Host", "order": 0},
        "port": {"type": "integer", "title": "Port", "default": 5432, "order": 1},
        "database": {"type": "string", "title": "Database", "default": "eggdb", "order": 2},
        "username": {"type": "string", "title": "User", "order": 3},
        "password": {"type": "string", "title": "Password", "airbyte_secret": True, "order": 4},
        "schema": {"type": "string", "title": "Schema", "default": "public", "order": 5},
        "ssl_mode": {"type": "string", "title": "SSL mode", "enum": ["disable", "prefer", "require", "verify-full"], "default": "prefer", "order": 6},
    },
}


def emit(message):
    sys.stdout.write(json.dumps(message, default=str) + "\n")
    sys.stdout.flush()


def log(level, text):
    emit({"type": "LOG", "log": {"level": level, "message": text}})


def connect(config):
    return psycopg2.connect(
        host=config["host"],
        port=config.get("port", 5432),
        dbname=config.get("database", "eggdb"),
        user=config["username"],
        password=config.get("password"),
        sslmode=config.get("ssl_mode", "prefer"),
    )


def check(config):
    try:
        with connect(config) as connection, connection.cursor() as cursor:
            cursor.execute("SELECT 1")
        emit({"type": "CONNECTION_STATUS", "connectionStatus": {"status": "SUCCEEDED"}})
    except Exception as error:
        emit({"type": "CONNECTION_STATUS", "connectionStatus": {"status": "FAILED", "message": str(error)}})


def quoted(name):
    return '"' + name.replace('"', '""') + '"'


def now_ms():
    return int(datetime.now(timezone.utc).timestamp() * 1000)


def read_json(path):
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)
