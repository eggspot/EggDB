//! SQL tables synced offline-first (D1107): `CREATE EXTENSION eggdb_sync` and
//! `eggdb_sync.enable(schema, table)` put a capture trigger on a table that logs each local row
//! change in `eggdb_sync.outbox`; [`drain_outbox`] hands those to the device's sync replica as
//! key-value writes, and [`write_rows`] writes the rows other devices changed back into the
//! tables, with capture off so they are not sent back. A row's sync key is its table and its
//! primary key, and its value the row as JSON.

use crate::error::SqlResult;
use crate::session::Session;
use crate::store::KeyValueStore;

/// The statements `CREATE EXTENSION eggdb_sync` runs.
pub(crate) const SYNC_TABLES_SCRIPT: &str = r"
CREATE SCHEMA eggdb_sync;
CREATE TABLE eggdb_sync.tables (
    table_schema text NOT NULL,
    table_name text NOT NULL,
    key_columns text NOT NULL,
    PRIMARY KEY (table_schema, table_name)
);
CREATE TABLE eggdb_sync.outbox (
    id bigserial PRIMARY KEY,
    table_schema text NOT NULL,
    table_name text NOT NULL,
    row_key text NOT NULL,
    row_data jsonb
);
CREATE FUNCTION eggdb_sync.row_key(columns text, row_data jsonb) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    key_object jsonb := '{}'::jsonb;
    column_name text;
BEGIN
    FOR column_name IN SELECT unnest(string_to_array(columns, ',')) LOOP
        key_object := key_object || jsonb_build_object(column_name, row_data -> column_name);
    END LOOP;
    RETURN key_object::text;
END
$$;
CREATE FUNCTION eggdb_sync.capture() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    columns text;
BEGIN
    IF coalesce(current_setting('eggdb_sync.applying', true), '') = 'on' THEN
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NEW;
    END IF;
    SELECT key_columns INTO columns FROM eggdb_sync.tables WHERE table_schema = TG_TABLE_SCHEMA AND table_name = TG_TABLE_NAME;
    IF TG_OP = 'DELETE' THEN
        INSERT INTO eggdb_sync.outbox (table_schema, table_name, row_key, row_data) VALUES (TG_TABLE_SCHEMA, TG_TABLE_NAME, eggdb_sync.row_key(columns, to_jsonb(OLD)), NULL);
        RETURN OLD;
    END IF;
    IF TG_OP = 'UPDATE' AND eggdb_sync.row_key(columns, to_jsonb(OLD)) <> eggdb_sync.row_key(columns, to_jsonb(NEW)) THEN
        INSERT INTO eggdb_sync.outbox (table_schema, table_name, row_key, row_data) VALUES (TG_TABLE_SCHEMA, TG_TABLE_NAME, eggdb_sync.row_key(columns, to_jsonb(OLD)), NULL);
    END IF;
    INSERT INTO eggdb_sync.outbox (table_schema, table_name, row_key, row_data) VALUES (TG_TABLE_SCHEMA, TG_TABLE_NAME, eggdb_sync.row_key(columns, to_jsonb(NEW)), to_jsonb(NEW));
    RETURN NEW;
