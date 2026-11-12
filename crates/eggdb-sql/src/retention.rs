pub(crate) const RETENTION_SCRIPT: &str = r##"
CREATE SCHEMA eggdb_retention;
CREATE TABLE eggdb_retention.rules (schema_name text NOT NULL, table_name text NOT NULL, time_column text NOT NULL, retain interval NOT NULL, PRIMARY KEY (schema_name, table_name));
CREATE TABLE eggdb_retention.holds (hold_name text PRIMARY KEY, schema_name text NOT NULL, table_name text NOT NULL, reason text NOT NULL, placed_at timestamptz NOT NULL DEFAULT now(), lifted_at timestamptz, row_condition text);
CREATE TABLE eggdb_retention.erasure_requests (request_id bigserial PRIMARY KEY, schema_name text NOT NULL, table_name text NOT NULL, row_condition text NOT NULL, requested_at timestamptz NOT NULL DEFAULT now(), waiting_for text, erased_at timestamptz, erased_rows bigint);
CREATE FUNCTION eggdb_retention.guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    held record;
    covered boolean;
    kept_by text;
    kept_for interval;
    kept_until timestamptz;
BEGIN
    FOR held IN SELECT h.hold_name, h.row_condition FROM eggdb_retention.holds h WHERE h.schema_name = TG_TABLE_SCHEMA AND h.table_name = TG_TABLE_NAME AND h.lifted_at IS NULL ORDER BY h.hold_name LOOP
        IF held.row_condition IS NULL THEN
            RAISE EXCEPTION 'table %.% is under legal hold %: its rows cannot be changed or removed until the hold is lifted', TG_TABLE_SCHEMA, TG_TABLE_NAME, held.hold_name USING ERRCODE = '55000';
        END IF;
        EXECUTE format('SELECT coalesce((%s), false) FROM jsonb_populate_record(NULL::%I.%I, %L::jsonb) AS t', held.row_condition, TG_TABLE_SCHEMA, TG_TABLE_NAME, to_jsonb(OLD)::text) INTO covered;
        IF covered THEN
            RAISE EXCEPTION 'a row of %.% is under legal hold %: it cannot be changed or removed until the hold is lifted', TG_TABLE_SCHEMA, TG_TABLE_NAME, held.hold_name USING ERRCODE = '55000';
        END IF;
    END LOOP;
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
        AND NEW.table_name = OLD.table_name AND NEW.reason = OLD.reason AND NEW.placed_at = OLD.placed_at
        AND NEW.row_condition IS NOT DISTINCT FROM OLD.row_condition THEN
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
CREATE FUNCTION eggdb_retention.place_hold(hold_name text, schema_name text, table_name text, reason text, row_condition text DEFAULT NULL) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO eggdb_retention.holds (hold_name, schema_name, table_name, reason, row_condition) VALUES (hold_name, schema_name, table_name, reason, row_condition);
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
        SELECT h.hold_name INTO hold FROM eggdb_retention.holds h WHERE h.schema_name = pending.schema_name AND h.table_name = pending.table_name AND h.lifted_at IS NULL AND h.row_condition IS NULL ORDER BY h.hold_name LIMIT 1;
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
            UPDATE eggdb_retention.erasure_requests SET waiting_for = 'retention or a legal hold on some rows' WHERE request_id = pending.request_id;
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
CREATE TABLE eggdb_retention.record_classes (record_class text PRIMARY KEY, retain interval NOT NULL CHECK (retain > interval '0'), disposition text NOT NULL DEFAULT 'delete' CHECK (disposition IN ('delete', 'review')), citation text);
CREATE TABLE eggdb_retention.scheduled_tables (schema_name text NOT NULL, table_name text NOT NULL, record_class text NOT NULL REFERENCES eggdb_retention.record_classes (record_class), scheduled_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (schema_name, table_name));
CREATE TABLE eggdb_retention.disposals (disposal_id bigserial PRIMARY KEY, schema_name text NOT NULL, table_name text NOT NULL, record_class text NOT NULL, disposed_at timestamptz NOT NULL DEFAULT now(), disposed_by text NOT NULL DEFAULT current_user, disposed_rows bigint NOT NULL);
CREATE TABLE eggdb_retention.ediscovery_exports (export_id bigserial PRIMARY KEY, matter text NOT NULL, schema_name text NOT NULL, table_name text NOT NULL, row_condition text NOT NULL, exported_at timestamptz NOT NULL DEFAULT now(), exported_by text NOT NULL DEFAULT current_user, documents bigint NOT NULL, sha256 text NOT NULL);
CREATE FUNCTION eggdb_retention.schedule(schema_name text, table_name text, time_column text, record_class text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    period interval;
BEGIN
    SELECT c.retain INTO period FROM eggdb_retention.record_classes c WHERE c.record_class = schedule.record_class;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'there is no record class %', record_class USING ERRCODE = '42704';
    END IF;
    PERFORM eggdb_retention.retain(schema_name, table_name, time_column, period);
    INSERT INTO eggdb_retention.scheduled_tables (schema_name, table_name, record_class) VALUES (schedule.schema_name, schedule.table_name, schedule.record_class);
END
$$;
CREATE FUNCTION eggdb_retention.row_holds_clause(schema_name text, table_name text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    clause text;
BEGIN
    SELECT string_agg(format('NOT coalesce((%s), false)', h.row_condition), ' AND ' ORDER BY h.hold_name) INTO clause
        FROM eggdb_retention.holds h WHERE h.schema_name = row_holds_clause.schema_name AND h.table_name = row_holds_clause.table_name AND h.lifted_at IS NULL AND h.row_condition IS NOT NULL;
    RETURN coalesce(clause, 'true');
END
$$;
CREATE FUNCTION eggdb_retention.due_for_disposition() RETURNS TABLE (schema_name text, table_name text, record_class text, disposition text, due_rows bigint) LANGUAGE plpgsql AS $$
DECLARE
    scheduled record;
    counted bigint;
BEGIN
    FOR scheduled IN SELECT st.schema_name AS s_schema, st.table_name AS s_table, st.record_class AS s_class, c.disposition AS s_disposition, r.time_column AS s_time, r.retain AS s_retain
            FROM eggdb_retention.scheduled_tables st
            JOIN eggdb_retention.record_classes c ON c.record_class = st.record_class
            JOIN eggdb_retention.rules r ON r.schema_name = st.schema_name AND r.table_name = st.table_name
            ORDER BY st.schema_name, st.table_name LOOP
        EXECUTE format('SELECT count(*) FROM %I.%I WHERE %I + %L::interval <= now() AND %s', scheduled.s_schema, scheduled.s_table, scheduled.s_time, scheduled.s_retain, eggdb_retention.row_holds_clause(scheduled.s_schema, scheduled.s_table)) INTO counted;
        schema_name := scheduled.s_schema;
        table_name := scheduled.s_table;
        record_class := scheduled.s_class;
        disposition := scheduled.s_disposition;
        due_rows := counted;
        RETURN NEXT;
    END LOOP;
END
$$;
CREATE FUNCTION eggdb_retention.dispose(schema_name text, table_name text, reviewed boolean DEFAULT false) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    scheduled record;
    hold text;
    removed bigint;
BEGIN
    SELECT st.record_class AS s_class, c.disposition AS s_disposition, r.time_column AS s_time, r.retain AS s_retain INTO scheduled
        FROM eggdb_retention.scheduled_tables st
        JOIN eggdb_retention.record_classes c ON c.record_class = st.record_class
        JOIN eggdb_retention.rules r ON r.schema_name = st.schema_name AND r.table_name = st.table_name
        WHERE st.schema_name = dispose.schema_name AND st.table_name = dispose.table_name;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'table %.% has no records schedule', schema_name, table_name USING ERRCODE = '42704';
    END IF;
    IF scheduled.s_disposition = 'review' AND NOT reviewed THEN
        RAISE EXCEPTION 'record class % needs a reviewed disposition: call dispose with reviewed => true', scheduled.s_class USING ERRCODE = '55000';
    END IF;
    SELECT h.hold_name INTO hold FROM eggdb_retention.holds h WHERE h.schema_name = dispose.schema_name AND h.table_name = dispose.table_name AND h.lifted_at IS NULL AND h.row_condition IS NULL ORDER BY h.hold_name LIMIT 1;
    IF hold IS NOT NULL THEN
        RAISE EXCEPTION 'table %.% is under legal hold %', schema_name, table_name, hold USING ERRCODE = '55000';
    END IF;
    EXECUTE format('DELETE FROM %I.%I WHERE %I + %L::interval <= now() AND %s', schema_name, table_name, scheduled.s_time, scheduled.s_retain, eggdb_retention.row_holds_clause(schema_name, table_name));
    GET DIAGNOSTICS removed = ROW_COUNT;
    INSERT INTO eggdb_retention.disposals (schema_name, table_name, record_class, disposed_rows) VALUES (dispose.schema_name, dispose.table_name, scheduled.s_class, removed);
    RETURN removed;
END
$$;
CREATE FUNCTION eggdb_retention.xml_text(value text) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
    RETURN replace(replace(replace(replace(replace(translate(value, chr(1) || chr(2) || chr(3) || chr(4) || chr(5) || chr(6) || chr(7) || chr(8) || chr(11) || chr(12) || chr(14) || chr(15) || chr(16) || chr(17) || chr(18) || chr(19) || chr(20) || chr(21) || chr(22) || chr(23) || chr(24) || chr(25) || chr(26) || chr(27) || chr(28) || chr(29) || chr(30) || chr(31), ''),
        '&', '&amp;'), '<', '&lt;'), '>', '&gt;'), '"', '&quot;'), '''', '&apos;');
END
$$;
CREATE FUNCTION eggdb_retention.export_ediscovery(matter text, schema_name text, table_name text, row_condition text DEFAULT 'true', key_column text DEFAULT NULL) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    found_row record;
    field record;
    tags text;
    documents text := '';
    counted bigint := 0;
    document_id text;
    exported text;
BEGIN
    FOR found_row IN EXECUTE format('SELECT to_jsonb(t) AS body FROM %I.%I t WHERE %s', schema_name, table_name, row_condition) LOOP
        counted := counted + 1;
        document_id := schema_name || '.' || table_name || '-' || coalesce(CASE WHEN key_column IS NULL THEN NULL ELSE found_row.body ->> key_column END, counted::text);
        tags := format('<Tag TagName="%s" TagDataType="Text" TagValue="%s"/>', chr(35) || 'Table', eggdb_retention.xml_text(schema_name || '.' || table_name));
        FOR field IN SELECT f.key AS name, f.value AS value FROM jsonb_each_text(found_row.body) f ORDER BY f.key LOOP
            tags := tags || format('<Tag TagName="%s" TagDataType="Text" TagValue="%s"/>', eggdb_retention.xml_text(field.name), eggdb_retention.xml_text(coalesce(field.value, '')));
        END LOOP;
        documents := documents || format('<Document DocID="%s" DocType="Record" MimeType="application/json"><Tags>%s</Tags></Document>', eggdb_retention.xml_text(document_id), tags) || chr(10);
    END LOOP;
    exported := '<?xml version="1.0" encoding="UTF-8"?>' || chr(10)
        || format('<Root DataInterchangeType="Update" Description="%s">', eggdb_retention.xml_text('EggDB records for matter ' || matter)) || chr(10)
        || format('<Batch name="%s"><Documents>', eggdb_retention.xml_text(matter)) || chr(10)
        || documents
        || '</Documents></Batch></Root>' || chr(10);
    INSERT INTO eggdb_retention.ediscovery_exports (matter, schema_name, table_name, row_condition, documents, sha256)
        VALUES (export_ediscovery.matter, export_ediscovery.schema_name, export_ediscovery.table_name, export_ediscovery.row_condition, counted, encode(sha256(exported), 'hex'));
    RETURN exported;
END
$$;
"##;

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

    #[test]
    fn row_holds_schedules_and_ediscovery_exports() {
        let store = records();
        let mut session = Session::new(&store);
        for sql in [
            "INSERT INTO eggdb_retention.record_classes (record_class, retain, citation) VALUES ('trade records', interval '7 years', 'SEC 17a-4')",
            "SELECT eggdb_retention.schedule('public', 'trades', 'booked_at', 'trade records')",
            "SELECT eggdb_retention.place_hold('case-7', 'public', 'trades', 'litigation', 'client = ''bob''')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT record_class, disposition, due_rows FROM eggdb_retention.due_for_disposition()").unwrap(),
            "record_class|disposition|due_rows\ntrade records|delete|1\nSELECT 1"
        );
        assert_eq!(run(&mut session, "DELETE FROM trades WHERE id = 3").unwrap_err().state().code(), "55000");
        assert_eq!(run(&mut session, "SELECT eggdb_retention.dispose('public', 'trades') AS disposed").unwrap(), "disposed\n1\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT id FROM trades ORDER BY id").unwrap(), "id\n2\n3\nSELECT 2");
        let exported = run(&mut session, "SELECT eggdb_retention.export_ediscovery('case-7', 'public', 'trades', 'client = ''bob''', 'id') AS xml").unwrap();
        assert!(exported.contains("<Document DocID=\"public.trades-3\""), "{exported}");
        assert!(exported.contains("<Tag TagName=\"client\" TagDataType=\"Text\" TagValue=\"bob\"/>"), "{exported}");
        assert_eq!(run(&mut session, "SELECT documents, length(sha256) AS digest FROM eggdb_retention.ediscovery_exports").unwrap(), "documents|digest\n1|64\nSELECT 1");
    }
}
