/// The statements `CREATE EXTENSION eggdb_advisor` runs: an index advisor that reads the
/// statement statistics, finds the columns the costliest statements compare with a parameter,
/// asks the planner whether those tables are read by a sequential scan, and proposes the index
/// that would let them be looked up instead, with the time the statements spent (D25, D31).
pub(crate) const ADVISOR_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_advisor;
CREATE TABLE eggdb_advisor.recommendations (recommendation_id bigserial PRIMARY KEY, schema_name text NOT NULL, table_name text NOT NULL, column_name text NOT NULL, statements bigint NOT NULL, calls bigint NOT NULL, total_ms double precision NOT NULL, ddl text NOT NULL, found_at timestamptz NOT NULL DEFAULT now(), applied_at timestamptz, UNIQUE (schema_name, table_name, column_name));
CREATE FUNCTION eggdb_advisor.scans_sequentially(schema_name text, table_name text, column_name text) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE
    sample text;
    plan_line record;
    sequential boolean := false;
BEGIN
    EXECUTE format('SELECT %1$I::text FROM %2$I.%3$I WHERE %1$I IS NOT NULL LIMIT 1', column_name, schema_name, table_name) INTO sample;
    IF sample IS NULL THEN
        RETURN false;
    END IF;
    FOR plan_line IN EXECUTE format('EXPLAIN SELECT * FROM %2$I.%3$I WHERE %1$I = %4$L', column_name, schema_name, table_name, sample) LOOP
        IF (to_jsonb(plan_line) ->> 'QUERY PLAN') LIKE '%Seq Scan on ' || table_name || '%' THEN
            sequential := true;
        END IF;
    END LOOP;
    RETURN sequential;
END
$$;
CREATE FUNCTION eggdb_advisor.find_indexes(least_total_ms double precision DEFAULT 0, schema_name text DEFAULT 'public') RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    statement record;
    found_table text[];
    condition text;
    compared record;
    found bigint := 0;
BEGIN
    CREATE TEMPORARY TABLE IF NOT EXISTS eggdb_advisor_candidates (table_name text, column_name text, calls bigint, total_ms double precision);
    DELETE FROM eggdb_advisor_candidates;
    FOR statement IN SELECT s.query, s.calls, s.total_exec_time FROM pg_stat_statements s WHERE s.total_exec_time >= least_total_ms AND s.query ~* '^\s*(select|update|delete)' ORDER BY s.total_exec_time DESC LIMIT 500 LOOP
        found_table := regexp_match(statement.query, '(?:from|update)\s+"?([A-Za-z_][A-Za-z0-9_]*)"?(?:\s+(?:as\s+)?[A-Za-z_][A-Za-z0-9_]*)?\s+(?:set\s.*?\s)?where\s+(.*)$', 'i');
        CONTINUE WHEN found_table IS NULL;
        condition := found_table[2];
        FOR compared IN SELECT m[1] AS column_name FROM regexp_matches(condition, '(?:^|[\s(.])"?([A-Za-z_][A-Za-z0-9_]*)"?\s*(?:=|<=|>=|<|>|\s+between\s|\s+in\s*\()\s*\(?\$[0-9]+', 'gi') AS m LOOP
            INSERT INTO eggdb_advisor_candidates VALUES (lower(found_table[1]), lower(compared.column_name), statement.calls, statement.total_exec_time);
        END LOOP;
    END LOOP;
    FOR compared IN SELECT c.table_name, c.column_name, count(*) AS statements, sum(c.calls) AS calls, sum(c.total_ms) AS total_ms
            FROM eggdb_advisor_candidates c
            WHERE EXISTS (SELECT 1 FROM information_schema.columns i WHERE i.table_schema = find_indexes.schema_name AND i.table_name = c.table_name AND i.column_name = c.column_name)
            GROUP BY c.table_name, c.column_name ORDER BY sum(c.total_ms) DESC LOOP
        CONTINUE WHEN NOT eggdb_advisor.scans_sequentially(schema_name, compared.table_name, compared.column_name);
        INSERT INTO eggdb_advisor.recommendations (schema_name, table_name, column_name, statements, calls, total_ms, ddl)
            VALUES (find_indexes.schema_name, compared.table_name, compared.column_name, compared.statements, compared.calls, compared.total_ms,
                    format('CREATE INDEX ON %I.%I (%I)', find_indexes.schema_name, compared.table_name, compared.column_name))
            ON CONFLICT (schema_name, table_name, column_name) DO UPDATE SET statements = EXCLUDED.statements, calls = EXCLUDED.calls, total_ms = EXCLUDED.total_ms, found_at = now();
        found := found + 1;
    END LOOP;
    RETURN found;
END
$$;
CREATE FUNCTION eggdb_advisor.apply(recommendation bigint) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    chosen record;
BEGIN
    SELECT r.ddl, r.applied_at INTO chosen FROM eggdb_advisor.recommendations r WHERE r.recommendation_id = recommendation;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'there is no recommendation %', recommendation USING ERRCODE = '42704';
    END IF;
    IF chosen.applied_at IS NOT NULL THEN
        RETURN chosen.ddl;
    END IF;
    EXECUTE chosen.ddl;
    UPDATE eggdb_advisor.recommendations SET applied_at = now() WHERE recommendation_id = recommendation;
    RETURN chosen.ddl;
END
$$;
CREATE FUNCTION eggdb_advisor.apply_all(least_total_ms double precision) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    chosen record;
    applied bigint := 0;
BEGIN
    FOR chosen IN SELECT r.recommendation_id FROM eggdb_advisor.recommendations r WHERE r.applied_at IS NULL AND r.total_ms >= least_total_ms ORDER BY r.total_ms DESC LOOP
        PERFORM eggdb_advisor.apply(chosen.recommendation_id);
        applied := applied + 1;
    END LOOP;
    RETURN applied;
END
$$;
"#;

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::tests::render;
    use crate::{Session, SqlError};

    fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(render(&result)))?;
        Ok(outputs.pop().unwrap_or_default())
    }

    #[test]
    fn a_sequentially_scanned_lookup_column_is_proposed_and_indexed() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_advisor",
            "CREATE TABLE orders (id integer PRIMARY KEY, customer text, total numeric)",
            "INSERT INTO orders VALUES (1, 'ada', 10), (2, 'bo', 20)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT eggdb_advisor.scans_sequentially('public', 'orders', 'customer') AS seq").unwrap(), "seq\nt\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT eggdb_advisor.scans_sequentially('public', 'orders', 'id') AS seq").unwrap(), "seq\nf\nSELECT 1");
        run(&mut session, "INSERT INTO eggdb_advisor.recommendations (schema_name, table_name, column_name, statements, calls, total_ms, ddl) VALUES ('public', 'orders', 'customer', 1, 100, 500, 'CREATE INDEX ON public.orders (customer)')").unwrap();
        run(&mut session, "SELECT eggdb_advisor.apply(1)").unwrap();
        assert_eq!(run(&mut session, "SELECT eggdb_advisor.scans_sequentially('public', 'orders', 'customer') AS seq").unwrap(), "seq\nf\nSELECT 1");
    }
}
