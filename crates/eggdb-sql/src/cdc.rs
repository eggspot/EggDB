//! Change streams out (D210): `CREATE EXTENSION eggdb_cdc` installs a transactional outbox and
//! a row trigger function that writes each change into it in the same transaction, and
//! [`deliver_changes`] hands the oldest changes, as Debezium-style JSON events, to a sink and
//! removes the ones it accepted, so every committed change is delivered at least once, in
//! commit order.

use crate::dialect::Dialect;
use crate::error::{SqlResult, SqlState};
use crate::session::Session;
use crate::store::KeyValueStore;
use crate::types::Value;

/// The statements `CREATE EXTENSION eggdb_cdc` runs.
pub(crate) const CDC_SCRIPT: &str = r"
CREATE SCHEMA eggdb_cdc;
CREATE TABLE eggdb_cdc.outbox (
    id bigserial PRIMARY KEY,
    table_schema text NOT NULL,
    table_name text NOT NULL,
    op text NOT NULL,
    before jsonb,
    after jsonb,
    captured_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE eggdb_cdc.options (
    name text PRIMARY KEY,
    value text NOT NULL
);
CREATE FUNCTION eggdb_cdc.capture() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO eggdb_cdc.outbox (table_schema, table_name, op, after) VALUES (TG_TABLE_SCHEMA, TG_TABLE_NAME, 'c', to_jsonb(NEW));
        RETURN NEW;
    ELSIF TG_OP = 'UPDATE' THEN
        INSERT INTO eggdb_cdc.outbox (table_schema, table_name, op, before, after) VALUES (TG_TABLE_SCHEMA, TG_TABLE_NAME, 'u', to_jsonb(OLD), to_jsonb(NEW));
        RETURN NEW;
    ELSE
        INSERT INTO eggdb_cdc.outbox (table_schema, table_name, op, before) VALUES (TG_TABLE_SCHEMA, TG_TABLE_NAME, 'd', to_jsonb(OLD));
        RETURN OLD;
    END IF;
END
$$;
";

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32))
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn text_of(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::Text(text) | Value::Jsonb(text) | Value::Json(text) => Some(text.clone()),
        other => other.to_text(),
    }
}

/// The Debezium-style envelope of one change: `before`, `after`, `source` (connector, database,
/// schema, table, `ts_ms`), `op` (`c`, `u` or `d`) and `ts_ms`.
pub fn debezium_event(
    database: &str,
    schema: &str,
    table: &str,
    op: &str,
    before: Option<&str>,
    after: Option<&str>,
    captured_ms: i64,
) -> String {
    format!(
        "{{\"before\":{},\"after\":{},\"source\":{{\"connector\":\"eggdb\",\"db\":{},\"schema\":{},\"table\":{},\"ts_ms\":{captured_ms}}},\"op\":{},\"ts_ms\":{captured_ms}}}",
        before.unwrap_or("null"),
        after.unwrap_or("null"),
        json_string(database),
        json_string(schema),
        json_string(table),
        json_string(op)
    )
}

/// The Kafka Connect type Debezium's JSON converter gives a column of SQL type `data_type`, as
/// `information_schema.columns` names it, matching how `to_jsonb` writes the column's values.
pub fn connect_type(data_type: &str) -> &'static str {
    match data_type {
        "smallint" => "int16",
        "integer" => "int32",
        "bigint" => "int64",
        "real" => "float",
        "double precision" | "numeric" => "double",
        "boolean" => "boolean",
        _ => "string",
    }
}

