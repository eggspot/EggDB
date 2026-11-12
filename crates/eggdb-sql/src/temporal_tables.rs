pub(crate) const REGISTRY_SCHEMA: &str = "eggdb_temporal";
pub(crate) const REGISTRY_TABLE: &str = "tables";
pub(crate) const SYSTEM_START: &str = "recorded_from";
pub(crate) const SYSTEM_END: &str = "recorded_to";

/// The statements `CREATE EXTENSION eggdb_temporal` runs: bitemporal tables, with a valid period
/// the application states and a recorded period the database keeps, so a row can be read as it
/// was true at any time and as it was known at any time (D50).
pub(crate) const TEMPORAL_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_temporal;
CREATE TABLE eggdb_temporal.tables (schema_name text NOT NULL, table_name text NOT NULL, key_columns text[] NOT NULL, history_table text NOT NULL, versions_view text NOT NULL, enabled_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (schema_name, table_name));
CREATE FUNCTION eggdb_temporal.enable(schema_name text, table_name text, key_columns text[]) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    history_name text := table_name || '_history';
    versions_name text := table_name || '_versions';
    function_name text := 'track_' || schema_name || '_' || table_name;
    column_list text;
    old_values text;
    key_match text;
    self_match text;
    body text;
BEGIN
    IF coalesce(array_length(key_columns, 1), 0) = 0 THEN
        RAISE EXCEPTION 'a bitemporal table needs at least one key column' USING ERRCODE = '22023';
    END IF;
    EXECUTE format('ALTER TABLE %I.%I ADD COLUMN IF NOT EXISTS valid_from timestamptz NOT NULL DEFAULT %L', schema_name, table_name, '-infinity');
    EXECUTE format('ALTER TABLE %I.%I ADD COLUMN IF NOT EXISTS valid_to timestamptz NOT NULL DEFAULT %L', schema_name, table_name, 'infinity');
    EXECUTE format('ALTER TABLE %I.%I ADD COLUMN IF NOT EXISTS recorded_from timestamptz NOT NULL DEFAULT now()', schema_name, table_name);
    EXECUTE format('ALTER TABLE %I.%I ADD COLUMN IF NOT EXISTS recorded_to timestamptz NOT NULL DEFAULT %L', schema_name, table_name, 'infinity');
    EXECUTE format('CREATE TABLE %I.%I AS SELECT * FROM %I.%I WHERE false', schema_name, history_name, schema_name, table_name);
    SELECT string_agg(format('%I', c.column_name), ', ' ORDER BY c.ordinal_position),
           string_agg(CASE WHEN c.column_name = 'recorded_to' THEN 'now()' ELSE format('OLD.%I', c.column_name) END, ', ' ORDER BY c.ordinal_position)
        INTO column_list, old_values
        FROM information_schema.columns c WHERE c.table_schema = enable.schema_name AND c.table_name = enable.table_name;
    SELECT string_agg(format('c.%1$I = NEW.%1$I', k.name), ' AND '), string_agg(format('c.%1$I = OLD.%1$I', k.name), ' AND ')
        INTO key_match, self_match
        FROM unnest(key_columns) AS k(name);
    body := format($body$
BEGIN
    IF TG_OP <> 'DELETE' AND NEW.valid_from >= NEW.valid_to THEN
        RAISE EXCEPTION 'valid_from must come before valid_to' USING ERRCODE = '22000';
    END IF;
    IF TG_OP = 'INSERT' THEN
        IF EXISTS (SELECT 1 FROM %1$I.%2$I c WHERE %3$s AND c.valid_from < NEW.valid_to AND c.valid_to > NEW.valid_from) THEN
            RAISE EXCEPTION 'the valid period overlaps another row with the same key' USING ERRCODE = '23P01';
        END IF;
    ELSIF TG_OP = 'UPDATE' THEN
        IF EXISTS (SELECT 1 FROM %1$I.%2$I c WHERE %3$s AND c.valid_from < NEW.valid_to AND c.valid_to > NEW.valid_from AND NOT (%4$s AND c.valid_from = OLD.valid_from)) THEN
            RAISE EXCEPTION 'the valid period overlaps another row with the same key' USING ERRCODE = '23P01';
        END IF;
        INSERT INTO %1$I.%5$I (%6$s) VALUES (%7$s);
    ELSE
        INSERT INTO %1$I.%5$I (%6$s) VALUES (%7$s);
        RETURN OLD;
    END IF;
    NEW.recorded_from := now();
    NEW.recorded_to := 'infinity';
    RETURN NEW;
END
$body$, schema_name, table_name, key_match, self_match, history_name, column_list, old_values);
    EXECUTE format('CREATE FUNCTION eggdb_temporal.%I() RETURNS trigger LANGUAGE plpgsql AS %L', function_name, body);
    EXECUTE format('CREATE TRIGGER eggdb_temporal_track BEFORE INSERT OR UPDATE OR DELETE ON %I.%I FOR EACH ROW EXECUTE FUNCTION eggdb_temporal.%I()', schema_name, table_name, function_name);
    EXECUTE format('CREATE VIEW %I.%I AS SELECT * FROM %I.%I UNION ALL SELECT * FROM %I.%I', schema_name, versions_name, schema_name, table_name, schema_name, history_name);
    INSERT INTO eggdb_temporal.tables (schema_name, table_name, key_columns, history_table, versions_view) VALUES (enable.schema_name, enable.table_name, enable.key_columns, history_name, versions_name);
