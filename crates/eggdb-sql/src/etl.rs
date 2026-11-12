/// The statements `CREATE EXTENSION eggdb_etl` runs: scheduled copies from the sources
/// `eggdb_foreign` reads (PostgreSQL and EggDB servers, SQL Server, Parquet and CSV files)
/// into tables, replacing, appending or loading only rows past a cursor, with each run logged
/// (D46, D229).
pub(crate) const ETL_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_etl;
CREATE TABLE eggdb_etl.copies (copy_name text PRIMARY KEY, source text NOT NULL, query text NOT NULL, target_table text NOT NULL, mode text NOT NULL DEFAULT 'replace' CHECK (mode IN ('replace', 'append', 'incremental')), cursor_column text, last_value text, created_at timestamptz NOT NULL DEFAULT now(), CHECK (mode <> 'incremental' OR (cursor_column IS NOT NULL AND last_value IS NOT NULL)));
CREATE TABLE eggdb_etl.runs (run_id bigserial PRIMARY KEY, copy_name text NOT NULL REFERENCES eggdb_etl.copies (copy_name), started_at timestamptz NOT NULL DEFAULT now(), finished_at timestamptz, rows_copied bigint, state text NOT NULL DEFAULT 'running' CHECK (state IN ('running', 'succeeded', 'failed')), message text);
CREATE FUNCTION eggdb_etl.define_copy(copy_name text, source text, query text, target_table text, mode text DEFAULT 'replace', cursor_column text DEFAULT NULL, start_value text DEFAULT NULL, schedule text DEFAULT NULL) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    IF mode = 'incremental' AND position('{last_value}' IN query) = 0 THEN
        RAISE EXCEPTION 'an incremental copy''s query needs {last_value} where the cursor goes' USING ERRCODE = '22023';
    END IF;
    INSERT INTO eggdb_etl.copies (copy_name, source, query, target_table, mode, cursor_column, last_value)
        VALUES (define_copy.copy_name, define_copy.source, define_copy.query, define_copy.target_table, define_copy.mode, define_copy.cursor_column, define_copy.start_value);
    IF schedule IS NOT NULL THEN
        IF NOT EXISTS (SELECT 1 FROM information_schema.tables t WHERE t.table_schema = 'cron' AND t.table_name = 'job') THEN
            RAISE EXCEPTION 'a scheduled copy needs CREATE EXTENSION pg_cron' USING ERRCODE = '55000';
        END IF;
        PERFORM cron.schedule('eggdb_etl ' || define_copy.copy_name, schedule, format('SELECT eggdb_etl.run(%L)', define_copy.copy_name));
    END IF;
END
$$;
CREATE FUNCTION eggdb_etl.run(wanted_copy text, batch_rows integer DEFAULT 1000) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    definition record;
    current_run bigint;
    wanted_query text;
    found_row record;
    batch jsonb := '[]'::jsonb;
    batched integer := 0;
    copied bigint := 0;
    newest text;
BEGIN
    SELECT * INTO definition FROM eggdb_etl.copies c WHERE c.copy_name = wanted_copy;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'there is no copy %', wanted_copy USING ERRCODE = '42704';
    END IF;
    INSERT INTO eggdb_etl.runs (copy_name) VALUES (wanted_copy) RETURNING runs.run_id INTO current_run;
    BEGIN
        wanted_query := CASE WHEN definition.mode = 'incremental' THEN replace(definition.query, '{last_value}', definition.last_value) ELSE definition.query END;
        IF definition.mode = 'replace' THEN
            EXECUTE format('DELETE FROM %s', definition.target_table);
        END IF;
        FOR found_row IN SELECT f.o::jsonb AS body FROM eggdb_foreign(definition.source, wanted_query) AS f(o) LOOP
            batch := batch || jsonb_build_array(found_row.body);
            batched := batched + 1;
            IF batched >= greatest(batch_rows, 1) THEN
                EXECUTE format('INSERT INTO %s SELECT * FROM jsonb_populate_recordset(NULL::%s, %L::jsonb)', definition.target_table, definition.target_table, batch::text);
                copied := copied + batched;
                batch := '[]'::jsonb;
                batched := 0;
            END IF;
        END LOOP;
        IF batched > 0 THEN
            EXECUTE format('INSERT INTO %s SELECT * FROM jsonb_populate_recordset(NULL::%s, %L::jsonb)', definition.target_table, definition.target_table, batch::text);
            copied := copied + batched;
        END IF;
        IF definition.mode = 'incremental' THEN
            EXECUTE format('SELECT max(%I)::text FROM %s', definition.cursor_column, definition.target_table) INTO newest;
            UPDATE eggdb_etl.copies SET last_value = coalesce(newest, last_value) WHERE copy_name = wanted_copy;
        END IF;
        UPDATE eggdb_etl.runs SET finished_at = now(), rows_copied = copied, state = 'succeeded' WHERE run_id = current_run;
    EXCEPTION WHEN OTHERS THEN
        UPDATE eggdb_etl.runs SET finished_at = now(), rows_copied = 0, state = 'failed', message = SQLSTATE || ': ' || SQLERRM WHERE run_id = current_run;
        RETURN NULL;
    END;
    RETURN copied;
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
    fn a_failed_copy_is_logged_and_incremental_copies_need_a_cursor() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_etl",
            "CREATE TABLE orders (id integer PRIMARY KEY, total numeric)",
            "SELECT eggdb_etl.define_copy('crm orders', 'postgresql://reader@crm.internal/sales', 'SELECT id, total FROM orders', 'orders')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert!(run(&mut session, "SELECT eggdb_etl.define_copy('bad', 'postgresql://reader@crm.internal/sales', 'SELECT id FROM orders', 'orders', 'incremental', 'id', '0')").is_err());
        assert_eq!(run(&mut session, "SELECT eggdb_etl.run('crm orders') IS NULL AS failed").unwrap(), "failed\nt\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT state, rows_copied FROM eggdb_etl.runs").unwrap(), "state|rows_copied\nfailed|0\nSELECT 1");
    }
}