fn connect_fields(columns: &[(String, String, bool)]) -> String {
    columns
        .iter()
        .map(|(name, data_type, nullable)| {
            format!(
                "{{\"type\":\"{}\",\"optional\":{nullable},\"field\":{}}}",
                connect_type(data_type),
                json_string(name)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The `schema` section Debezium's JSON converter puts beside the envelope (`schemas.enable`):
/// the table's columns as the `before` and `after` structs, then `source`, `op` and `ts_ms`.
/// `columns` are each column's name, SQL type and whether it takes NULL, in table order.
pub fn debezium_schema(
    database: &str,
    schema: &str,
    table: &str,
    columns: &[(String, String, bool)],
) -> String {
    let record = format!("{database}.{schema}.{table}");
    let value = format!(
        "{{\"type\":\"struct\",\"fields\":[{}],\"optional\":true,\"name\":{}",
        connect_fields(columns),
        json_string(&format!("{record}.Value"))
    );
    let source = "{\"type\":\"struct\",\"fields\":[{\"type\":\"string\",\"optional\":false,\"field\":\"connector\"},{\"type\":\"string\",\"optional\":false,\"field\":\"db\"},{\"type\":\"string\",\"optional\":false,\"field\":\"schema\"},{\"type\":\"string\",\"optional\":false,\"field\":\"table\"},{\"type\":\"int64\",\"optional\":false,\"field\":\"ts_ms\"}],\"optional\":false,\"name\":\"io.debezium.connector.eggdb.Source\",\"field\":\"source\"}";
    format!(
        "{{\"type\":\"struct\",\"fields\":[{value},\"field\":\"before\"}},{value},\"field\":\"after\"}},{source},{{\"type\":\"string\",\"optional\":false,\"field\":\"op\"}},{{\"type\":\"int64\",\"optional\":true,\"field\":\"ts_ms\"}}],\"optional\":false,\"name\":{},\"version\":1}}",
        json_string(&format!("{record}.Envelope"))
    )
}

fn schemas_enabled(session: &mut Session<'_>) -> bool {
    let mut enabled = false;
    let _ = session.execute(
        "SELECT value FROM eggdb_cdc.options WHERE name = 'schemas.enable'",
        &mut |result| {
            enabled = result
                .rows
                .first()
                .and_then(|row| text_of(row.first()))
                .is_some_and(|value| value.eq_ignore_ascii_case("true"));
        },
    );
    enabled
}

fn table_columns(session: &mut Session<'_>, schema: &str, table: &str) -> SqlResult<Vec<(String, String, bool)>> {
    let mut columns = Vec::new();
    session.execute(
        &format!(
            "SELECT column_name, data_type, is_nullable FROM information_schema.columns WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
            sql_text(schema),
            sql_text(table)
        ),
        &mut |result| {
            columns = result
                .rows
                .iter()
                .map(|row| {
                    (
                        text_of(row.first()).unwrap_or_default(),
                        text_of(row.get(1)).unwrap_or_default(),
                        text_of(row.get(2)).is_none_or(|nullable| nullable == "YES"),
                    )
                })
                .collect();
        },
    )?;
    Ok(columns)
}

/// Hands up to `limit` of the oldest captured changes to `deliver`, in order, as Debezium-style
/// JSON; stops at the first one `deliver` refuses, and removes the ones it accepted. Returns
/// how many were delivered. A file without the extension has nothing to deliver.
pub fn deliver_changes<S: KeyValueStore>(
    store: &S,
    database: &str,
    limit: u64,
    deliver: &mut dyn FnMut(&str) -> bool,
) -> SqlResult<u64> {
    deliver_outbox(store, database, limit, deliver).map(|(delivered, _)| delivered)
}

/// How many deliveries of the oldest change may fail before it moves to `eggdb_cdc.dead_letters`.
pub const MAX_DELIVERY_ATTEMPTS: i64 = 10;
const FIRST_RETRY_MS: i64 = 1_000;
const LONGEST_RETRY_MS: i64 = 300_000;
const DELIVERY_TABLES: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS eggdb_cdc.delivery_state (target text PRIMARY KEY, failures integer NOT NULL, retry_at bigint NOT NULL)",
    "CREATE TABLE IF NOT EXISTS eggdb_cdc.dead_letters (id bigint PRIMARY KEY, target text NOT NULL, event text NOT NULL, failures integer NOT NULL, failed_at timestamptz NOT NULL DEFAULT now())",
];

fn sql_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The wait before retrying after `failures` failed deliveries in a row: one second, doubled
/// with each failure, at most five minutes.
pub fn retry_delay_ms(failures: i64) -> i64 {
    let doublings = u32::try_from(failures.saturating_sub(1).clamp(0, 20)).unwrap_or(20);
    FIRST_RETRY_MS.saturating_mul(1i64 << doublings).min(LONGEST_RETRY_MS)
}

/// [`deliver_changes`] for one named delivery `target`, with retries: after a refused change,
/// the target waits (see [`retry_delay_ms`]) before trying again, and a change refused
/// [`MAX_DELIVERY_ATTEMPTS`] times in a row moves to `eggdb_cdc.dead_letters` with its event,
/// so the changes after it flow again. `now_ms` is the time in milliseconds since 1970.
pub fn deliver_changes_with_retries<S: KeyValueStore>(
    store: &S,
    database: &str,
    target: &str,
    limit: u64,
    now_ms: i64,
    deliver: &mut dyn FnMut(&str) -> bool,
) -> SqlResult<u64> {
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    for statement in DELIVERY_TABLES {
        if let Err(error) = session.execute(statement, &mut |_| {}) {
            let _ = session.close();
            return if matches!(error.state(), SqlState::INVALID_SCHEMA_NAME | SqlState::UNDEFINED_TABLE) {
                Ok(0)
            } else {
                Err(error)
            };
        }
    }
    let mut state = Vec::new();
    session.execute(
        &format!(
            "SELECT failures, retry_at FROM eggdb_cdc.delivery_state WHERE target = {}",
            sql_text(target)
        ),
        &mut |result| {
            if result.columns.is_some() {
                state = result.rows;
            }
        },
    )?;
    let number = |value: Option<&Value>| match value {
        Some(Value::Integer(number)) => i64::from(*number),
        Some(Value::BigInt(number)) => *number,
        _ => 0,
    };
    let (failures, retry_at) = state
        .first()
        .map_or((0, 0), |row| (number(row.first()), number(row.get(1))));
    if now_ms < retry_at {
        session.close()?;
        return Ok(0);
    }
    session.close()?;
    let (delivered, refused) = deliver_outbox(store, database, limit, deliver)?;
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    let failures = match &refused {
        None => 0,
        Some(_) => failures + 1,
    };
    let mut statements = vec![format!("DELETE FROM eggdb_cdc.delivery_state WHERE target = {}", sql_text(target))];
    match refused {
        Some((id, event)) if failures >= MAX_DELIVERY_ATTEMPTS => {
            statements.push(format!(
                "INSERT INTO eggdb_cdc.dead_letters (id, target, event, failures) VALUES ({id}, {}, {}, {failures}) ON CONFLICT (id) DO NOTHING",
                sql_text(target),
                sql_text(&event)
            ));
            statements.push(format!("DELETE FROM eggdb_cdc.outbox WHERE id = {id}"));
        }
        Some(_) => statements.push(format!(
            "INSERT INTO eggdb_cdc.delivery_state VALUES ({}, {failures}, {})",
            sql_text(target),
            now_ms.saturating_add(retry_delay_ms(failures))
        )),
        None => {}
    }
    for statement in statements {
        session.execute(&statement, &mut |_| {})?;
    }
    session.close()?;
    Ok(delivered)
}

fn deliver_outbox<S: KeyValueStore>(
    store: &S,
    database: &str,
    limit: u64,
    deliver: &mut dyn FnMut(&str) -> bool,
) -> SqlResult<(u64, Option<(i64, String)>)> {
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    let mut rows = Vec::new();
    let listed = session.execute(
        &format!(
            "SELECT id, table_schema, table_name, op, before::text, after::text, (extract(epoch from captured_at) * 1000)::bigint FROM eggdb_cdc.outbox ORDER BY id LIMIT {limit}"
        ),
        &mut |result| rows = result.rows,
    );
    if let Err(error) = listed {
        let _ = session.close();
        return if matches!(
            error.state(),
            SqlState::UNDEFINED_TABLE | SqlState::INVALID_SCHEMA_NAME
        ) {
            Ok((0, None))
        } else {
            Err(error)
        };
    }
    let with_schemas = schemas_enabled(&mut session);
    let mut schemas: std::collections::HashMap<(String, String), String> = std::collections::HashMap::new();
    let mut last_delivered = None;
    let mut delivered = 0;
    let mut refused = None;
    for row in &rows {
        let Some(Value::BigInt(id)) = row.first() else {
            continue;
        };
        let schema = text_of(row.get(1)).unwrap_or_default();
        let table = text_of(row.get(2)).unwrap_or_default();
        let op = text_of(row.get(3)).unwrap_or_default();
        let before = text_of(row.get(4));
        let after = text_of(row.get(5));
        let captured_ms = match row.get(6) {
            Some(Value::BigInt(milliseconds)) => *milliseconds,
            _ => 0,
        };
        let mut event = debezium_event(
            database,
            &schema,
            &table,
            &op,
            before.as_deref(),
            after.as_deref(),
            captured_ms,
        );
        if with_schemas {
            let key = (schema.clone(), table.clone());
            if !schemas.contains_key(&key) {
                let columns = table_columns(&mut session, &schema, &table)?;
                schemas.insert(key.clone(), debezium_schema(database, &schema, &table, &columns));
            }
            if let Some(described) = schemas.get(&key) {
                event = format!("{{\"schema\":{described},\"payload\":{event}}}");
            }
        }
        if !deliver(&event) {
            refused = Some((*id, event));
            break;
        }
        last_delivered = Some(*id);
        delivered += 1;
    }
    if let Some(last) = last_delivered {
        session.execute(
            &format!("DELETE FROM eggdb_cdc.outbox WHERE id <= {last}"),
            &mut |_| {},
        )?;
    }
    session.close()?;
    Ok((delivered, refused))
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_DELIVERY_ATTEMPTS, connect_type, debezium_event, deliver_changes, deliver_changes_with_retries,
        retry_delay_ms,
    };
    use crate::memory_store::MemoryStore;
    use crate::session::Session;

    #[test]
    fn captured_changes_are_delivered_once_in_order() {
        let store = MemoryStore::default();
        assert_eq!(
            deliver_changes(&store, "app", 10, &mut |_| true).unwrap(),
            0
        );
        let mut session = Session::new(&store);
        for statement in [
            "CREATE EXTENSION eggdb_cdc",
            "CREATE TABLE accounts (id int PRIMARY KEY, balance int)",
            "CREATE TRIGGER accounts_cdc AFTER INSERT OR UPDATE OR DELETE ON accounts FOR EACH ROW EXECUTE FUNCTION eggdb_cdc.capture()",
            "INSERT INTO accounts VALUES (1, 100)",
            "UPDATE accounts SET balance = 50 WHERE id = 1",
            "DELETE FROM accounts WHERE id = 1",
        ] {
            session
                .execute(statement, &mut |_| {})
                .unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
        let mut seen = Vec::new();
        let mut refuse_after = 2;
        let first = deliver_changes(&store, "app", 10, &mut |event| {
            if refuse_after == 0 {
                return false;
            }
            refuse_after -= 1;
            seen.push(event.to_owned());
            true
        })
        .unwrap();
        assert_eq!(first, 2);
        let second = deliver_changes(&store, "app", 10, &mut |event| {
            seen.push(event.to_owned());
            true
        })
        .unwrap();
        assert_eq!(second, 1);
        assert!(
            seen[0].contains("\"op\":\"c\"") && seen[0].contains("\"before\":null"),
            "{}",
            seen[0]
        );
        assert!(
            seen[1].contains("\"op\":\"u\"") && seen[1].contains("\"balance\": 100")
                || seen[1].contains("\"balance\":100"),
            "{}",
            seen[1]
        );
        assert!(
            seen[2].contains("\"op\":\"d\"") && seen[2].contains("\"after\":null"),
            "{}",
            seen[2]
        );
        assert_eq!(
            deliver_changes(&store, "app", 10, &mut |_| true).unwrap(),
            0
        );
    }

    #[test]
    fn rows_changed_by_cascades_and_replace_are_captured() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for statement in [
            "CREATE EXTENSION eggdb_cdc",
            "CREATE TABLE owners (id int PRIMARY KEY)",
            "CREATE TABLE pets (id int PRIMARY KEY, owner_id int REFERENCES owners ON DELETE CASCADE ON UPDATE SET NULL)",
            "CREATE TRIGGER pets_cdc AFTER INSERT OR UPDATE OR DELETE ON pets FOR EACH ROW EXECUTE FUNCTION eggdb_cdc.capture()",
            "INSERT INTO owners VALUES (1), (2)",
            "INSERT INTO pets VALUES (10, 1), (20, 2)",
            "DELETE FROM eggdb_cdc.outbox",
            "DELETE FROM owners WHERE id = 1",
            "UPDATE owners SET id = 3 WHERE id = 2",
        ] {
            session
                .execute(statement, &mut |_| {})
                .unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
        let mut sqlite = Session::with_dialect(&store, crate::dialect::Dialect::Sqlite);
        sqlite
            .execute("INSERT OR REPLACE INTO pets VALUES (20, NULL)", &mut |_| {})
            .unwrap();
        let mut seen = Vec::new();
        deliver_changes(&store, "app", 10, &mut |event| {
            seen.push(event.to_owned());
            true
        })
        .unwrap();
        let ops: Vec<&str> = seen
            .iter()
            .map(|event| {
                ["c", "u", "d"]
                    .into_iter()
                    .find(|op| event.contains(&format!("\"op\":\"{op}\"")))
                    .unwrap_or("?")
            })
            .collect();
        assert_eq!(ops, vec!["d", "u", "d", "c"], "{seen:?}");
        assert!(seen[0].contains("10"), "{}", seen[0]);
        assert!(
            seen[1].contains("\"owner_id\": null") || seen[1].contains("\"owner_id\":null"),
            "{}",
            seen[1]
        );
    }

    #[test]
    fn events_escape_names() {
        let event = debezium_event("a\"b", "public", "t", "c", None, Some("{}"), 5);
        assert!(event.starts_with("{\"before\":null,\"after\":{},\"source\":{\"connector\":\"eggdb\",\"db\":\"a\\\"b\""), "{event}");
    }

    #[test]
    fn the_schema_section_describes_the_table_when_asked_for() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for statement in [
            "CREATE EXTENSION eggdb_cdc",
            "CREATE TABLE accounts (id int PRIMARY KEY, balance bigint, note text)",
            "CREATE TRIGGER accounts_cdc AFTER INSERT OR UPDATE OR DELETE ON accounts FOR EACH ROW EXECUTE FUNCTION eggdb_cdc.capture()",
            "INSERT INTO accounts VALUES (1, 100, 'a')",
            "INSERT INTO eggdb_cdc.options VALUES ('schemas.enable', 'true')",
            "INSERT INTO accounts VALUES (2, 200, 'b')",
        ] {
            session
                .execute(statement, &mut |_| {})
                .unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
        let mut seen = Vec::new();
        deliver_changes(&store, "app", 10, &mut |event| {
            seen.push(event.to_owned());
            true
        })
        .unwrap();
        assert_eq!(seen.len(), 2);
        for event in &seen {
            assert!(event.starts_with("{\"schema\":{\"type\":\"struct\""), "{event}");
            assert!(event.contains("{\"type\":\"int32\",\"optional\":false,\"field\":\"id\"}"), "{event}");
            assert!(event.contains("{\"type\":\"int64\",\"optional\":true,\"field\":\"balance\"}"), "{event}");
            assert!(event.contains("\"name\":\"app.public.accounts.Envelope\""), "{event}");
            assert!(event.contains(",\"payload\":{\"before\":null,\"after\":"), "{event}");
        }
        assert_eq!(connect_type("numeric"), "double");
        assert_eq!(connect_type("timestamp with time zone"), "string");
    }

    #[test]
    fn refused_changes_back_off_and_then_go_to_the_dead_letters() {
        assert_eq!(retry_delay_ms(1), 1_000);
        assert_eq!(retry_delay_ms(3), 4_000);
        assert_eq!(retry_delay_ms(40), 300_000);
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for statement in [
            "CREATE EXTENSION eggdb_cdc",
            "CREATE TABLE notes (id int PRIMARY KEY)",
            "CREATE TRIGGER notes_cdc AFTER INSERT ON notes FOR EACH ROW EXECUTE FUNCTION eggdb_cdc.capture()",
            "INSERT INTO notes VALUES (1)",
            "INSERT INTO notes VALUES (2)",
        ] {
            session
                .execute(statement, &mut |_| {})
                .unwrap_or_else(|error| panic!("{statement}: {error}"));
        }
        let mut now = 0i64;
        let mut tries = 0;
        for _ in 0..MAX_DELIVERY_ATTEMPTS {
            assert_eq!(
                deliver_changes_with_retries(&store, "app", "hook", 10, now, &mut |event| {
                    tries += 1;
                    !event.contains("\"id\": 1") && !event.contains("\"id\":1")
                })
                .unwrap(),
                0
            );
            assert_eq!(deliver_changes_with_retries(&store, "app", "hook", 10, now, &mut |_| true).unwrap(), 0);
            now += 400_000;
        }
        assert_eq!(tries, MAX_DELIVERY_ATTEMPTS);
        let mut seen = Vec::new();
        assert_eq!(
            deliver_changes_with_retries(&store, "app", "hook", 10, now, &mut |event| {
                seen.push(event.to_owned());
                true
            })
            .unwrap(),
            1
        );
        let mut dead = 0;
        session
            .execute("SELECT count(*) FROM eggdb_cdc.dead_letters", &mut |result| {
                if let Some(row) = result.rows.first() {
                    dead = row.first().and_then(|value| value.to_text()).unwrap_or_default().parse::<i64>().unwrap_or(0);
                }
            })
            .unwrap();
        assert_eq!(dead, 1);
    }
}

/// Where the engine keeps each commit's captured row changes when capture without triggers is
/// on (D1030): this prefix and the capture's sequence, big-endian.
pub const CAPTURE_PREFIX: &[u8] = b"\xffCOL\x00\x7fcapture\x00";

/// One captured key: its stored bytes before the commit and after it, `None` when absent.
pub type CapturedKey = (Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>);

/// Whether `key` holds a row of a SQL table, the keys capture without triggers records.
pub fn is_row_key(key: &[u8]) -> bool {
    key.len() > crate::encoding::NAMESPACE.len() + 4 && key.starts_with(&crate::encoding::rows_prefix(0)[..crate::encoding::NAMESPACE.len() + 1])
}

/// The capture record of one commit's row changes.
pub fn encode_capture(keys: &[CapturedKey]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(keys.len() as u64).to_le_bytes());
    let put = |bytes: Option<&Vec<u8>>, out: &mut Vec<u8>| match bytes {
        Some(bytes) => {
            out.push(1);
            out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        None => out.push(0),
    };
    for (key, before, after) in keys {
        put(Some(key), &mut out);
        put(before.as_ref(), &mut out);
        put(after.as_ref(), &mut out);
    }
    out
}

pub(crate) fn decode_capture_record(bytes: &[u8]) -> Option<Vec<CapturedKey>> {
    let mut at = 0usize;
    let number = |at: &mut usize| -> Option<u64> {
        let value = u64::from_le_bytes(bytes.get(*at..*at + 8)?.try_into().ok()?);
        *at += 8;
        Some(value)
    };
    let count = number(&mut at)?;
    let part = |at: &mut usize| -> Option<Option<Vec<u8>>> {
        let present = *bytes.get(*at)?;
        *at += 1;
        if present == 0 {
            return Some(None);
        }
        let length = usize::try_from(u64::from_le_bytes(bytes.get(*at..*at + 8)?.try_into().ok()?)).ok()?;
        *at += 8;
        let value = bytes.get(*at..at.checked_add(length)?)?.to_vec();
        *at += length;
        Some(Some(value))
    };
    let mut keys = Vec::with_capacity(usize::try_from(count.min(1 << 16)).ok()?);
    for _ in 0..count {
        let key = part(&mut at)??;
        let before = part(&mut at)?;
        let after = part(&mut at)?;
        keys.push((key, before, after));
    }
    (at == bytes.len()).then_some(keys)
}

fn row_json(definition: &crate::encoding::TableDefinition, stored: &[u8]) -> SqlResult<String> {
    let values = crate::encoding::decode_row(stored, &definition.columns)?;
    let mut out = String::from("{");
    for (index, (column, value)) in definition.columns.iter().zip(values.iter()).enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&json_string(&column.name));
        out.push(':');
        out.push_str(&crate::json_functions::to_json_text(value)?);
    }
    out.push('}');
    Ok(out)
}

pub(crate) fn table_definitions(reader: &dyn crate::store::KeyValueRead) -> SqlResult<std::collections::HashMap<u32, crate::encoding::TableDefinition>> {
    let prefix = crate::encoding::table_key("");
    let end = crate::encoding::prefix_end(&prefix);
    let mut tables = std::collections::HashMap::new();
    reader.scan(&prefix, &end, crate::store::ScanDirection::Forward, &mut |key, value| {
        if crate::encoding::is_view_record(value) {
            return Ok(true);
        }
        let name = String::from_utf8_lossy(&key[prefix.len()..]).into_owned();
        if let Ok(definition) = crate::encoding::TableDefinition::decode(&name, value) {
            tables.insert(definition.id, definition);
        }
        Ok(true)
    })?;
    Ok(tables)
}

fn with_transaction(event: String, transaction: &str, total_order: u64, collection_order: u64) -> String {
    let Some(body) = event.strip_suffix('}') else {
        return event;
    };
    format!(
        "{body},\"transaction\":{{\"id\":{},\"total_order\":{total_order},\"data_collection_order\":{collection_order}}}}}",
        json_string(transaction)
    )
}

fn captured_events(
    tables: &std::collections::HashMap<u32, crate::encoding::TableDefinition>,
    database: &str,
    transaction: &str,
    keys: &[CapturedKey],
    now_ms: i64,
) -> SqlResult<Vec<String>> {
    let id_at = crate::encoding::NAMESPACE.len() + 1;
    let mut events = Vec::new();
    let mut per_table: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
    for (key, before, after) in keys {
        let Some(id) = key.get(id_at..id_at + 4).and_then(|bytes| bytes.try_into().ok()).map(u32::from_be_bytes) else {
            continue;
        };
        let Some(definition) = tables.get(&id) else {
            continue;
        };
        let schema = crate::schemas::schema_of(&definition.name);
        let schema = if schema.is_empty() { crate::schemas::DEFAULT_SCHEMA } else { schema };
        if schema == "eggdb_cdc" {
            continue;
        }
        let table = crate::schemas::base_name(&definition.name);
        let op = match (before, after) {
            (None, Some(_)) => "c",
            (Some(_), Some(_)) => "u",
            (Some(_), None) => "d",
            (None, None) => continue,
        };
        let before = before.as_deref().map(|stored| row_json(definition, stored)).transpose()?;
        let after = after.as_deref().map(|stored| row_json(definition, stored)).transpose()?;
        let collection_order = per_table.entry(id).or_insert(0);
        *collection_order += 1;
        let event = debezium_event(database, schema, table, op, before.as_deref(), after.as_deref(), now_ms);
        events.push(with_transaction(event, transaction, events.len() as u64 + 1, *collection_order));
    }
    Ok(events)
}

/// Hands the row changes the engine captured without triggers (D1030), commit by commit and
/// in commit order, to `deliver` as Debezium-style JSON events with full `before` and `after`
/// rows, and moves its position past each commit once every event of it was accepted, removing
/// the records every consumer (it and the replication slots, D1031) has passed; stops at the
/// first refusal, so delivery is at least once and in order. Returns how many commits were
/// delivered.
pub fn deliver_captured_changes<S: KeyValueStore>(
    store: &S,
    database: &str,
    limit: u64,
    now_ms: i64,
    deliver: &mut dyn FnMut(&str) -> bool,
) -> SqlResult<u64> {
    let read = store.begin_read()?;
    let tables = table_definitions(&read)?;
    let position = crate::logical_decoding::cdc_position(&read)?;
    let mut start = CAPTURE_PREFIX.to_vec();
    start.extend_from_slice(&position.saturating_add(1).to_be_bytes());
    let end = crate::encoding::prefix_end(CAPTURE_PREFIX);
    let mut records: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    read.scan(&start, &end, crate::store::ScanDirection::Forward, &mut |key, value| {
        records.push((key.to_vec(), value.to_vec()));
        Ok((records.len() as u64) < limit)
    })?;
    drop(read);
    let mut delivered = 0;
    for (key, value) in records {
        let keys = decode_capture_record(&value).ok_or_else(|| crate::encoding::corrupt("captured changes"))?;
        let transaction = key
            .get(CAPTURE_PREFIX.len()..)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_be_bytes)
            .unwrap_or_default()
            .to_string();
        let events = captured_events(&tables, database, &transaction, &keys, now_ms)?;
        if !events.iter().all(|event| deliver(event)) {
            break;
        }
        let sequence = key.get(CAPTURE_PREFIX.len()..).and_then(|bytes| bytes.try_into().ok()).map(u64::from_be_bytes).unwrap_or(0);
        let mut write: Box<dyn crate::store::BoxedWrite + '_> = Box::new(store.begin_write()?);
        crate::logical_decoding::set_cdc_position(write.as_mut(), sequence)?;
        crate::logical_decoding::prune(write.as_mut(), 1_000)?;
        write.commit_boxed()?;
        delivered += 1;
    }
    Ok(delivered)
}

#[cfg(test)]
mod capture_tests {
    use super::*;

    #[test]
    fn capture_records_read_back() {
        let keys = vec![(b"k1".to_vec(), None, Some(b"v".to_vec())), (b"k2".to_vec(), Some(b"old".to_vec()), None)];
        assert_eq!(decode_capture_record(&encode_capture(&keys)), Some(keys));
        assert!(decode_capture_record(b"\x01").is_none());
    }
}
