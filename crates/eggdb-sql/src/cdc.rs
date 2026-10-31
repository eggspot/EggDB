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
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
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
pub fn debezium_event(database: &str, schema: &str, table: &str, op: &str, before: Option<&str>, after: Option<&str>, captured_ms: i64) -> String {
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

/// Hands up to `limit` of the oldest captured changes to `deliver`, in order, as Debezium-style
/// JSON; stops at the first one `deliver` refuses, and removes the ones it accepted. Returns
/// how many were delivered. A file without the extension has nothing to deliver.
pub fn deliver_changes<S: KeyValueStore>(store: &S, database: &str, limit: u64, deliver: &mut dyn FnMut(&str) -> bool) -> SqlResult<u64> {
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
        return if matches!(error.state(), SqlState::UNDEFINED_TABLE | SqlState::INVALID_SCHEMA_NAME) { Ok(0) } else { Err(error) };
    }
    let mut last_delivered = None;
    let mut delivered = 0;
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
        let event = debezium_event(database, &schema, &table, &op, before.as_deref(), after.as_deref(), captured_ms);
        if !deliver(&event) {
            break;
        }
        last_delivered = Some(*id);
        delivered += 1;
    }
    if let Some(last) = last_delivered {
        session.execute(&format!("DELETE FROM eggdb_cdc.outbox WHERE id <= {last}"), &mut |_| {})?;
    }
    session.close()?;
    Ok(delivered)
}

#[cfg(test)]
mod tests {
    use super::{debezium_event, deliver_changes};
    use crate::memory_store::MemoryStore;
    use crate::session::Session;

    #[test]
    fn captured_changes_are_delivered_once_in_order() {
        let store = MemoryStore::default();
        assert_eq!(deliver_changes(&store, "app", 10, &mut |_| true).unwrap(), 0);
        let mut session = Session::new(&store);
        for statement in [
            "CREATE EXTENSION eggdb_cdc",
            "CREATE TABLE accounts (id int PRIMARY KEY, balance int)",
            "CREATE TRIGGER accounts_cdc AFTER INSERT OR UPDATE OR DELETE ON accounts FOR EACH ROW EXECUTE FUNCTION eggdb_cdc.capture()",
            "INSERT INTO accounts VALUES (1, 100)",
            "UPDATE accounts SET balance = 50 WHERE id = 1",
            "DELETE FROM accounts WHERE id = 1",
        ] {
            session.execute(statement, &mut |_| {}).unwrap_or_else(|error| panic!("{statement}: {error}"));
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
        assert!(seen[0].contains("\"op\":\"c\"") && seen[0].contains("\"before\":null"), "{}", seen[0]);
        assert!(seen[1].contains("\"op\":\"u\"") && seen[1].contains("\"balance\": 100") || seen[1].contains("\"balance\":100"), "{}", seen[1]);
        assert!(seen[2].contains("\"op\":\"d\"") && seen[2].contains("\"after\":null"), "{}", seen[2]);
        assert_eq!(deliver_changes(&store, "app", 10, &mut |_| true).unwrap(), 0);
    }

    #[test]
    fn events_escape_names() {
        let event = debezium_event("a\"b", "public", "t", "c", None, Some("{}"), 5);
        assert!(event.starts_with("{\"before\":null,\"after\":{},\"source\":{\"connector\":\"eggdb\",\"db\":\"a\\\"b\""), "{event}");
    }
}
