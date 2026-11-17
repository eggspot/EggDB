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
CREATE TABLE eggdb_advisor.advice (advice_id bigserial PRIMARY KEY, kind text NOT NULL, target text NOT NULL, detail text NOT NULL, ddl text, total_ms double precision NOT NULL DEFAULT 0, found_at timestamptz NOT NULL DEFAULT now(), UNIQUE (kind, target));
CREATE FUNCTION eggdb_advisor.find_unused_indexes(schema_name text DEFAULT 'public') RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    unused record;
    found bigint := 0;
BEGIN
    FOR unused IN SELECT i.relname, i.indexrelname FROM pg_stat_user_indexes i
            WHERE i.schemaname = find_unused_indexes.schema_name AND i.idx_scan = 0
              AND NOT EXISTS (SELECT 1 FROM information_schema.table_constraints c WHERE c.table_schema = i.schemaname AND c.constraint_name = i.indexrelname AND c.constraint_type IN ('PRIMARY KEY', 'UNIQUE'))
            ORDER BY 1, 2 LOOP
        INSERT INTO eggdb_advisor.advice (kind, target, detail, ddl)
            VALUES ('unused index', format('%s.%s', find_unused_indexes.schema_name, unused.indexrelname),
                    format('index %s on %s has not been used since the statistics were reset; every write still updates it', unused.indexrelname, unused.relname),
                    format('DROP INDEX %I.%I', find_unused_indexes.schema_name, unused.indexrelname))
            ON CONFLICT (kind, target) DO UPDATE SET detail = EXCLUDED.detail, found_at = now();
        found := found + 1;
    END LOOP;
    RETURN found;
END
$$;
CREATE FUNCTION eggdb_advisor.find_plan_advice(least_total_ms double precision DEFAULT 0) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    statement record;
    shape text;
    advice text;
    found bigint := 0;
BEGIN
    FOR statement IN SELECT s.query, s.calls, s.total_exec_time FROM pg_stat_statements s WHERE s.total_exec_time >= least_total_ms ORDER BY s.total_exec_time DESC LIMIT 500 LOOP
        shape := lower(statement.query);
        FOR advice IN SELECT unnest(ARRAY[
                CASE WHEN shape ~ '^\s*select\s+\*\s+from' THEN 'reads every column: list the columns it needs, so fewer bytes are read and sent' END,
                CASE WHEN shape ~ '\soffset\s+\$[0-9]+' THEN 'pages with OFFSET, which reads and drops every skipped row: page by the last key seen instead (WHERE key > $1 ORDER BY key LIMIT n)' END,
                CASE WHEN shape ~ '\snot\s+in\s*\(\s*select' THEN 'uses NOT IN with a subquery, which a NULL in it makes return nothing: use NOT EXISTS' END,
                CASE WHEN shape ~ '^\s*(update|delete)' AND shape !~ '\swhere\s' THEN 'changes every row of its table: check it is meant, and batch it if the table is large' END,
                CASE WHEN shape ~ '^\s*select\s+count\(\*\)\s+from\s+[a-z_."]+\s*$' THEN 'counts a whole table on every call: keep the count in a summary table or use the planner''s estimate when an exact count is not needed' END,
                CASE WHEN shape ~ 'lower\(\s*"?[a-z_]+"?\s*\)\s*=\s*\$[0-9]+' THEN 'compares lower(column): an expression index on lower(column), or a citext column, lets it use an index' END
            ]) AS a WHERE a IS NOT NULL LOOP
            INSERT INTO eggdb_advisor.advice (kind, target, detail, total_ms)
                VALUES ('plan', left(statement.query, 200), advice, statement.total_exec_time)
                ON CONFLICT (kind, target) DO UPDATE SET detail = EXCLUDED.detail, total_ms = EXCLUDED.total_ms, found_at = now();
            found := found + 1;
        END LOOP;
    END LOOP;
    RETURN found;
END
$$;
CREATE FUNCTION eggdb_advisor.configuration_advice() RETURNS TABLE (setting text, current text, recommended text, reason text) LANGUAGE plpgsql AS $$
DECLARE
    value text;
BEGIN
    SELECT s.setting INTO value FROM pg_settings s WHERE s.name = 'statement_timeout';
    IF value IS NOT NULL AND value IN ('0', '0ms') THEN
        setting := 'statement_timeout'; current := value; recommended := '5min';
        reason := 'no statement is ever stopped, so one runaway query can hold a connection for good';
        RETURN NEXT;
    END IF;
    SELECT s.setting INTO value FROM pg_settings s WHERE s.name = 'idle_in_transaction_session_timeout';
    IF value IS NOT NULL AND value IN ('0', '0ms') THEN
        setting := 'idle_in_transaction_session_timeout'; current := value; recommended := '10min';
        reason := 'a client that opens a transaction and goes quiet keeps old row versions alive and its locks held';
        RETURN NEXT;
    END IF;
    SELECT s.setting INTO value FROM pg_settings s WHERE s.name = 'log_min_duration_statement';
    IF value IS NOT NULL AND value IN ('-1') THEN
        setting := 'log_min_duration_statement'; current := value; recommended := '1s';
        reason := 'slow statements are not logged, so the slowest work goes unseen';
        RETURN NEXT;
    END IF;
    IF (SELECT count(*) FROM pg_stat_statements) = 0 THEN
        setting := 'pg_stat_statements'; current := 'empty'; recommended := 'collect';
        reason := 'the advisor learns from statement statistics; run the workload, then ask again';
        RETURN NEXT;
    END IF;
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
    fn an_index_nobody_uses_is_proposed_for_dropping_and_settings_are_reviewed() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_advisor",
            "CREATE TABLE orders (id integer PRIMARY KEY, customer text)",
            "CREATE INDEX orders_customer ON orders (customer)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT eggdb_advisor.find_unused_indexes() AS found").unwrap(), "found\n1\nSELECT 1");
        assert_eq!(
            run(&mut session, "SELECT kind, ddl FROM eggdb_advisor.advice").unwrap(),
            "kind|ddl\nunused index|DROP INDEX public.orders_customer\nSELECT 1"
        );
        run(&mut session, "SELECT setting FROM eggdb_advisor.configuration_advice()").unwrap();
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
