import json
import sys

import psycopg2.extras

from airbyte_eggdb.protocol import CONNECTION_SPEC, check, connect, emit, now_ms, quoted, read_json

BATCH_ROWS = 1000


def write(config, configured, lines):
    schema = config.get("schema", "public")
    streams = {chosen["stream"]["name"]: chosen for chosen in configured["streams"]}
    pending = {name: [] for name in streams}
    with connect(config) as connection, connection.cursor() as cursor:
        cursor.execute(f"CREATE SCHEMA IF NOT EXISTS {quoted(schema)}")
        for name, chosen in streams.items():
            table = f"{quoted(schema)}.{quoted('_airbyte_raw_' + name)}"
            cursor.execute(
                f"CREATE TABLE IF NOT EXISTS {table} (_airbyte_ab_id bigserial PRIMARY KEY, _airbyte_data jsonb NOT NULL, _airbyte_emitted_at timestamptz NOT NULL)"
            )
            if chosen.get("destination_sync_mode") == "overwrite":
                cursor.execute(f"DELETE FROM {table}")

        def flush(name):
            rows = pending[name]
            if not rows:
                return
            table = f"{quoted(schema)}.{quoted('_airbyte_raw_' + name)}"
            psycopg2.extras.execute_values(
                cursor,
                f"INSERT INTO {table} (_airbyte_data, _airbyte_emitted_at) VALUES %s",
                rows,
                template="(%s::jsonb, to_timestamp(%s / 1000.0))",
            )
            rows.clear()

        for line in lines:
            line = line.strip()
            if not line:
                continue
            message = json.loads(line)
            if message.get("type") == "RECORD":
                record = message["record"]
                name = record["stream"]
                if name in pending:
                    pending[name].append((json.dumps(record["data"], default=str), record.get("emitted_at", now_ms())))
                    if len(pending[name]) >= BATCH_ROWS:
                        flush(name)
            elif message.get("type") == "STATE":
                for name in pending:
                    flush(name)
                connection.commit()
                emit(message)
        for name in pending:
            flush(name)
        connection.commit()


def main(arguments=None):
    arguments = arguments if arguments is not None else sys.argv[1:]
    command = arguments[0] if arguments else "spec"
    options = dict(zip(arguments[1::2], arguments[2::2]))
    if command == "spec":
        emit(
            {
                "type": "SPEC",
                "spec": {
                    "documentationUrl": "https://github.com/eggspot/EggDB/blob/main/docs/connectors.md",
                    "supported_destination_sync_modes": ["overwrite", "append"],
                    "connectionSpecification": CONNECTION_SPEC,
                },
            }
        )
        return
    config = read_json(options["--config"])
    if command == "check":
        check(config)
    elif command == "write":
        write(config, read_json(options["--catalog"]), sys.stdin)


if __name__ == "__main__":
    main()
