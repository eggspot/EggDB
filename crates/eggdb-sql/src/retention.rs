pub(crate) const RETENTION_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_retention;
CREATE TABLE eggdb_retention.rules (schema_name text NOT NULL, table_name text NOT NULL, time_column text NOT NULL, retain interval NOT NULL, PRIMARY KEY (schema_name, table_name));
CREATE TABLE eggdb_retention.holds (hold_name text PRIMARY KEY, schema_name text NOT NULL, table_name text NOT NULL, reason text NOT NULL, placed_at timestamptz NOT NULL DEFAULT now(), lifted_at timestamptz);
CREATE TABLE eggdb_retention.erasure_requests (request_id bigserial PRIMARY KEY, schema_name text NOT NULL, table_name text NOT NULL, row_condition text NOT NULL, requested_at timestamptz NOT NULL DEFAULT now(), waiting_for text, erased_at timestamptz, erased_rows bigint);
CREATE FUNCTION eggdb_retention.guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    hold text;
    kept_by text;
    kept_for interval;
    kept_until timestamptz;
BEGIN
    SELECT h.hold_name INTO hold FROM eggdb_retention.holds h WHERE h.schema_name = TG_TABLE_SCHEMA AND h.table_name = TG_TABLE_NAME AND h.lifted_at IS NULL ORDER BY h.hold_name LIMIT 1;
    IF hold IS NOT NULL THEN
        RAISE EXCEPTION 'table %.% is under legal hold %: its rows cannot be changed or removed until the hold is lifted', TG_TABLE_SCHEMA, TG_TABLE_NAME, hold USING ERRCODE = '55000';
    END IF;
    SELECT r.time_column, r.retain INTO kept_by, kept_for FROM eggdb_retention.rules r WHERE r.schema_name = TG_TABLE_SCHEMA AND r.table_name = TG_TABLE_NAME;
    IF kept_by IS NOT NULL THEN
        IF TG_OP = 'UPDATE' THEN
            RAISE EXCEPTION 'table %.% is write-once: rows can be removed after their retention, never changed', TG_TABLE_SCHEMA, TG_TABLE_NAME USING ERRCODE = '55000';
        END IF;
        kept_until := (to_jsonb(OLD) ->> kept_by)::timestamptz + kept_for;
        IF kept_until IS NULL OR kept_until > now() THEN
            RAISE EXCEPTION 'a row of %.% is retained until %', TG_TABLE_SCHEMA, TG_TABLE_NAME, coalesce(kept_until::text, 'its ' || kept_by || ' is set') USING ERRCODE = '55000';
        END IF;
    END IF;
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END
$$;
CREATE FUNCTION eggdb_retention.guard_truncate() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM eggdb_retention.holds h WHERE h.schema_name = TG_TABLE_SCHEMA AND h.table_name = TG_TABLE_NAME AND h.lifted_at IS NULL)
        OR EXISTS (SELECT 1 FROM eggdb_retention.rules r WHERE r.schema_name = TG_TABLE_SCHEMA AND r.table_name = TG_TABLE_NAME) THEN
        RAISE EXCEPTION 'table %.% has a retention rule or a legal hold: it cannot be truncated', TG_TABLE_SCHEMA, TG_TABLE_NAME USING ERRCODE = '55000';
    END IF;
    RETURN NULL;
END
$$;
CREATE FUNCTION eggdb_retention.protect_rules() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'retention rules are permanent: they cannot be changed or removed' USING ERRCODE = '55000';
END
$$;
CREATE FUNCTION eggdb_retention.protect_holds() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND OLD.lifted_at IS NULL AND NEW.lifted_at IS NOT NULL
        AND NEW.hold_name = OLD.hold_name AND NEW.schema_name = OLD.schema_name
        AND NEW.table_name = OLD.table_name AND NEW.reason = OLD.reason AND NEW.placed_at = OLD.placed_at THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'a legal hold can only be lifted, with eggdb_retention.lift_hold' USING ERRCODE = '55000';
