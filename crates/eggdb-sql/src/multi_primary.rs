//! Multi-primary SQL rows (D1098): `CREATE EXTENSION eggdb_multi_primary` lets every server of a
//! group take writes to the same tables. `eggdb_mp.enable(schema, table, rule, resolver,
//! counters)` puts a capture trigger on a table that stamps each local change with a hybrid
//! logical clock and appends it to `eggdb_mp.log`; `eggdb serve --multi-primary` pulls the other
//! servers' logs and hands each change to `eggdb_mp.apply`, which settles a conflict by the
//! table's rule: `lww` (the newer stamp wins), `function` (a SQL function given the local and
//! the remote row decides), or `crdt` (the named counter columns add up the remote change, the
//! other columns by the newer stamp).

/// The statements `CREATE EXTENSION eggdb_multi_primary` runs.
pub(crate) const MULTI_PRIMARY_SCRIPT: &str = r"
CREATE SCHEMA eggdb_mp;
CREATE TABLE eggdb_mp.settings (
    name text PRIMARY KEY,
    value text NOT NULL
);
INSERT INTO eggdb_mp.settings (name, value) VALUES ('node', 'node'), ('clock', '000000000000000.000000');
CREATE TABLE eggdb_mp.rules (
    table_schema text NOT NULL,
    table_name text NOT NULL,
    rule text NOT NULL,
    resolver text,
    counters text,
    key_columns text NOT NULL,
    PRIMARY KEY (table_schema, table_name)
);
CREATE TABLE eggdb_mp.log (
    id bigserial PRIMARY KEY,
    origin text NOT NULL,
    stamp text NOT NULL,
    table_schema text NOT NULL,
    table_name text NOT NULL,
    op text NOT NULL,
    before jsonb,
    after jsonb
);
CREATE TABLE eggdb_mp.stamps (
    table_schema text NOT NULL,
    table_name text NOT NULL,
    row_key text NOT NULL,
    stamp text NOT NULL,
    PRIMARY KEY (table_schema, table_name, row_key)
);
CREATE TABLE eggdb_mp.cursors (
    peer text PRIMARY KEY,
    last_id bigint NOT NULL
);
CREATE FUNCTION eggdb_mp.tick(seen text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    clock_text text;
    node_name text;
    now_ms bigint;
    held_ms bigint;
    held_counter integer;
    next_ms bigint;
    next_counter integer;
BEGIN
    SELECT value INTO clock_text FROM eggdb_mp.settings WHERE name = 'clock';
    SELECT value INTO node_name FROM eggdb_mp.settings WHERE name = 'node';
    now_ms := floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint;
    held_ms := split_part(clock_text, '.', 1)::bigint;
    held_counter := split_part(clock_text, '.', 2)::integer;
    IF seen IS NOT NULL AND (split_part(seen, '.', 1)::bigint > held_ms OR (split_part(seen, '.', 1)::bigint = held_ms AND split_part(seen, '.', 2)::integer > held_counter)) THEN
        held_ms := split_part(seen, '.', 1)::bigint;
        held_counter := split_part(seen, '.', 2)::integer;
    END IF;
    IF now_ms > held_ms THEN
        next_ms := now_ms;
        next_counter := 0;
    ELSE
        next_ms := held_ms;
        next_counter := held_counter + 1;
    END IF;
    UPDATE eggdb_mp.settings SET value = lpad(next_ms::text, 15, '0') || '.' || lpad(next_counter::text, 6, '0') WHERE name = 'clock';
    RETURN lpad(next_ms::text, 15, '0') || '.' || lpad(next_counter::text, 6, '0') || '.' || node_name;
END
$$;
CREATE FUNCTION eggdb_mp.row_key(columns text, row_data jsonb) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    key_object jsonb := '{}'::jsonb;
    column_name text;
BEGIN
    FOR column_name IN SELECT unnest(string_to_array(columns, ',')) LOOP
        key_object := key_object || jsonb_build_object(column_name, row_data -> column_name);
    END LOOP;
    RETURN key_object::text;
END
$$;
CREATE FUNCTION eggdb_mp.capture() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    change_stamp text;
    columns text;
    node_name text;
    changed jsonb;
    changed_key text;
BEGIN
    IF coalesce(current_setting('eggdb_mp.applying', true), '') = 'on' THEN
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NEW;
    END IF;
    change_stamp := eggdb_mp.tick(NULL);
    SELECT key_columns INTO columns FROM eggdb_mp.rules WHERE table_schema = TG_TABLE_SCHEMA AND table_name = TG_TABLE_NAME;
    SELECT value INTO node_name FROM eggdb_mp.settings WHERE name = 'node';
    IF TG_OP = 'DELETE' THEN
        changed := to_jsonb(OLD);
        INSERT INTO eggdb_mp.log (origin, stamp, table_schema, table_name, op, before) VALUES (node_name, change_stamp, TG_TABLE_SCHEMA, TG_TABLE_NAME, 'd', changed);
    ELSIF TG_OP = 'UPDATE' THEN
        changed := to_jsonb(NEW);
        INSERT INTO eggdb_mp.log (origin, stamp, table_schema, table_name, op, before, after) VALUES (node_name, change_stamp, TG_TABLE_SCHEMA, TG_TABLE_NAME, 'u', to_jsonb(OLD), changed);
    ELSE
        changed := to_jsonb(NEW);
        INSERT INTO eggdb_mp.log (origin, stamp, table_schema, table_name, op, after) VALUES (node_name, change_stamp, TG_TABLE_SCHEMA, TG_TABLE_NAME, 'c', changed);
    END IF;
    changed_key := eggdb_mp.row_key(columns, changed);
    DELETE FROM eggdb_mp.stamps WHERE table_schema = TG_TABLE_SCHEMA AND table_name = TG_TABLE_NAME AND row_key = changed_key;
    INSERT INTO eggdb_mp.stamps (table_schema, table_name, row_key, stamp) VALUES (TG_TABLE_SCHEMA, TG_TABLE_NAME, changed_key, change_stamp);
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END
$$;
CREATE FUNCTION eggdb_mp.enable(target_schema text, target_table text, conflict_rule text, resolver_function text, counter_columns text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    columns text;
BEGIN
    IF conflict_rule NOT IN ('lww', 'function', 'crdt') THEN
        RAISE EXCEPTION 'the conflict rule must be lww, function or crdt, not %', conflict_rule;
    END IF;
    IF conflict_rule = 'function' AND resolver_function IS NULL THEN
        RAISE EXCEPTION 'the function rule needs the name of a function taking (local jsonb, remote jsonb, local_stamp text, remote_stamp text) and returning the row as jsonb, or NULL to delete it';
    END IF;
    IF conflict_rule = 'crdt' AND counter_columns IS NULL THEN
        RAISE EXCEPTION 'the crdt rule needs the counter columns, separated by commas';
    END IF;
    SELECT string_agg(k.column_name, ',' ORDER BY k.ordinal_position) INTO columns
    FROM information_schema.table_constraints c
    JOIN information_schema.key_column_usage k
      ON k.constraint_name = c.constraint_name AND k.table_schema = c.table_schema AND k.table_name = c.table_name
    WHERE c.constraint_type = 'PRIMARY KEY' AND c.table_schema = target_schema AND c.table_name = target_table;
    IF columns IS NULL THEN
        RAISE EXCEPTION 'table %.% needs a primary key to take writes on several servers', target_schema, target_table;
    END IF;
    DELETE FROM eggdb_mp.rules WHERE table_schema = target_schema AND table_name = target_table;
    INSERT INTO eggdb_mp.rules (table_schema, table_name, rule, resolver, counters, key_columns) VALUES (target_schema, target_table, conflict_rule, resolver_function, counter_columns, columns);
    EXECUTE format('DROP TRIGGER IF EXISTS eggdb_mp_capture ON %I.%I', target_schema, target_table);
    EXECUTE format('CREATE TRIGGER eggdb_mp_capture AFTER INSERT OR UPDATE OR DELETE ON %I.%I FOR EACH ROW EXECUTE FUNCTION eggdb_mp.capture()', target_schema, target_table);
    RETURN columns;
END
$$;
CREATE FUNCTION eggdb_mp.apply(change_origin text, change_stamp text, target_schema text, target_table text, change_op text, before_row jsonb, after_row jsonb) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    rule_name text;
    resolver_name text;
    counter_columns text;
    columns text;
    changed_key text;
    held_stamp text;
    local_row jsonb;
    merged jsonb;
    predicate text := '';
    column_name text;
    keyed jsonb;
    outcome text := 'applied';
BEGIN
    PERFORM set_config('eggdb_mp.applying', 'on', true);
    PERFORM eggdb_mp.tick(change_stamp);
    SELECT rule, resolver, counters, key_columns INTO rule_name, resolver_name, counter_columns, columns
    FROM eggdb_mp.rules WHERE table_schema = target_schema AND table_name = target_table;
    IF columns IS NULL THEN
        PERFORM set_config('eggdb_mp.applying', 'off', true);
        RETURN 'skipped: the table takes no multi-primary writes here';
    END IF;
    keyed := coalesce(after_row, before_row);
    changed_key := eggdb_mp.row_key(columns, keyed);
    FOR column_name IN SELECT unnest(string_to_array(columns, ',')) LOOP
        IF predicate <> '' THEN
            predicate := predicate || ' AND ';
        END IF;
        predicate := predicate || format('%I::text = %L', column_name, keyed ->> column_name);
    END LOOP;
    EXECUTE format('SELECT to_jsonb(t) FROM %I.%I AS t WHERE %s', target_schema, target_table, predicate) INTO local_row;
    SELECT stamp INTO held_stamp FROM eggdb_mp.stamps WHERE table_schema = target_schema AND table_name = target_table AND row_key = changed_key;
    IF rule_name = 'function' AND held_stamp IS NOT NULL AND local_row IS NOT NULL THEN
        EXECUTE format('SELECT %s($1, $2, $3, $4)', resolver_name) INTO merged USING local_row, after_row, held_stamp, change_stamp;
        outcome := 'resolved';
    ELSIF rule_name = 'crdt' AND change_op <> 'd' THEN
        IF held_stamp IS NOT NULL AND held_stamp >= change_stamp THEN
            merged := coalesce(local_row, after_row);
        ELSE
            merged := after_row;
        END IF;
        FOR column_name IN SELECT unnest(string_to_array(counter_columns, ',')) LOOP
            merged := jsonb_set(merged, ARRAY[column_name], to_jsonb(
                coalesce((local_row ->> column_name)::numeric, 0)
                + coalesce((after_row ->> column_name)::numeric, 0)
                - coalesce((before_row ->> column_name)::numeric, 0)));
        END LOOP;
        outcome := 'merged';
    ELSE
        IF held_stamp IS NOT NULL AND held_stamp >= change_stamp THEN
            PERFORM set_config('eggdb_mp.applying', 'off', true);
            RETURN 'kept the newer row';
        END IF;
        IF change_op = 'd' THEN
            merged := NULL;
        ELSE
            merged := after_row;
        END IF;
    END IF;
    EXECUTE format('DELETE FROM %I.%I WHERE %s', target_schema, target_table, predicate);
    IF merged IS NOT NULL THEN
        EXECUTE format('INSERT INTO %I.%I SELECT * FROM jsonb_populate_record(NULL::%I.%I, $1)', target_schema, target_table, target_schema, target_table) USING merged;
    END IF;
    DELETE FROM eggdb_mp.stamps WHERE table_schema = target_schema AND table_name = target_table AND row_key = changed_key;
    INSERT INTO eggdb_mp.stamps (table_schema, table_name, row_key, stamp) VALUES (target_schema, target_table, changed_key, greatest(coalesce(held_stamp, ''), change_stamp));
    PERFORM set_config('eggdb_mp.applying', 'off', true);
    RETURN outcome;
END
$$;
";

/// Quotes `text` as a SQL string literal, or `NULL`.
pub fn literal(text: Option<&str>) -> String {
    match text {
        Some(text) => format!("'{}'", text.replace('\'', "''")),
        None => "NULL".to_owned(),
    }
}

/// One change from another server's `eggdb_mp.log`, as its columns' text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedChange {
    /// The change's id in its server's log.
    pub id: i64,
    /// The server that made it.
    pub origin: String,
    /// Its hybrid-clock stamp.
    pub stamp: String,
    /// The table's schema.
    pub table_schema: String,
    /// The table.
    pub table_name: String,
    /// `c`, `u` or `d`.
    pub op: String,
    /// The row before, as JSON, for updates and deletions.
    pub before: Option<String>,
    /// The row after, as JSON, for inserts and updates.
    pub after: Option<String>,
}

/// The query that reads a server's own changes after `after_id`, at most `limit` of them.
pub fn log_query(after_id: i64, limit: u32) -> String {
    format!(
        "SELECT id, origin, stamp, table_schema, table_name, op, before::text, after::text FROM eggdb_mp.log WHERE id > {after_id} ORDER BY id LIMIT {limit}"
    )
}

/// The statements that apply `changes` from `peer` in one transaction and move its cursor past
/// them.
pub fn apply_script(peer: &str, changes: &[LoggedChange]) -> Option<String> {
    let last = changes.last()?.id;
    let mut script = String::from("BEGIN;\n");
    for change in changes {
        script.push_str(&format!(
            "SELECT eggdb_mp.apply({}, {}, {}, {}, {}, {}::jsonb, {}::jsonb);\n",
            literal(Some(&change.origin)),
            literal(Some(&change.stamp)),
            literal(Some(&change.table_schema)),
            literal(Some(&change.table_name)),
            literal(Some(&change.op)),
            literal(change.before.as_deref()),
            literal(change.after.as_deref()),
        ));
    }
    script.push_str(&format!(
        "DELETE FROM eggdb_mp.cursors WHERE peer = {peer};\nINSERT INTO eggdb_mp.cursors (peer, last_id) VALUES ({peer}, {last});\nCOMMIT;\n",
        peer = literal(Some(peer)),
    ));
    Some(script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_double_their_quotes() {
        assert_eq!(literal(Some("it's")), "'it''s'");
        assert_eq!(literal(None), "NULL");
    }

    #[test]
    fn an_apply_script_moves_the_cursor_to_the_last_change() {
        let change = LoggedChange {
            id: 41,
            origin: "b".to_owned(),
            stamp: "000000000000001.000000.b".to_owned(),
            table_schema: "public".to_owned(),
            table_name: "orders".to_owned(),
            op: "c".to_owned(),
            before: None,
            after: Some(r#"{"id": 1, "note": "it's"}"#.to_owned()),
        };
        let script = apply_script("b", &[change]).unwrap();
        assert!(script.contains("NULL::jsonb, '{\"id\": 1, \"note\": \"it''s\"}'::jsonb"));
        assert!(script.contains("VALUES ('b', 41)"));
        assert_eq!(apply_script("b", &[]), None);
    }
}
