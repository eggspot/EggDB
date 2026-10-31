pub(crate) const CATALOG_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_catalog;
CREATE TABLE eggdb_catalog.terms (term text PRIMARY KEY, definition text NOT NULL, steward text);
CREATE TABLE eggdb_catalog.tags (schema_name text NOT NULL, table_name text NOT NULL, column_name text NOT NULL DEFAULT '', term text NOT NULL REFERENCES eggdb_catalog.terms (term), PRIMARY KEY (schema_name, table_name, column_name, term));
CREATE TABLE eggdb_catalog.quality_rules (rule_name text PRIMARY KEY, schema_name text NOT NULL, table_name text NOT NULL, row_condition text NOT NULL, description text);
CREATE TABLE eggdb_catalog.quality_results (rule_name text NOT NULL, checked_at timestamptz NOT NULL DEFAULT now(), checked_rows bigint NOT NULL, failed_rows bigint NOT NULL);
CREATE TABLE eggdb_catalog.lineage (source_asset text NOT NULL, target_asset text NOT NULL, job_name text NOT NULL DEFAULT '', PRIMARY KEY (source_asset, target_asset, job_name));
CREATE FUNCTION eggdb_catalog.check_quality() RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    rule record;
    total bigint;
    failed bigint;
    broken bigint := 0;
BEGIN
    FOR rule IN SELECT * FROM eggdb_catalog.quality_rules ORDER BY rule_name LOOP
        EXECUTE format('SELECT count(*), coalesce(sum(CASE WHEN coalesce((%s), false) THEN 0 ELSE 1 END), 0) FROM %I.%I', rule.row_condition, rule.schema_name, rule.table_name) INTO total, failed;
        INSERT INTO eggdb_catalog.quality_results (rule_name, checked_rows, failed_rows) VALUES (rule.rule_name, total, failed);
        IF failed > 0 THEN
            broken := broken + 1;
        END IF;
    END LOOP;
    RETURN broken;
END
$$;
CREATE FUNCTION eggdb_catalog.upstream(asset text) RETURNS TABLE (upstream_asset text, hops integer) LANGUAGE plpgsql AS $$
BEGIN
    RETURN QUERY WITH RECURSIVE walked (found, distance) AS (
        SELECT l.source_asset, 1 FROM eggdb_catalog.lineage l WHERE l.target_asset = asset
        UNION
        SELECT l.source_asset, w.distance + 1 FROM eggdb_catalog.lineage l JOIN walked w ON l.target_asset = w.found WHERE w.distance < 100
    ) SELECT found, min(distance) FROM walked GROUP BY found ORDER BY 2, 1;
END
$$;
CREATE FUNCTION eggdb_catalog.openlineage_event(job text, event_type text) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE
    inputs jsonb;
    outputs jsonb;
BEGIN
    IF event_type NOT IN ('START', 'RUNNING', 'COMPLETE', 'ABORT', 'FAIL', 'OTHER') THEN
        RAISE EXCEPTION 'OpenLineage event type % is not START, RUNNING, COMPLETE, ABORT, FAIL or OTHER', event_type USING ERRCODE = '22023';
    END IF;
    SELECT coalesce(jsonb_agg(jsonb_build_object('namespace', 'eggdb', 'name', a) ORDER BY a), '[]'::jsonb) INTO inputs
        FROM (SELECT DISTINCT source_asset AS a FROM eggdb_catalog.lineage WHERE job_name = job) s;
    SELECT coalesce(jsonb_agg(jsonb_build_object('namespace', 'eggdb', 'name', a) ORDER BY a), '[]'::jsonb) INTO outputs
        FROM (SELECT DISTINCT target_asset AS a FROM eggdb_catalog.lineage WHERE job_name = job) t;
    RETURN jsonb_build_object(
        'eventType', event_type,
        'eventTime', to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
        'producer', 'https://github.com/eggspot/EggDB',
        'schemaURL', 'https://openlineage.io/spec/2-0-2/OpenLineage.json#/$defs/RunEvent',
        'run', jsonb_build_object('runId', gen_random_uuid()::text),
        'job', jsonb_build_object('namespace', 'eggdb', 'name', job),
        'inputs', inputs,
        'outputs', outputs);
END
$$;
"#;

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::tests::render;
    use crate::{Session, SqlError};

    fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<String, SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(render(&result)))?;
        Ok(outputs.pop().unwrap_or_default())
    }

    fn catalog() -> MemoryStore {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_catalog",
            "CREATE TABLE customers (id integer PRIMARY KEY, email text, country text)",
            "INSERT INTO customers VALUES (1, 'a@x.io', 'VN'), (2, NULL, 'SE'), (3, 'c@x.io', 'XX')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        store
    }

    #[test]
    fn quality_rules_count_the_rows_that_break_them() {
        let store = catalog();
        let mut session = Session::new(&store);
        for sql in [
            "INSERT INTO eggdb_catalog.quality_rules (rule_name, schema_name, table_name, row_condition) VALUES ('email present', 'public', 'customers', 'email IS NOT NULL'), ('known country', 'public', 'customers', 'country IN (''VN'', ''SE'')'), ('ids positive', 'public', 'customers', 'id > 0')",
        ] {
            run(&mut session, sql).unwrap();
        }
        assert_eq!(run(&mut session, "SELECT eggdb_catalog.check_quality() AS broken").unwrap(), "broken\n2\nSELECT 1");
        assert_eq!(
            run(&mut session, "SELECT rule_name, checked_rows, failed_rows FROM eggdb_catalog.quality_results ORDER BY rule_name").unwrap(),
            "rule_name|checked_rows|failed_rows\nemail present|3|1\nids positive|3|0\nknown country|3|1\nSELECT 3"
        );
    }

    #[test]
    fn glossary_terms_tag_columns_and_lineage_walks_upstream() {
        let store = catalog();
        let mut session = Session::new(&store);
        for sql in [
            "INSERT INTO eggdb_catalog.terms VALUES ('PII', 'Personal data under GDPR', 'privacy-team')",
            "INSERT INTO eggdb_catalog.tags VALUES ('public', 'customers', 'email', 'PII')",
            "INSERT INTO eggdb_catalog.lineage VALUES ('crm.contacts', 'public.customers', 'load_crm'), ('public.customers', 'mart.revenue', 'build_mart'), ('public.orders', 'mart.revenue', 'build_mart')",
        ] {
            run(&mut session, sql).unwrap();
        }
        assert!(run(&mut session, "INSERT INTO eggdb_catalog.tags VALUES ('public', 'customers', 'country', 'missing')").is_err());
        assert_eq!(
            run(&mut session, "SELECT * FROM eggdb_catalog.upstream('mart.revenue')").unwrap(),
            "upstream_asset|hops\npublic.customers|1\npublic.orders|1\ncrm.contacts|2\nSELECT 3"
        );
        assert_eq!(
            run(&mut session, "SELECT e -> 'inputs' AS inputs, e -> 'job' ->> 'name' AS job, e ->> 'eventType' AS type FROM eggdb_catalog.openlineage_event('build_mart', 'COMPLETE') AS e").unwrap(),
            "inputs|job|type\n[{\"name\": \"public.customers\", \"namespace\": \"eggdb\"}, {\"name\": \"public.orders\", \"namespace\": \"eggdb\"}]|build_mart|COMPLETE\nSELECT 1"
        );
        assert!(run(&mut session, "SELECT eggdb_catalog.openlineage_event('build_mart', 'DONE')").is_err());
    }
}
