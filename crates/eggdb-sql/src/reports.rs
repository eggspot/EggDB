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
CREATE TABLE eggdb_bi.models (model_name text PRIMARY KEY, source text NOT NULL, description text);
CREATE TABLE eggdb_bi.dimensions (model_name text NOT NULL REFERENCES eggdb_bi.models (model_name) ON DELETE CASCADE, dimension_name text NOT NULL, expression text NOT NULL, description text, PRIMARY KEY (model_name, dimension_name));
CREATE TABLE eggdb_bi.metrics (model_name text NOT NULL REFERENCES eggdb_bi.models (model_name) ON DELETE CASCADE, metric_name text NOT NULL, expression text NOT NULL, description text, PRIMARY KEY (model_name, metric_name));
CREATE FUNCTION eggdb_bi.metric_sql(model text, metric_names text[], dimension_names text[] DEFAULT '{}', filter text DEFAULT NULL, order_by text DEFAULT NULL, row_limit integer DEFAULT NULL) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    model_source text;
    chosen text[] := '{}';
    grouped text[] := '{}';
    wanted text;
    found text;
    built text;
BEGIN
    SELECT m.source INTO model_source FROM eggdb_bi.models m WHERE m.model_name = model;
    IF model_source IS NULL THEN
        RAISE EXCEPTION 'no model is named %', model USING ERRCODE = '42704';
    END IF;
    IF coalesce(array_length(metric_names, 1), 0) = 0 THEN
        RAISE EXCEPTION 'name at least one metric of %', model USING ERRCODE = '22023';
    END IF;
    FOR i IN 1..coalesce(array_length(dimension_names, 1), 0) LOOP
        wanted := dimension_names[i];
        SELECT d.expression INTO found FROM eggdb_bi.dimensions d WHERE d.model_name = model AND d.dimension_name = wanted;
        IF found IS NULL THEN
            RAISE EXCEPTION 'model % has no dimension %', model, wanted USING ERRCODE = '42703';
        END IF;
        chosen := chosen || format('%s AS %I', found, wanted);
        grouped := grouped || i::text;
    END LOOP;
    FOR i IN 1..array_length(metric_names, 1) LOOP
        wanted := metric_names[i];
        SELECT m.expression INTO found FROM eggdb_bi.metrics m WHERE m.model_name = model AND m.metric_name = wanted;
        IF found IS NULL THEN
            RAISE EXCEPTION 'model % has no metric %', model, wanted USING ERRCODE = '42703';
        END IF;
        chosen := chosen || format('%s AS %I', found, wanted);
    END LOOP;
    built := 'SELECT ' || array_to_string(chosen, ', ') || ' FROM (' || model_source || ') AS ' || quote_ident(model);
    IF filter IS NOT NULL THEN
        built := built || ' WHERE ' || filter;
    END IF;
    IF coalesce(array_length(grouped, 1), 0) > 0 THEN
        built := built || ' GROUP BY ' || array_to_string(grouped, ', ');
    END IF;
    IF order_by IS NOT NULL THEN
        built := built || ' ORDER BY ' || order_by;
    END IF;
    IF row_limit IS NOT NULL THEN
        built := built || ' LIMIT ' || row_limit;
    END IF;
    RETURN built;
END
$$;
CREATE FUNCTION eggdb_bi.metric_query(model text, metric_names text[], dimension_names text[] DEFAULT '{}', filter text DEFAULT NULL, order_by text DEFAULT NULL, row_limit integer DEFAULT NULL) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE
    found_rows jsonb;
BEGIN
    EXECUTE format('SELECT coalesce(jsonb_agg(to_jsonb(q)), ''[]''::jsonb) FROM (%s) q', eggdb_bi.metric_sql(model, metric_names, dimension_names, filter, order_by, row_limit)) INTO found_rows;
    RETURN found_rows;
END
$$;
";

/// Hands at most `limit` queued report messages, oldest first, to `deliver` as the target URL
/// and the JSON body, and removes those it accepted, stopping at the first it refuses so none is
/// skipped. Returns how many were delivered.
pub fn deliver_reports<S: KeyValueStore>(
    store: &S,
    limit: u64,
    deliver: &mut dyn FnMut(&str, &str) -> bool,
) -> SqlResult<u64> {
    let mut session = Session::with_dialect(store, Dialect::PostgreSql);
    let mut rows = Vec::new();
    let listed = session.execute(
        &format!(
            "SELECT id, target, body::text FROM eggdb_bi.deliveries ORDER BY id LIMIT {limit}"
        ),
        &mut |result| rows = result.rows,
    );
    if let Err(error) = listed {
        let _ = session.close();
        return if matches!(
            error.state(),
            SqlState::UNDEFINED_TABLE | SqlState::INVALID_SCHEMA_NAME
        ) {
            Ok(0)
        } else {
            Err(error)
        };
    }
    let mut last = None;
    let mut delivered = 0;
    for row in &rows {
        let (Some(Value::BigInt(id)), Some(target), Some(body)) = (
            row.first(),
            row.get(1).and_then(Value::to_text),
            row.get(2).and_then(Value::to_text),
        ) else {
            continue;
        };
        if !deliver(&target, &body) {
            break;
        }
        last = Some(*id);
        delivered += 1;
    }
    if let Some(last) = last {
        session.execute(
            &format!("DELETE FROM eggdb_bi.deliveries WHERE id <= {last}"),
            &mut |_| {},
        )?;
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
        assert!(
            seen[0].1.contains("\"count\": 2") && seen[0].1.contains("milk"),
            "{}",
            seen[0].1
        );
        assert!(
            seen[1].1.contains("eggs") && !seen[1].1.contains("milk"),
            "{}",
            seen[1].1
        );
        assert_eq!(deliver_reports(&store, 10, &mut |_, _| true).unwrap(), 0);
        let mut retry = Session::new(&store);
        retry
            .execute("SELECT eggdb_bi.run_report('low')", &mut |_| {})
            .unwrap();
        retry.close().unwrap();
        assert_eq!(deliver_reports(&store, 10, &mut |_, _| false).unwrap(), 0);
        assert_eq!(deliver_reports(&store, 10, &mut |_, _| true).unwrap(), 1);
    }
}
