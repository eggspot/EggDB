//! Scheduled reports and alerts (D238): `CREATE EXTENSION eggdb_bi` keeps named queries in
//! `eggdb_bi.reports`; `eggdb_bi.run_report(name)`, scheduled with pg_cron (D200), runs one and
//! queues its rows as a JSON message for the report's target, or only when there are rows for an
//! alert; [`deliver_reports`] hands queued messages to the host, which posts them, and removes
//! the ones delivered.

use crate::dialect::Dialect;
use crate::error::{SqlResult, SqlState};
use crate::session::Session;
use crate::store::KeyValueStore;
use crate::types::Value;

pub(crate) const BI_SCRIPT: &str = r"
CREATE SCHEMA eggdb_bi;
CREATE TABLE eggdb_bi.reports (
    name text PRIMARY KEY,
    query text NOT NULL,
    target text NOT NULL,
    only_when_rows boolean NOT NULL DEFAULT false
);
CREATE TABLE eggdb_bi.deliveries (
    id bigserial PRIMARY KEY,
    report text NOT NULL,
    target text NOT NULL,
    body jsonb NOT NULL,
    made_at timestamptz NOT NULL DEFAULT now()
);
CREATE FUNCTION eggdb_bi.run_report(report_name text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    report_query text;
    report_target text;
    alert boolean;
    found_rows jsonb;
    row_count bigint;
BEGIN
    SELECT r.query, r.target, r.only_when_rows INTO report_query, report_target, alert FROM eggdb_bi.reports r WHERE r.name = report_name;
    IF report_query IS NULL THEN
        RAISE EXCEPTION 'no report is named %', report_name USING ERRCODE = '42704';
    END IF;
    EXECUTE format('SELECT coalesce(jsonb_agg(to_jsonb(q)), ''[]''::jsonb), count(*) FROM (%s) q', report_query) INTO found_rows, row_count;
    IF row_count > 0 OR NOT alert THEN
        INSERT INTO eggdb_bi.deliveries (report, target, body) VALUES (report_name, report_target, jsonb_build_object('report', report_name, 'rows', found_rows, 'count', row_count, 'text', report_name || ': ' || row_count || ' rows'));
    END IF;
    RETURN row_count;
END
$$;
";

/// Hands at most `limit` queued report messages, oldest first, to `deliver` as the target URL
/// and the JSON body, and removes those it accepted, stopping at the first it refuses so none is
/// skipped. Returns how many were delivered.
pub fn deliver_reports<S: KeyValueStore>(store: &S, limit: u64, deliver: &mut dyn FnMut(&str, &str) -> bool) -> SqlResult<u64> {
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    let mut rows = Vec::new();
    let listed = session.execute(&format!("SELECT id, target, body::text FROM eggdb_bi.deliveries ORDER BY id LIMIT {limit}"), &mut |result| rows = result.rows);
    if let Err(error) = listed {
        let _ = session.close();
        return if matches!(error.state(), SqlState::UNDEFINED_TABLE | SqlState::INVALID_SCHEMA_NAME) { Ok(0) } else { Err(error) };
    }
    let mut last = None;
    let mut delivered = 0;
    for row in &rows {
        let (Some(Value::BigInt(id)), Some(target), Some(body)) = (row.first(), row.get(1).and_then(Value::to_text), row.get(2).and_then(Value::to_text)) else {
            continue;
        };
        if !deliver(&target, &body) {
            break;
        }
        last = Some(*id);
        delivered += 1;
    }
    if let Some(last) = last {
        session.execute(&format!("DELETE FROM eggdb_bi.deliveries WHERE id <= {last}"), &mut |_| {})?;
    }
    session.close()?;
    Ok(delivered)
}

#[cfg(test)]
mod tests {
    use super::deliver_reports;
    use crate::Session;
    use crate::memory_store::MemoryStore;

    #[test]
    fn reports_queue_their_rows_and_alerts_only_fire_with_rows() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        session
            .execute(
                "CREATE EXTENSION eggdb_bi; CREATE TABLE stock (item text, quantity integer); INSERT INTO stock VALUES ('eggs', 3), ('milk', 40);
                 INSERT INTO eggdb_bi.reports VALUES ('levels', 'SELECT item, quantity FROM stock ORDER BY item', 'http://hooks.local/levels', false);
                 INSERT INTO eggdb_bi.reports VALUES ('low', 'SELECT item FROM stock WHERE quantity < 5', 'http://hooks.local/low', true);
                 INSERT INTO eggdb_bi.reports VALUES ('empty', 'SELECT item FROM stock WHERE quantity < 0', 'http://hooks.local/empty', true);
                 SELECT eggdb_bi.run_report('levels'); SELECT eggdb_bi.run_report('low'); SELECT eggdb_bi.run_report('empty');",
                &mut |_| {},
            )
            .unwrap();
        session.close().unwrap();
        let mut seen = Vec::new();
        assert_eq!(
            deliver_reports(&store, 10, &mut |target, body| {
                seen.push((target.to_owned(), body.to_owned()));
                true
            })
            .unwrap(),
            2
        );
        assert_eq!(seen[0].0, "http://hooks.local/levels");
        assert!(seen[0].1.contains("\"count\": 2") && seen[0].1.contains("milk"), "{}", seen[0].1);
        assert!(seen[1].1.contains("eggs") && !seen[1].1.contains("milk"), "{}", seen[1].1);
        assert_eq!(deliver_reports(&store, 10, &mut |_, _| true).unwrap(), 0);
        let mut retry = Session::new(&store);
        retry.execute("SELECT eggdb_bi.run_report('low')", &mut |_| {}).unwrap();
        retry.close().unwrap();
        assert_eq!(deliver_reports(&store, 10, &mut |_, _| false).unwrap(), 0);
        assert_eq!(deliver_reports(&store, 10, &mut |_, _| true).unwrap(), 1);
    }
}
