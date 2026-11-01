pub(crate) const MIGRATION_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_migration;
CREATE TABLE eggdb_migration.jobs (job_name text PRIMARY KEY, source_table text NOT NULL, target_table text NOT NULL, copied_rows bigint NOT NULL DEFAULT 0, batches bigint NOT NULL DEFAULT 0, updated_at timestamptz NOT NULL DEFAULT now());
CREATE FUNCTION eggdb_migration.copy_batch(job text, source_schema text, source_table text, target_schema text, target_table text, key_column text, batch_rows integer) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    copied bigint;
BEGIN
    IF batch_rows < 1 OR batch_rows > 1000000 THEN
        RAISE EXCEPTION 'a batch is from 1 to 1000000 rows, not %', batch_rows USING ERRCODE = '22023';
    END IF;
    EXECUTE format('INSERT INTO %I.%I SELECT * FROM %I.%I s WHERE NOT EXISTS (SELECT 1 FROM %I.%I) OR s.%I > (SELECT max(%I) FROM %I.%I) ORDER BY s.%I LIMIT %s',
        target_schema, target_table, source_schema, source_table, target_schema, target_table, key_column, key_column, target_schema, target_table, key_column, batch_rows);
    GET DIAGNOSTICS copied = ROW_COUNT;
    UPDATE eggdb_migration.jobs SET copied_rows = copied_rows + copied, batches = batches + 1, updated_at = now() WHERE job_name = job;
    IF NOT FOUND THEN
        INSERT INTO eggdb_migration.jobs (job_name, source_table, target_table, copied_rows, batches) VALUES (job, source_schema || '.' || source_table, target_schema || '.' || target_table, copied, 1);
    END IF;
    RETURN copied;
END
$$;
CREATE FUNCTION eggdb_migration.compare(source_schema text, source_table text, target_schema text, target_table text, key_column text) RETURNS TABLE (row_key text, difference text) LANGUAGE plpgsql AS $$
DECLARE
    checked record;
BEGIN
    FOR checked IN EXECUTE format('SELECT s.%I::text AS k FROM %I.%I s WHERE NOT EXISTS (SELECT 1 FROM %I.%I t WHERE t.%I = s.%I) ORDER BY s.%I', key_column, source_schema, source_table, target_schema, target_table, key_column, key_column, key_column) LOOP
        row_key := checked.k;
        difference := 'missing in target';
        RETURN NEXT;
    END LOOP;
    FOR checked IN EXECUTE format('SELECT t.%I::text AS k FROM %I.%I t WHERE NOT EXISTS (SELECT 1 FROM %I.%I s WHERE s.%I = t.%I) ORDER BY t.%I', key_column, target_schema, target_table, source_schema, source_table, key_column, key_column, key_column) LOOP
        row_key := checked.k;
        difference := 'missing in source';
        RETURN NEXT;
    END LOOP;
    FOR checked IN EXECUTE format('SELECT s.%I::text AS k, to_jsonb(s) AS source_row, to_jsonb(t) AS target_row FROM %I.%I s JOIN %I.%I t ON t.%I = s.%I ORDER BY s.%I', key_column, source_schema, source_table, target_schema, target_table, key_column, key_column, key_column) LOOP
        IF checked.source_row <> checked.target_row THEN
            row_key := checked.k;
            difference := 'different';
            RETURN NEXT;
        END IF;
    END LOOP;
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
    fn a_migration_copies_in_resumable_batches_and_verifies_the_result() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_migration",
            "CREATE SCHEMA legacy",
            "CREATE TABLE legacy.accounts (id integer PRIMARY KEY, owner text, balance numeric)",
            "CREATE TABLE accounts (id integer PRIMARY KEY, owner text, balance numeric)",
            "INSERT INTO legacy.accounts SELECT n, 'owner ' || n, n * 10 FROM generate_series(1, 25) AS n",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        let batch = "SELECT eggdb_migration.copy_batch('accounts', 'legacy', 'accounts', 'public', 'accounts', 'id', 10) AS copied";
        assert_eq!(run(&mut session, batch).unwrap(), "copied\n10\nSELECT 1");
        assert_eq!(
            run(&mut session, "SELECT count(*) AS differences FROM eggdb_migration.compare('legacy', 'accounts', 'public', 'accounts', 'id')").unwrap(),
            "differences\n15\nSELECT 1"
        );
        assert_eq!(run(&mut session, batch).unwrap(), "copied\n10\nSELECT 1");
        assert_eq!(run(&mut session, batch).unwrap(), "copied\n5\nSELECT 1");
        assert_eq!(run(&mut session, batch).unwrap(), "copied\n0\nSELECT 1");
        assert_eq!(
            run(
                &mut session,
                "SELECT copied_rows, batches FROM eggdb_migration.jobs"
            )
            .unwrap(),
            "copied_rows|batches\n25|4\nSELECT 1"
        );
        run(&mut session, "UPDATE accounts SET balance = 0 WHERE id = 7").unwrap();
        run(&mut session, "DELETE FROM accounts WHERE id = 9").unwrap();
        run(&mut session, "INSERT INTO accounts VALUES (99, 'stray', 1)").unwrap();
        assert_eq!(
            run(&mut session, "SELECT * FROM eggdb_migration.compare('legacy', 'accounts', 'public', 'accounts', 'id')").unwrap(),
            "row_key|difference\n9|missing in target\n99|missing in source\n7|different\nSELECT 3"
        );
        assert!(run(&mut session, "SELECT eggdb_migration.copy_batch('accounts', 'legacy', 'accounts', 'public', 'accounts', 'id', 0)").is_err());
    }
}