END
$$;
CREATE TRIGGER eggdb_retention_rules_kept BEFORE UPDATE OR DELETE ON eggdb_retention.rules FOR EACH ROW EXECUTE FUNCTION eggdb_retention.protect_rules();
CREATE TRIGGER eggdb_retention_rules_not_truncated BEFORE TRUNCATE ON eggdb_retention.rules FOR EACH STATEMENT EXECUTE FUNCTION eggdb_retention.protect_rules();
CREATE TRIGGER eggdb_retention_holds_kept BEFORE UPDATE OR DELETE ON eggdb_retention.holds FOR EACH ROW EXECUTE FUNCTION eggdb_retention.protect_holds();
CREATE TRIGGER eggdb_retention_holds_not_truncated BEFORE TRUNCATE ON eggdb_retention.holds FOR EACH STATEMENT EXECUTE FUNCTION eggdb_retention.protect_rules();
CREATE FUNCTION eggdb_retention.guard_table(schema_name text, table_name text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    EXECUTE format('DROP TRIGGER IF EXISTS eggdb_retention_guard ON %I.%I', schema_name, table_name);
    EXECUTE format('CREATE TRIGGER eggdb_retention_guard BEFORE UPDATE OR DELETE ON %I.%I FOR EACH ROW EXECUTE FUNCTION eggdb_retention.guard()', schema_name, table_name);
    EXECUTE format('DROP TRIGGER IF EXISTS eggdb_retention_truncate ON %I.%I', schema_name, table_name);
    EXECUTE format('CREATE TRIGGER eggdb_retention_truncate BEFORE TRUNCATE ON %I.%I FOR EACH STATEMENT EXECUTE FUNCTION eggdb_retention.guard_truncate()', schema_name, table_name);
END
$$;
CREATE FUNCTION eggdb_retention.retain(schema_name text, table_name text, time_column text, retain interval) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    IF retain <= interval '0' THEN
        RAISE EXCEPTION 'a retention period must be positive' USING ERRCODE = '22023';
    END IF;
    INSERT INTO eggdb_retention.rules VALUES (schema_name, table_name, time_column, retain);
    PERFORM eggdb_retention.guard_table(schema_name, table_name);
END
$$;
CREATE FUNCTION eggdb_retention.place_hold(hold_name text, schema_name text, table_name text, reason text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO eggdb_retention.holds (hold_name, schema_name, table_name, reason) VALUES (hold_name, schema_name, table_name, reason);
    PERFORM eggdb_retention.guard_table(schema_name, table_name);
END
$$;
CREATE FUNCTION eggdb_retention.lift_hold(lifted text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    UPDATE eggdb_retention.holds SET lifted_at = now() WHERE hold_name = lifted AND lifted_at IS NULL;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'there is no active legal hold %', lifted USING ERRCODE = '22023';
    END IF;
END
$$;
CREATE FUNCTION eggdb_retention.process_erasures() RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    pending record;
    hold text;
    erased bigint;
    done bigint := 0;
BEGIN
    FOR pending IN SELECT * FROM eggdb_retention.erasure_requests WHERE erased_at IS NULL ORDER BY request_id LOOP
        SELECT h.hold_name INTO hold FROM eggdb_retention.holds h WHERE h.schema_name = pending.schema_name AND h.table_name = pending.table_name AND h.lifted_at IS NULL ORDER BY h.hold_name LIMIT 1;
        IF hold IS NOT NULL THEN
            UPDATE eggdb_retention.erasure_requests SET waiting_for = 'legal hold ' || hold WHERE request_id = pending.request_id;
            CONTINUE;
        END IF;
        BEGIN
            EXECUTE format('DELETE FROM %I.%I WHERE %s', pending.schema_name, pending.table_name, pending.row_condition);
            GET DIAGNOSTICS erased = ROW_COUNT;
            UPDATE eggdb_retention.erasure_requests SET waiting_for = NULL, erased_at = now(), erased_rows = erased WHERE request_id = pending.request_id;
            done := done + 1;
        EXCEPTION WHEN SQLSTATE '55000' THEN
            UPDATE eggdb_retention.erasure_requests SET waiting_for = 'retention' WHERE request_id = pending.request_id;
        END;
    END LOOP;
    RETURN done;
END
$$;
CREATE FUNCTION eggdb_retention.request_erasure(schema_name text, table_name text, row_condition text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    request bigint;
BEGIN
    INSERT INTO eggdb_retention.erasure_requests (schema_name, table_name, row_condition) VALUES (schema_name, table_name, row_condition) RETURNING request_id INTO request;
    PERFORM eggdb_retention.process_erasures();
    RETURN request;
END
$$;
"#;

#[cfg(test)]
mod tests {
    use crate::error::SqlState;
    use crate::memory_store::MemoryStore;
    use crate::tests::render;
    use crate::{Session, SqlError};

    fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(render(&result)))?;
        Ok(outputs.pop().unwrap_or_default())
    }

    fn records() -> MemoryStore {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_retention",
            "CREATE TABLE trades (id integer PRIMARY KEY, client text, booked_at timestamptz NOT NULL)",
            "INSERT INTO trades VALUES (1, 'ada', now() - interval '8 years'), (2, 'ada', now() - interval '1 day'), (3, 'bob', now() - interval '9 years')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        drop(session);
        store
    }

    #[test]
    fn write_once_rows_are_never_changed_and_removed_only_after_retention() {
        let store = records();
        let mut session = Session::new(&store);
        run(
            &mut session,
            "SELECT eggdb_retention.retain('public', 'trades', 'booked_at', interval '7 years')",
        )
        .unwrap();
        assert_eq!(
            run(
                &mut session,
                "UPDATE trades SET client = 'eve' WHERE id = 1"
            )
            .unwrap_err()
            .state(),
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE
        );
        assert_eq!(
            run(&mut session, "DELETE FROM trades WHERE id = 2")
                .unwrap_err()
                .state(),
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE
        );
        assert_eq!(
            run(&mut session, "DELETE FROM trades WHERE id = 1").unwrap(),
            "DELETE 1"
        );
        assert!(
            run(
                &mut session,
                "SELECT eggdb_retention.retain('public', 'trades', 'booked_at', interval '-1 day')"
            )
            .is_err()
        );
    }

    #[test]
    fn erasure_waits_for_a_legal_hold_and_is_recorded() {
        let store = records();
        let mut session = Session::new(&store);
        run(&mut session, "SELECT eggdb_retention.place_hold('case-42', 'public', 'trades', 'litigation with ada')").unwrap();
        assert_eq!(
            run(&mut session, "DELETE FROM trades WHERE id = 3")
                .unwrap_err()
                .state(),
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE
        );
        run(
            &mut session,
            "SELECT eggdb_retention.request_erasure('public', 'trades', 'client = ''ada''')",
        )
        .unwrap();
        assert_eq!(
            run(
                &mut session,
                "SELECT waiting_for, erased_rows FROM eggdb_retention.erasure_requests"
            )
            .unwrap(),
            "waiting_for|erased_rows\nlegal hold case-42|NULL\nSELECT 1"
        );
        run(&mut session, "SELECT eggdb_retention.lift_hold('case-42')").unwrap();
        assert_eq!(
            run(
                &mut session,
                "SELECT eggdb_retention.process_erasures() AS done"
            )
            .unwrap(),
            "done\n1\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, "SELECT waiting_for IS NULL AS cleared, erased_rows FROM eggdb_retention.erasure_requests").unwrap(),
            "cleared|erased_rows\nt|2\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, "SELECT count(*) AS left FROM trades").unwrap(),
            "left\n1\nSELECT 1"
        );
        assert!(run(&mut session, "SELECT eggdb_retention.lift_hold('case-42')").is_err());
    }

    #[test]
    fn truncate_and_edits_of_the_rules_and_holds_cannot_get_around_retention() {
        let store = records();
        let mut session = Session::new(&store);
        run(
            &mut session,
            "SELECT eggdb_retention.retain('public', 'trades', 'booked_at', interval '7 years')",
        )
        .unwrap();
        run(
            &mut session,
            "SELECT eggdb_retention.place_hold('case-7', 'public', 'trades', 'audit')",
        )
        .unwrap();
        for sql in [
            "TRUNCATE trades",
            "DELETE FROM eggdb_retention.rules",
            "UPDATE eggdb_retention.rules SET retain = interval '1 day'",
            "TRUNCATE eggdb_retention.rules",
            "DELETE FROM eggdb_retention.holds",
            "UPDATE eggdb_retention.holds SET table_name = 'other'",
            "TRUNCATE eggdb_retention.holds",
        ] {
            assert_eq!(
                run(&mut session, sql).unwrap_err().state(),
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "{sql}"
            );
        }
        run(&mut session, "SELECT eggdb_retention.lift_hold('case-7')").unwrap();
        assert_eq!(
            run(&mut session, "SELECT count(*) FROM trades").unwrap(),
            "count\n3\nSELECT 1"
        );
    }

    #[test]
    fn cascades_and_replace_cannot_remove_retained_rows() {
        let store = records();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE TABLE clients (name text PRIMARY KEY)",
            "INSERT INTO clients VALUES ('ada'), ('bob')",
            "CREATE TABLE fills (id integer PRIMARY KEY, client text REFERENCES clients ON DELETE CASCADE, booked_at timestamptz NOT NULL)",
            "INSERT INTO fills VALUES (1, 'ada', now())",
            "SELECT eggdb_retention.retain('public', 'fills', 'booked_at', interval '7 years')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        let cascaded = run(&mut session, "DELETE FROM clients WHERE name = 'ada'").unwrap_err();
        assert_eq!(
            cascaded.state(),
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "{cascaded}"
        );
        assert_eq!(
            run(&mut session, "DELETE FROM clients WHERE name = 'bob'").unwrap(),
            "DELETE 1"
        );
        let mut sqlite = Session::with_dialect(&store, crate::Dialect::Sqlite);
        let replaced = run(
            &mut sqlite,
            "INSERT OR REPLACE INTO fills VALUES (1, 'ada', '2020-01-01 00:00:00+00')",
        )
        .unwrap_err();
        assert_eq!(
            replaced.state(),
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "{replaced}"
        );
        assert_eq!(
            run(&mut session, "SELECT count(*) FROM fills").unwrap(),
            "count\n1\nSELECT 1"
        );
    }
}
