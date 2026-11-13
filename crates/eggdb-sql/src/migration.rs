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
CREATE TABLE eggdb_migration.ranges (job_name text NOT NULL, piece integer NOT NULL, low_key text, high_key text, claimed_by text, done boolean NOT NULL DEFAULT false, copied_rows bigint NOT NULL DEFAULT 0, PRIMARY KEY (job_name, piece));
CREATE FUNCTION eggdb_migration.split_ranges(job text, schema_name text, table_name text, key_column text, pieces integer) RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
    checked record;
    made integer := 0;
BEGIN
    IF pieces < 1 OR pieces > 100000 THEN
        RAISE EXCEPTION 'a job is split into 1 to 100000 ranges, not %', pieces USING ERRCODE = '22023';
    END IF;
    DELETE FROM eggdb_migration.ranges WHERE job_name = job;
    FOR checked IN EXECUTE format('SELECT min(k)::text AS low FROM (SELECT s.%I AS k, ntile(%s) OVER (ORDER BY s.%I) AS piece FROM %I.%I s) AS split GROUP BY piece ORDER BY min(k)', key_column, pieces, key_column, schema_name, table_name) LOOP
        IF made > 0 THEN
            UPDATE eggdb_migration.ranges SET high_key = checked.low WHERE job_name = job AND piece = made;
        END IF;
        made := made + 1;
        INSERT INTO eggdb_migration.ranges (job_name, piece, low_key, high_key) VALUES (job, made, CASE WHEN made = 1 THEN NULL ELSE checked.low END, NULL);
    END LOOP;
    IF made = 0 THEN
        INSERT INTO eggdb_migration.ranges (job_name, piece, low_key, high_key) VALUES (job, 1, NULL, NULL);
        made := 1;
    END IF;
    RETURN made;
END
$$;
CREATE FUNCTION eggdb_migration.claim_range(job text, worker text) RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
    candidate integer;
    taken bigint;
BEGIN
    SELECT min(piece) INTO candidate FROM eggdb_migration.ranges WHERE job_name = job AND claimed_by IS NULL AND NOT done;
    IF candidate IS NULL THEN
        RETURN NULL;
    END IF;
    UPDATE eggdb_migration.ranges SET claimed_by = worker WHERE job_name = job AND piece = candidate AND claimed_by IS NULL;
    GET DIAGNOSTICS taken = ROW_COUNT;
    IF taken = 1 THEN
        RETURN candidate;
    END IF;
    RETURN NULL;
END
$$;
CREATE FUNCTION eggdb_migration.within(alias text, key_column text, low_key text, high_key text) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
    RETURN format('(%L IS NULL OR %I.%I >= %L) AND (%L IS NULL OR %I.%I < %L)', low_key, alias, key_column, low_key, high_key, alias, key_column, high_key);
END
$$;
CREATE FUNCTION eggdb_migration.copy_range_batch(job text, range_piece integer, source_schema text, source_table text, target_schema text, target_table text, key_column text, batch_rows integer) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    low text;
    high text;
    copied bigint;
BEGIN
    IF batch_rows < 1 OR batch_rows > 1000000 THEN
        RAISE EXCEPTION 'a batch is from 1 to 1000000 rows, not %', batch_rows USING ERRCODE = '22023';
    END IF;
    SELECT low_key, high_key INTO low, high FROM eggdb_migration.ranges WHERE job_name = job AND piece = range_piece;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'job % has no range %', job, range_piece USING ERRCODE = '22023';
    END IF;
    EXECUTE format('INSERT INTO %I.%I SELECT * FROM %I.%I s WHERE %s AND (NOT EXISTS (SELECT 1 FROM %I.%I t WHERE %s) OR s.%I > (SELECT max(t.%I) FROM %I.%I t WHERE %s)) ORDER BY s.%I LIMIT %s',
        target_schema, target_table, source_schema, source_table, eggdb_migration.within('s', key_column, low, high),
        target_schema, target_table, eggdb_migration.within('t', key_column, low, high),
        key_column, key_column, target_schema, target_table, eggdb_migration.within('t', key_column, low, high),
        key_column, batch_rows);
    GET DIAGNOSTICS copied = ROW_COUNT;
    UPDATE eggdb_migration.ranges SET copied_rows = copied_rows + copied, done = (copied = 0) WHERE job_name = job AND piece = range_piece;
    RETURN copied;
