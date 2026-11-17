import sys

from airbyte_eggdb.protocol import CONNECTION_SPEC, TYPES, check, connect, emit, now_ms, quoted, read_json


def discover(config):
    schema = config.get("schema", "public")
    streams = {}
    keys = {}
    with connect(config) as connection, connection.cursor() as cursor:
        cursor.execute(
            "SELECT table_name, column_name, data_type FROM information_schema.columns WHERE table_schema = %s ORDER BY table_name, ordinal_position",
            (schema,),
        )
        for table, column, data_type in cursor.fetchall():
            properties = streams.setdefault(table, {})
            properties[column] = dict(TYPES.get(data_type, {"type": "string"}))
            properties[column]["type"] = [properties[column]["type"], "null"]
        cursor.execute(
            "SELECT k.table_name, k.column_name FROM information_schema.table_constraints c JOIN information_schema.key_column_usage k ON k.constraint_name = c.constraint_name AND k.table_schema = c.table_schema WHERE c.constraint_type = 'PRIMARY KEY' AND c.table_schema = %s ORDER BY k.ordinal_position",
            (schema,),
        )
        for table, column in cursor.fetchall():
            keys.setdefault(table, []).append([column])
    catalog = []
    for table, properties in streams.items():
        catalog.append(
            {
                "name": table,
                "namespace": schema,
                "json_schema": {"type": "object", "properties": properties},
                "supported_sync_modes": ["full_refresh", "incremental"],
                "source_defined_cursor": False,
                "source_defined_primary_key": keys.get(table, []),
            }
        )
    emit({"type": "CATALOG", "catalog": {"streams": catalog}})


def read(config, configured, state):
    cursors = {}
    for entry in state or []:
        stream = entry.get("stream", {})
        name = stream.get("stream_descriptor", {}).get("name")
        if name:
            cursors[name] = stream.get("stream_state", {}).get("cursor")
    with connect(config) as connection:
        for chosen in configured["streams"]:
            stream = chosen["stream"]
            name = stream["name"]
            namespace = stream.get("namespace") or config.get("schema", "public")
            cursor_field = (chosen.get("cursor_field") or [None])[0]
            incremental = chosen.get("sync_mode") == "incremental" and cursor_field
            sql = f"SELECT * FROM {quoted(namespace)}.{quoted(name)}"
            parameters = ()
            if incremental:
                if cursors.get(name) is not None:
                    sql += f" WHERE {quoted(cursor_field)} > %s"
                    parameters = (cursors[name],)
                sql += f" ORDER BY {quoted(cursor_field)}"
            newest = cursors.get(name)
            with connection.cursor() as rows:
                rows.execute(sql, parameters)
                columns = [column.name for column in rows.description]
                for batch in iter(lambda: rows.fetchmany(10000), []):
                    for values in batch:
                        record = dict(zip(columns, values))
                        emit({"type": "RECORD", "record": {"stream": name, "namespace": namespace, "data": record, "emitted_at": now_ms()}})
                        if incremental:
                            newest = record.get(cursor_field)
            if incremental:
                emit(
                    {
                        "type": "STATE",
                        "state": {
                            "type": "STREAM",
                            "stream": {"stream_descriptor": {"name": name, "namespace": namespace}, "stream_state": {"cursor": newest}},
                        },
                    }
                )


def main(arguments=None):
    arguments = arguments if arguments is not None else sys.argv[1:]
    command = arguments[0] if arguments else "spec"
    options = dict(zip(arguments[1::2], arguments[2::2]))
    if command == "spec":
        emit({"type": "SPEC", "spec": {"documentationUrl": "https://github.com/eggspot/EggDB/blob/main/docs/connectors.md", "connectionSpecification": CONNECTION_SPEC}})
        return
    config = read_json(options["--config"])
    if command == "check":
        check(config)
    elif command == "discover":
        discover(config)
    elif command == "read":
        state = read_json(options["--state"]) if "--state" in options else []
        read(config, read_json(options["--catalog"]), state)


if __name__ == "__main__":
    main()
