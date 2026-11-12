pub(crate) const REGISTRY_SCHEMA: &str = "eggdb_temporal";
pub(crate) const REGISTRY_TABLE: &str = "tables";
pub(crate) const SYSTEM_START: &str = "recorded_from";
pub(crate) const SYSTEM_END: &str = "recorded_to";

/// The statements `CREATE EXTENSION eggdb_temporal` runs: bitemporal tables, with a valid period
/// the application states and a recorded period the database keeps, so a row can be read as it
/// was true at any time and as it was known at any time (D50).
pub(crate) const TEMPORAL_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_temporal;
CREATE TABLE eggdb_temporal.tables (schema_name text NOT NULL, table_name text NOT NULL, key_columns text[] NOT NULL, history_table text NOT NULL, versions_view text NOT NULL, enabled_at timestamptz NOT NULL DEFAULT now(), system_start text NOT NULL DEFAULT 'recorded_from', system_end text NOT NULL DEFAULT 'recorded_to', PRIMARY KEY (schema_name, table_name));
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
CREATE FUNCTION eggdb_temporal.enable_system_versioning(schema_name text, table_name text, start_column text, end_column text, history_table text DEFAULT NULL, open_end text DEFAULT 'infinity') RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    history_name text := coalesce(enable_system_versioning.history_table, enable_system_versioning.table_name || '_history');
    versions_name text := enable_system_versioning.table_name || '_versions';
    function_name text := 'version_' || enable_system_versioning.schema_name || '_' || enable_system_versioning.table_name;
    column_list text;
    old_values text;
    body text;
BEGIN
    IF EXISTS (SELECT 1 FROM eggdb_temporal.tables t WHERE t.schema_name = enable_system_versioning.schema_name AND t.table_name = enable_system_versioning.table_name) THEN
        RAISE EXCEPTION 'table %.% is already versioned', enable_system_versioning.schema_name, enable_system_versioning.table_name USING ERRCODE = '42710';
    END IF;
    EXECUTE format('ALTER TABLE %I.%I ADD COLUMN IF NOT EXISTS %I timestamptz NOT NULL DEFAULT now()', enable_system_versioning.schema_name, enable_system_versioning.table_name, start_column);
    EXECUTE format('ALTER TABLE %I.%I ADD COLUMN IF NOT EXISTS %I timestamptz NOT NULL DEFAULT %L', enable_system_versioning.schema_name, enable_system_versioning.table_name, end_column, open_end);
    IF NOT EXISTS (SELECT 1 FROM information_schema.tables t WHERE t.table_schema = enable_system_versioning.schema_name AND t.table_name = history_name) THEN
        EXECUTE format('CREATE TABLE %I.%I AS SELECT * FROM %I.%I WHERE false', enable_system_versioning.schema_name, history_name, enable_system_versioning.schema_name, enable_system_versioning.table_name);
    END IF;
    SELECT string_agg(format('%I', c.column_name), ', ' ORDER BY c.ordinal_position),
           string_agg(CASE WHEN c.column_name = end_column THEN 'now()' ELSE format('OLD.%I', c.column_name) END, ', ' ORDER BY c.ordinal_position)
        INTO column_list, old_values
        FROM information_schema.columns c WHERE c.table_schema = enable_system_versioning.schema_name AND c.table_name = enable_system_versioning.table_name;
    body := format($body$
BEGIN
    IF TG_OP <> 'INSERT' THEN
        INSERT INTO %1$I.%2$I (%3$s) VALUES (%4$s);
    END IF;
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    NEW.%5$I := now();
    NEW.%6$I := %7$L;
    RETURN NEW;
END
$body$, enable_system_versioning.schema_name, history_name, column_list, old_values, start_column, end_column, open_end);
    EXECUTE format('CREATE FUNCTION eggdb_temporal.%I() RETURNS trigger LANGUAGE plpgsql AS %L', function_name, body);
    EXECUTE format('CREATE TRIGGER eggdb_temporal_version BEFORE INSERT OR UPDATE OR DELETE ON %I.%I FOR EACH ROW EXECUTE FUNCTION eggdb_temporal.%I()', enable_system_versioning.schema_name, enable_system_versioning.table_name, function_name);
    EXECUTE format('CREATE VIEW %I.%I AS SELECT * FROM %I.%I UNION ALL SELECT * FROM %I.%I', enable_system_versioning.schema_name, versions_name, enable_system_versioning.schema_name, enable_system_versioning.table_name, enable_system_versioning.schema_name, history_name);
    INSERT INTO eggdb_temporal.tables (schema_name, table_name, key_columns, history_table, versions_view, system_start, system_end) VALUES (enable_system_versioning.schema_name, enable_system_versioning.table_name, ARRAY[]::text[], history_name, versions_name, start_column, end_column);