END
$$;
CREATE FUNCTION eggdb_migration.range_digest(schema_name text, table_name text, key_column text, low_key text, high_key text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    checked record;
    digest text;
BEGIN
    FOR checked IN EXECUTE format('SELECT count(*) AS n, md5(coalesce(string_agg(to_jsonb(s)::text, %L ORDER BY s.%I), %L)) AS d FROM %I.%I s WHERE %s', chr(10), key_column, '', schema_name, table_name, eggdb_migration.within('s', key_column, low_key, high_key)) LOOP
        digest := checked.n || ':' || checked.d;
    END LOOP;
    RETURN digest;
END
$$;
CREATE FUNCTION eggdb_migration.compare_ranges(job text, source_schema text, source_table text, target_schema text, target_table text, key_column text) RETURNS TABLE (piece integer, low_key text, high_key text, source_digest text, target_digest text) LANGUAGE plpgsql AS $$
DECLARE
    checked record;
BEGIN
    FOR checked IN SELECT r.piece AS p, r.low_key AS l, r.high_key AS h FROM eggdb_migration.ranges r WHERE r.job_name = job ORDER BY r.piece LOOP
        source_digest := eggdb_migration.range_digest(source_schema, source_table, key_column, checked.l, checked.h);
        target_digest := eggdb_migration.range_digest(target_schema, target_table, key_column, checked.l, checked.h);
        IF source_digest IS DISTINCT FROM target_digest THEN
            piece := checked.p;
            low_key := checked.l;
            high_key := checked.h;
            RETURN NEXT;
        END IF;
    END LOOP;
END
$$;
CREATE FUNCTION eggdb_migration.compare_range(source_schema text, source_table text, target_schema text, target_table text, key_column text, low_key text, high_key text) RETURNS TABLE (row_key text, difference text) LANGUAGE plpgsql AS $$
DECLARE
    checked record;
BEGIN
    FOR checked IN EXECUTE format('SELECT s.%I::text AS k FROM %I.%I s WHERE %s AND NOT EXISTS (SELECT 1 FROM %I.%I t WHERE t.%I = s.%I) ORDER BY s.%I', key_column, source_schema, source_table, eggdb_migration.within('s', key_column, low_key, high_key), target_schema, target_table, key_column, key_column, key_column) LOOP
        row_key := checked.k;
        difference := 'missing in target';
        RETURN NEXT;
    END LOOP;
    FOR checked IN EXECUTE format('SELECT t.%I::text AS k FROM %I.%I t WHERE %s AND NOT EXISTS (SELECT 1 FROM %I.%I s WHERE s.%I = t.%I) ORDER BY t.%I', key_column, target_schema, target_table, eggdb_migration.within('t', key_column, low_key, high_key), source_schema, source_table, key_column, key_column, key_column) LOOP
        row_key := checked.k;
        difference := 'missing in source';
        RETURN NEXT;
    END LOOP;
    FOR checked IN EXECUTE format('SELECT s.%I::text AS k, to_jsonb(s) AS source_row, to_jsonb(t) AS target_row FROM %I.%I s JOIN %I.%I t ON t.%I = s.%I WHERE %s ORDER BY s.%I', key_column, source_schema, source_table, target_schema, target_table, key_column, key_column, eggdb_migration.within('s', key_column, low_key, high_key), key_column) LOOP
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

    #[test]
    fn ranges_are_claimed_copied_and_compared_by_digest() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_migration",
            "CREATE SCHEMA legacy",
            "CREATE TABLE legacy.events (id integer PRIMARY KEY, body text)",
            "CREATE TABLE events (id integer PRIMARY KEY, body text)",
            "INSERT INTO legacy.events SELECT n, 'event ' || n FROM generate_series(1, 40) AS n",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT eggdb_migration.split_ranges('events', 'legacy', 'events', 'id', 4) AS ranges").unwrap(),
            "ranges\n4\nSELECT 1"
        );
        for worker in ["node a", "node b", "node a", "node b"] {
            let piece = run(&mut session, &format!("SELECT eggdb_migration.claim_range('events', '{worker}') AS piece")).unwrap();
            let piece: i64 = piece.lines().nth(1).unwrap().parse().unwrap();
            loop {
                let copied = run(
                    &mut session,
                    &format!("SELECT eggdb_migration.copy_range_batch('events', {piece}, 'legacy', 'events', 'public', 'events', 'id', 4) AS copied"),
                )
                .unwrap();
                if copied.lines().nth(1) == Some("0") {
                    break;
                }
            }
        }
        assert_eq!(
            run(&mut session, "SELECT eggdb_migration.claim_range('events', 'node c') AS piece").unwrap(),
            "piece\n\nSELECT 1"
        );
        assert_eq!(run(&mut session, "SELECT count(*) AS rows FROM events").unwrap(), "rows\n40\nSELECT 1");
        assert_eq!(
            run(&mut session, "SELECT count(*) AS differing FROM eggdb_migration.compare_ranges('events', 'legacy', 'events', 'public', 'events', 'id')").unwrap(),
            "differing\n0\nSELECT 1"
        );
        run(&mut session, "UPDATE events SET body = 'changed' WHERE id = 33").unwrap();
        let differing = run(&mut session, "SELECT piece, low_key, high_key FROM eggdb_migration.compare_ranges('events', 'legacy', 'events', 'public', 'events', 'id')").unwrap();
        assert_eq!(differing.lines().count(), 3, "{differing}");
        let (low, high) = {
            let row: Vec<&str> = differing.lines().nth(1).unwrap().split('|').collect();
            (row[1].to_owned(), row[2].to_owned())
        };
        let high = if high.is_empty() { "NULL".to_owned() } else { format!("'{high}'") };
        assert_eq!(
            run(&mut session, &format!("SELECT * FROM eggdb_migration.compare_range('legacy', 'events', 'public', 'events', 'id', '{low}', {high})")).unwrap(),
            "row_key|difference\n33|different\nSELECT 1"
        );
    }
}