END
$$;
CREATE FUNCTION eggdb_temporal.split_period(schema_name text, table_name text, key jsonb, period_start timestamptz, period_end timestamptz) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    key_condition text;
    column_list text;
    copy_template text;
    boundary timestamptz;
    found_row record;
BEGIN
    IF period_start >= period_end THEN
        RAISE EXCEPTION 'the period must start before it ends' USING ERRCODE = '22000';
    END IF;
    SELECT string_agg(format('%I = %L', k.key, k.value), ' AND ') INTO key_condition FROM jsonb_each_text(key) k;
    IF key_condition IS NULL THEN
        RAISE EXCEPTION 'the key names no columns' USING ERRCODE = '22023';
    END IF;
    SELECT string_agg(format('%I', c.column_name), ', ' ORDER BY c.ordinal_position),
           string_agg(CASE c.column_name WHEN 'valid_from' THEN '{starts}' WHEN 'valid_to' THEN '{ends}' ELSE format('%I', c.column_name) END, ', ' ORDER BY c.ordinal_position)
        INTO column_list, copy_template
        FROM information_schema.columns c WHERE c.table_schema = split_period.schema_name AND c.table_name = split_period.table_name;
    FOREACH boundary IN ARRAY ARRAY[period_end, period_start] LOOP
        FOR found_row IN EXECUTE format('SELECT valid_from AS starts, valid_to AS ends FROM %I.%I WHERE %s AND valid_from < %L AND valid_to > %L', schema_name, table_name, key_condition, boundary, boundary) LOOP
            EXECUTE format('UPDATE %I.%I SET valid_to = %L WHERE %s AND valid_from = %L', schema_name, table_name, boundary, key_condition, found_row.starts);
            EXECUTE format('INSERT INTO %I.%I (%s) SELECT %s FROM %I.%I WHERE %s AND valid_from = %L', schema_name, table_name, column_list,
                replace(replace(copy_template, '{starts}', format('%L::timestamptz', boundary)), '{ends}', format('%L::timestamptz', found_row.ends)),
                schema_name, table_name, key_condition, found_row.starts);
        END LOOP;
    END LOOP;
    RETURN key_condition;
END
$$;
CREATE FUNCTION eggdb_temporal.update_period(schema_name text, table_name text, key jsonb, period_start timestamptz, period_end timestamptz, changes jsonb) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    key_condition text;
    assignments text;
    changed bigint;
BEGIN
    SELECT string_agg(format('%I = %L', c.key, c.value), ', ') INTO assignments FROM jsonb_each_text(changes) c;
    IF assignments IS NULL THEN
        RAISE EXCEPTION 'the changes name no columns' USING ERRCODE = '22023';
    END IF;
    key_condition := eggdb_temporal.split_period(schema_name, table_name, key, period_start, period_end);
    EXECUTE format('UPDATE %I.%I SET %s WHERE %s AND valid_from >= %L AND valid_to <= %L', schema_name, table_name, assignments, key_condition, period_start, period_end);
    GET DIAGNOSTICS changed = ROW_COUNT;
    RETURN changed;
END
$$;
CREATE FUNCTION eggdb_temporal.delete_period(schema_name text, table_name text, key jsonb, period_start timestamptz, period_end timestamptz) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    key_condition text;
    removed bigint;
BEGIN
    key_condition := eggdb_temporal.split_period(schema_name, table_name, key, period_start, period_end);
    EXECUTE format('DELETE FROM %I.%I WHERE %s AND valid_from >= %L AND valid_to <= %L', schema_name, table_name, key_condition, period_start, period_end);
    GET DIAGNOSTICS removed = ROW_COUNT;
    RETURN removed;
END
$$;
CREATE FUNCTION eggdb_temporal.as_of(schema_name text, table_name text, valid_at timestamptz, recorded_at timestamptz DEFAULT now()) RETURNS TABLE (version jsonb) LANGUAGE plpgsql AS $$
DECLARE
    found_row record;