END
$$;
CREATE FUNCTION eggdb_sync.enable(target_schema text, target_table text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    columns text;
BEGIN
    SELECT string_agg(k.column_name, ',' ORDER BY k.ordinal_position) INTO columns
    FROM information_schema.table_constraints c
    JOIN information_schema.key_column_usage k
      ON k.constraint_name = c.constraint_name AND k.table_schema = c.table_schema AND k.table_name = c.table_name
    WHERE c.constraint_type = 'PRIMARY KEY' AND c.table_schema = target_schema AND c.table_name = target_table;
    IF columns IS NULL THEN
        RAISE EXCEPTION 'table %.% needs a primary key to sync', target_schema, target_table;
    END IF;
    DELETE FROM eggdb_sync.tables WHERE table_schema = target_schema AND table_name = target_table;
    INSERT INTO eggdb_sync.tables (table_schema, table_name, key_columns) VALUES (target_schema, target_table, columns);
    EXECUTE format('DROP TRIGGER IF EXISTS eggdb_sync_capture ON %I.%I', target_schema, target_table);
    EXECUTE format('CREATE TRIGGER eggdb_sync_capture AFTER INSERT OR UPDATE OR DELETE ON %I.%I FOR EACH ROW EXECUTE FUNCTION eggdb_sync.capture()', target_schema, target_table);
    RETURN columns;
END
$$;
CREATE FUNCTION eggdb_sync.write_row(target_schema text, target_table text, key_text text, row_data jsonb) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    columns text;
    keyed jsonb := key_text::jsonb;
    predicate text := '';
    column_name text;
BEGIN
    SELECT key_columns INTO columns FROM eggdb_sync.tables WHERE table_schema = target_schema AND table_name = target_table;
    IF columns IS NULL THEN
        RETURN;
    END IF;
    PERFORM set_config('eggdb_sync.applying', 'on', true);
    FOR column_name IN SELECT unnest(string_to_array(columns, ',')) LOOP
        IF predicate <> '' THEN
            predicate := predicate || ' AND ';
        END IF;
        predicate := predicate || format('%I::text = %L', column_name, keyed ->> column_name);
    END LOOP;
    EXECUTE format('DELETE FROM %I.%I WHERE %s', target_schema, target_table, predicate);
    IF row_data IS NOT NULL THEN
        EXECUTE format('INSERT INTO %I.%I SELECT * FROM jsonb_populate_record(NULL::%I.%I, $1)', target_schema, target_table, target_schema, target_table) USING row_data;
    END IF;
    PERFORM set_config('eggdb_sync.applying', 'off', true);
END
$$;
";

const KEY_SEPARATOR: u8 = 0;

/// The sync key of a row: `schema.table`, a zero byte, and its primary key as JSON.
pub fn row_sync_key(schema: &str, table: &str, row_key: &str) -> Vec<u8> {
    let mut key = format!("{schema}.{table}").into_bytes();
    key.push(KEY_SEPARATOR);
    key.extend_from_slice(row_key.as_bytes());
    key
}

/// Splits a sync key [`row_sync_key`] made back into schema, table and primary key; `None` for
/// keys that are not rows of a synced table.
pub fn split_sync_key(key: &[u8]) -> Option<(String, String, String)> {
    let at = key.iter().position(|byte| *byte == KEY_SEPARATOR)?;
    let name = std::str::from_utf8(&key[..at]).ok()?;
    let (schema, table) = name.split_once('.')?;
    let row_key = std::str::from_utf8(&key[at + 1..]).ok()?;
    Some((schema.to_owned(), table.to_owned(), row_key.to_owned()))
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// One logged local change: its sync key and its row as JSON, `None` for a deletion.
pub type OutboxChange = (Vec<u8>, Option<Vec<u8>>);

/// Hands up to `limit` logged local changes, oldest first, to `deliver`, and removes them from
/// the outbox only once it succeeded, so a failure keeps them for the next call; returns how
/// many were delivered.
pub fn drain_outbox<S: KeyValueStore>(
    store: &S,
    limit: u32,
    deliver: &mut dyn FnMut(&[OutboxChange]) -> Result<(), String>,
) -> SqlResult<usize> {
    let mut session = Session::with_dialect(store, crate::dialect::Dialect::PostgreSql);
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    session.execute(
        &format!("SELECT id, table_schema, table_name, row_key, row_data::text FROM eggdb_sync.outbox ORDER BY id LIMIT {limit}"),
        &mut |result| rows = result.rows.iter().map(|row| row.iter().map(|value| value.to_text()).collect()).collect(),
    )?;
    let last = rows.last().and_then(|row| row.first().cloned().flatten()).and_then(|text| text.parse::<i64>().ok());
    let changes: Vec<OutboxChange> = rows
        .into_iter()
        .filter_map(|row| {
            let text = |at: usize| row.get(at).cloned().flatten();
            Some((row_sync_key(&text(1)?, &text(2)?, &text(3)?), text(4).map(String::into_bytes)))
        })
        .collect();
    let Some(last) = last else {
        let _ = session.close();
        return Ok(0);
    };
    deliver(&changes).map_err(|message| crate::error::SqlError::new(crate::error::SqlState::IO_ERROR, message))?;
    session.execute(&format!("DELETE FROM eggdb_sync.outbox WHERE id <= {last}"), &mut |_| {})?;
    let _ = session.close();
    Ok(changes.len())
}

/// Writes rows other devices changed into their tables, in one transaction, with capture off:
/// each `(sync key, row as JSON or None)` replaces the table's row with that primary key, or
/// deletes it. Keys of tables not synced here are passed over. Returns how many rows were
/// written.
pub fn write_rows<S: KeyValueStore>(store: &S, rows: &[OutboxChange]) -> SqlResult<usize> {
    let mut script = String::from("BEGIN;\n");
    let mut written = 0;
    for (key, value) in rows {
        let Some((schema, table, row_key)) = split_sync_key(key) else {
            continue;
        };
        let data = match value {
            Some(bytes) => format!("{}::jsonb", literal(&String::from_utf8_lossy(bytes))),
            None => "NULL::jsonb".to_owned(),
        };
        script.push_str(&format!(
            "SELECT eggdb_sync.write_row({}, {}, {}, {data});\n",
            literal(&schema),
            literal(&table),
            literal(&row_key)
        ));
        written += 1;
    }
    script.push_str("COMMIT;\n");
    if written == 0 {
        return Ok(0);
    }
    let mut session = Session::with_dialect(store, crate::dialect::Dialect::PostgreSql);
    session.execute(&script, &mut |_| {})?;
    let _ = session.close();
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_store::MemoryStore;
    use crate::tests::render;

    fn ok(session: &mut Session<'_>, sql: &str) -> String {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(render(&result))).unwrap_or_else(|error| panic!("{sql}: {error}"));
        outputs.pop().unwrap_or_default()
    }

    #[test]
    fn sync_keys_split_back() {
        let key = row_sync_key("public", "notes", "{\"id\": 7}");
        assert_eq!(split_sync_key(&key), Some(("public".to_owned(), "notes".to_owned(), "{\"id\": 7}".to_owned())));
        assert_eq!(split_sync_key(b"no separator"), None);
    }

    #[test]
    fn local_changes_leave_through_the_outbox_and_remote_rows_come_in_without_echo() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        ok(&mut session, "CREATE EXTENSION eggdb_sync");
        ok(&mut session, "CREATE TABLE notes (id integer PRIMARY KEY, body text)");
        ok(&mut session, "SELECT eggdb_sync.enable('public', 'notes')");
        ok(&mut session, "INSERT INTO notes VALUES (1, 'on the phone'), (2, 'to delete')");
        ok(&mut session, "DELETE FROM notes WHERE id = 2");
        drop(session);
        assert_eq!(drain_outbox(&store, 100, &mut |_| Err("offline".to_owned())).map_err(|error| error.message().to_owned()), Err("offline".to_owned()));
        let mut drained = Vec::new();
        assert_eq!(drain_outbox(&store, 100, &mut |changes| {
            drained.extend_from_slice(changes);
            Ok(())
        }).unwrap(), 3);
        assert!(drained[2].1.is_none());
        assert_eq!(drain_outbox(&store, 100, &mut |_| Ok(())).unwrap(), 0);
        let incoming = vec![
            (row_sync_key("public", "notes", "{\"id\": 3}"), Some(b"{\"id\": 3, \"body\": \"from the laptop\"}".to_vec())),
            (row_sync_key("public", "notes", "{\"id\": 1}"), None),
            (row_sync_key("public", "elsewhere", "{\"id\": 1}"), None),
        ];
        assert_eq!(write_rows(&store, &incoming).unwrap(), 3);
        let mut session = Session::new(&store);
        assert_eq!(ok(&mut session, "SELECT id, body FROM notes ORDER BY id"), "id|body\n3|from the laptop\n");
        drop(session);
        assert_eq!(drain_outbox(&store, 100, &mut |_| Ok(())).unwrap(), 0);
    }
}