END
$$;
CREATE FUNCTION eggdb_temporal.disable_system_versioning(schema_name text, table_name text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    versions_name text;
BEGIN
    SELECT t.versions_view INTO versions_name FROM eggdb_temporal.tables t WHERE t.schema_name = disable_system_versioning.schema_name AND t.table_name = disable_system_versioning.table_name AND t.system_start <> 'recorded_from';
    IF NOT FOUND THEN
        RAISE EXCEPTION 'table %.% is not system-versioned', disable_system_versioning.schema_name, disable_system_versioning.table_name USING ERRCODE = '42809';
    END IF;
    EXECUTE format('DROP VIEW %I.%I', disable_system_versioning.schema_name, versions_name);
    EXECUTE format('DROP TRIGGER eggdb_temporal_version ON %I.%I', disable_system_versioning.schema_name, disable_system_versioning.table_name);
    EXECUTE format('DROP FUNCTION eggdb_temporal.%I()', 'version_' || disable_system_versioning.schema_name || '_' || disable_system_versioning.table_name);
    DELETE FROM eggdb_temporal.tables t WHERE t.schema_name = disable_system_versioning.schema_name AND t.table_name = disable_system_versioning.table_name;
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

    #[test]
    fn mariadb_and_sql_server_syntax_make_tables_system_versioned() {
        let store = MemoryStore::default();
        let mut mariadb = Session::with_dialect(&store, crate::Dialect::MySql);
        for sql in [
            "CREATE TABLE stock (sku VARCHAR(10) PRIMARY KEY, amount INT) WITH SYSTEM VERSIONING",
            "INSERT INTO stock (sku, amount) VALUES ('egg', 5)",
            "UPDATE stock SET amount = 6 WHERE sku = 'egg'",
        ] {
            run(&mut mariadb, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut mariadb, "SELECT COUNT(*) AS versions FROM stock FOR SYSTEM_TIME ALL").unwrap(),
            "versions\n2\nSELECT 1"
        );
        run(&mut mariadb, "ALTER TABLE stock DROP SYSTEM VERSIONING").unwrap();
        assert!(run(&mut mariadb, "SELECT * FROM stock FOR SYSTEM_TIME ALL").is_err());
        let mut sqlserver = Session::with_dialect(&store, crate::Dialect::SqlServer);
        run(
            &mut sqlserver,
            "CREATE TABLE prices (id INT PRIMARY KEY, price INT, \
             ValidFrom DATETIME2 GENERATED ALWAYS AS ROW START HIDDEN NOT NULL, \
             ValidTo DATETIME2 GENERATED ALWAYS AS ROW END HIDDEN NOT NULL, \
             PERIOD FOR SYSTEM_TIME (ValidFrom, ValidTo)) \
             WITH (SYSTEM_VERSIONING = ON (HISTORY_TABLE = dbo.PricesHistory))",
        )
        .unwrap_or_else(|error| panic!("create: {error}"));
        run(&mut sqlserver, "INSERT INTO prices (id, price) VALUES (1, 10)").unwrap();
        run(&mut sqlserver, "UPDATE prices SET price = 11 WHERE id = 1").unwrap();
        assert!(run(&mut sqlserver, "SELECT COUNT(*) AS kept FROM PricesHistory").unwrap().contains('1'));
        let refused = run(
            &mut sqlserver,
            "CREATE TABLE plain (id INT PRIMARY KEY) WITH (SYSTEM_VERSIONING = ON)",
        )
        .unwrap_err();
        assert_eq!(refused.vendor_code(), Some(13510));
    }

    #[test]
    fn sql_server_alter_table_turns_versioning_on_over_a_declared_period() {
        let store = MemoryStore::default();
        let mut sqlserver = Session::with_dialect(&store, crate::Dialect::SqlServer);
        run(&mut sqlserver, "CREATE TABLE orders (id INT PRIMARY KEY, status VARCHAR(10))").unwrap();
        let refused = run(&mut sqlserver, "ALTER TABLE orders SET (SYSTEM_VERSIONING = ON)").unwrap_err();
        assert_eq!(refused.vendor_code(), Some(13510));
        for sql in [
            "ALTER TABLE orders ADD \
             SysStart DATETIME2 GENERATED ALWAYS AS ROW START HIDDEN NOT NULL DEFAULT SYSUTCDATETIME(), \
             SysEnd DATETIME2 GENERATED ALWAYS AS ROW END HIDDEN NOT NULL DEFAULT '9999-12-31 23:59:59.9999999', \
             PERIOD FOR SYSTEM_TIME (SysStart, SysEnd)",
            "ALTER TABLE orders SET (SYSTEM_VERSIONING = ON (HISTORY_TABLE = dbo.OrdersHistory, HISTORY_RETENTION_PERIOD = 6 MONTHS))",
            "INSERT INTO orders (id, status) VALUES (1, 'new')",
            "UPDATE orders SET status = 'paid' WHERE id = 1",
        ] {
            run(&mut sqlserver, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert!(run(&mut sqlserver, "SELECT COUNT(*) AS kept FROM OrdersHistory").unwrap().contains('1'));
        let mut postgresql = Session::new(&store);
        assert_eq!(
            run(&mut postgresql, "SELECT keep FROM eggdb_temporal.history_retention").unwrap(),
            "keep\n6 mons\nSELECT 1"
        );
        run(&mut sqlserver, "ALTER TABLE orders SET (SYSTEM_VERSIONING = ON (HISTORY_RETENTION_PERIOD = INFINITE))").unwrap();
        assert_eq!(
            run(&mut postgresql, "SELECT count(*) AS rules FROM eggdb_temporal.history_retention").unwrap(),
            "rules\n0\nSELECT 1"
        );
        run(&mut sqlserver, "ALTER TABLE orders SET (SYSTEM_VERSIONING = OFF)").unwrap();
        run(&mut sqlserver, "ALTER TABLE orders SET (SYSTEM_VERSIONING = ON)").unwrap();
        run(&mut sqlserver, "UPDATE orders SET status = 'sent' WHERE id = 1").unwrap();
        assert!(run(&mut sqlserver, "SELECT COUNT(*) AS kept FROM orders_history").unwrap().contains('1'));
    }
}