BEGIN
    FOR found_row IN EXECUTE format('SELECT to_jsonb(v) AS body FROM %I.%I v WHERE v.valid_from <= %L AND v.valid_to > %L AND v.recorded_from <= %L AND v.recorded_to > %L ORDER BY v.valid_from',
            schema_name, table_name || '_versions', valid_at, valid_at, recorded_at, recorded_at) LOOP
        version := found_row.body;
        RETURN NEXT;
    END LOOP;
END
$$;
CREATE FUNCTION eggdb_temporal.history(schema_name text, table_name text, key jsonb) RETURNS TABLE (version jsonb) LANGUAGE plpgsql AS $$
DECLARE
    key_condition text;
    found_row record;
BEGIN
    SELECT string_agg(format('v.%I = %L', k.key, k.value), ' AND ') INTO key_condition FROM jsonb_each_text(key) k;
    IF key_condition IS NULL THEN
        RAISE EXCEPTION 'the key names no columns' USING ERRCODE = '22023';
    END IF;
    FOR found_row IN EXECUTE format('SELECT to_jsonb(v) AS body FROM %I.%I v WHERE %s ORDER BY v.recorded_from, v.valid_from', schema_name, table_name || '_versions', key_condition) LOOP
        version := found_row.body;
        RETURN NEXT;
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
    fn periods_split_on_change_and_history_keeps_what_was_known() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_temporal",
            "CREATE TABLE prices (sku text NOT NULL, price numeric NOT NULL)",
            "INSERT INTO prices VALUES ('egg', 10)",
            "SELECT eggdb_temporal.enable('public', 'prices', ARRAY['sku'])",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT eggdb_temporal.update_period('public', 'prices', '{\"sku\": \"egg\"}', '2026-01-01', '2026-02-01', '{\"price\": 12}') AS changed").unwrap(),
            "changed\n1\nSELECT 1"
        );
        assert_eq!(run(&mut session, "SELECT price FROM prices ORDER BY valid_from").unwrap(), "price\n10\n12\n10\nSELECT 3");
        assert_eq!(
            run(&mut session, "SELECT version ->> 'price' AS price FROM eggdb_temporal.as_of('public', 'prices', '2026-01-15')").unwrap(),
            "price\n12\nSELECT 1"
        );
        assert_eq!(run(&mut session, "SELECT count(*) AS kept FROM prices_history").unwrap(), "kept\n3\nSELECT 1");
        assert_eq!(
            run(&mut session, "INSERT INTO prices (sku, price, valid_from, valid_to) VALUES ('egg', 9, '2026-01-10', '2026-01-20')").unwrap_err().state().code(),
            "23P01"
        );
        assert_eq!(
            run(&mut session, "SELECT eggdb_temporal.delete_period('public', 'prices', '{\"sku\": \"egg\"}', '2026-03-01', '2026-04-01') AS removed").unwrap(),
            "removed\n1\nSELECT 1"
        );
        assert_eq!(run(&mut session, "SELECT count(*) AS left_rows FROM prices").unwrap(), "left_rows\n4\nSELECT 1");
    }

    #[test]
    fn for_system_time_reads_the_rows_known_at_a_time() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_temporal",
            "CREATE TABLE stock (sku text NOT NULL, amount integer NOT NULL)",
            "SELECT eggdb_temporal.enable('public', 'stock', ARRAY['sku'])",
            "INSERT INTO stock (sku, amount) VALUES ('egg', 5)",
            "UPDATE stock SET recorded_from = '2026-01-01' WHERE sku = 'egg'",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        run(&mut session, "UPDATE stock SET amount = 7 WHERE sku = 'egg'").unwrap();
        assert_eq!(
            run(&mut session, "SELECT count(*) AS versions FROM stock FOR SYSTEM_TIME ALL").unwrap(),
            "versions\n3\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, "SELECT amount FROM stock FOR SYSTEM_TIME AS OF now() + interval '1 day' AS s").unwrap(),
            "amount\n7\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, "SELECT amount FROM stock FOR SYSTEM_TIME AS OF '1999-01-01'").unwrap(),
            "amount\nSELECT 0"
        );
        assert_eq!(
            run(&mut session, "SELECT * FROM prices_missing FOR SYSTEM_TIME ALL").unwrap_err().state().code(),
            "42P01"
        );
        run(&mut session, "CREATE TABLE plain (a integer)").unwrap();
        assert_eq!(
            run(&mut session, "SELECT * FROM plain FOR SYSTEM_TIME ALL").unwrap_err().state().code(),
            "42809"
        );
    }
}
