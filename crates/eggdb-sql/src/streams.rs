/// The statements `CREATE EXTENSION eggdb_streams` runs: streaming SQL over append-only tables,
/// with event time, a watermark that trails the newest event by the allowed lateness, events
/// older than the watermark kept aside as late, and tumbling, hopping and session windows
/// emitted into tables once the watermark passes their end (D55).
pub(crate) const STREAMS_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_streams;
CREATE TABLE eggdb_streams.streams (stream text PRIMARY KEY, schema_name text NOT NULL, table_name text NOT NULL, time_column text NOT NULL, allowed_lateness interval NOT NULL DEFAULT interval '0' CHECK (allowed_lateness >= interval '0'), watermark timestamptz NOT NULL DEFAULT '-infinity', UNIQUE (schema_name, table_name));
CREATE TABLE eggdb_streams.late_events (late_id bigserial PRIMARY KEY, stream text NOT NULL REFERENCES eggdb_streams.streams (stream), event_time timestamptz, arrived_at timestamptz NOT NULL DEFAULT now(), event jsonb NOT NULL);
CREATE TABLE eggdb_streams.joins (join_name text PRIMARY KEY, left_stream text NOT NULL REFERENCES eggdb_streams.streams (stream), right_stream text NOT NULL REFERENCES eggdb_streams.streams (stream), within interval NOT NULL CHECK (within >= interval '0'), join_condition text NOT NULL, select_list text NOT NULL, target_table text NOT NULL, emitted_until timestamptz NOT NULL DEFAULT '-infinity');
CREATE TABLE eggdb_streams.windows (window_name text PRIMARY KEY, stream text NOT NULL REFERENCES eggdb_streams.streams (stream), kind text NOT NULL CHECK (kind IN ('tumbling', 'hopping', 'session')), size interval, slide interval, gap interval, group_columns text[] NOT NULL DEFAULT '{}', aggregates text NOT NULL, target_table text NOT NULL, emitted_until timestamptz, open_from timestamptz NOT NULL DEFAULT '-infinity');
CREATE FUNCTION eggdb_streams.divert_late() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    found_stream record;
    event_time timestamptz;
BEGIN
    IF current_setting('eggdb_streams.replaying', true) = 'on' THEN
        RETURN NEW;
    END IF;
    SELECT s.stream, s.time_column, s.watermark INTO found_stream FROM eggdb_streams.streams s WHERE s.schema_name = TG_TABLE_SCHEMA AND s.table_name = TG_TABLE_NAME;
    event_time := (to_jsonb(NEW) ->> found_stream.time_column)::timestamptz;
    IF event_time IS NULL OR event_time < found_stream.watermark THEN
        INSERT INTO eggdb_streams.late_events (stream, event_time, event) VALUES (found_stream.stream, event_time, to_jsonb(NEW));
        RETURN NULL;
    END IF;
    RETURN NEW;
END
$$;
CREATE FUNCTION eggdb_streams.create_stream(stream_name text, schema_name text, table_name text, time_column text, allowed_lateness interval DEFAULT interval '0') RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO eggdb_streams.streams (stream, schema_name, table_name, time_column, allowed_lateness) VALUES (stream_name, schema_name, table_name, time_column, allowed_lateness);
    EXECUTE format('CREATE TRIGGER eggdb_streams_late BEFORE INSERT ON %I.%I FOR EACH ROW EXECUTE FUNCTION eggdb_streams.divert_late()', schema_name, table_name);
END
$$;
CREATE FUNCTION eggdb_streams.window_sql(wanted_window text, lower_bound timestamptz, upper_bound timestamptz) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    job record;
    source record;
    group_column text;
    grouped_e text := '';
    grouped_s text := '';
    partition_x text := '';
    partition_e text := '';
BEGIN
    SELECT * INTO job FROM eggdb_streams.windows w WHERE w.window_name = wanted_window;
    SELECT * INTO source FROM eggdb_streams.streams s WHERE s.stream = job.stream;
    FOREACH group_column IN ARRAY job.group_columns LOOP
        grouped_e := grouped_e || ', e.' || quote_ident(group_column);
        grouped_s := grouped_s || ', s.' || quote_ident(group_column);
        partition_x := partition_x || CASE WHEN partition_x = '' THEN 'PARTITION BY ' ELSE ', ' END || 'x.' || quote_ident(group_column);
        partition_e := partition_e || CASE WHEN partition_e = '' THEN 'PARTITION BY ' ELSE ', ' END || 'e.' || quote_ident(group_column);
    END LOOP;
    IF job.kind = 'session' THEN
        RETURN format('SELECT min(s.%1$I) AS window_start, max(s.%1$I) + %2$L::interval AS window_end%3$s, %4$s FROM ('
            || 'SELECT e.*, sum(CASE WHEN e.eggdb_previous IS NULL OR e.%1$I - e.eggdb_previous >= %2$L::interval THEN 1 ELSE 0 END) OVER (%5$s ORDER BY e.%1$I ROWS UNBOUNDED PRECEDING) AS eggdb_session FROM ('
            || 'SELECT x.*, lag(x.%1$I) OVER (%6$s ORDER BY x.%1$I) AS eggdb_previous FROM %7$I.%8$I x WHERE x.%1$I >= %9$L::timestamptz) e) s GROUP BY s.eggdb_session%3$s',
            source.time_column, job.gap, grouped_s, job.aggregates, partition_e, partition_x, source.schema_name, source.table_name, lower_bound);
    END IF;
    RETURN format('SELECT w.window_start, w.window_start + %1$L::interval AS window_end%2$s, %3$s FROM generate_series(%4$L::timestamptz, %5$L::timestamptz, %6$L::interval) AS w(window_start) '
        || 'JOIN %7$I.%8$I e ON e.%9$I >= w.window_start AND e.%9$I < w.window_start + %1$L::interval GROUP BY w.window_start%2$s',
        job.size, grouped_e, job.aggregates, lower_bound, upper_bound, coalesce(job.slide, job.size), source.schema_name, source.table_name, source.time_column);
END
$$;
CREATE FUNCTION eggdb_streams.create_window(window_name text, stream_name text, kind text, aggregates text, target_table text, size interval DEFAULT NULL, slide interval DEFAULT NULL, gap interval DEFAULT NULL, group_columns text[] DEFAULT '{}') RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    IF kind IN ('tumbling', 'hopping') AND (size IS NULL OR size <= interval '0') THEN
        RAISE EXCEPTION 'a % window needs a positive size', kind USING ERRCODE = '22023';
    END IF;
    IF kind = 'hopping' AND (slide IS NULL OR slide <= interval '0' OR slide > size OR extract(epoch FROM size)::bigint % extract(epoch FROM slide)::bigint <> 0) THEN
        RAISE EXCEPTION 'a hopping window needs a slide that divides its size' USING ERRCODE = '22023';
    END IF;
    IF kind = 'session' AND (gap IS NULL OR gap <= interval '0') THEN
        RAISE EXCEPTION 'a session window needs a positive gap' USING ERRCODE = '22023';
    END IF;
    INSERT INTO eggdb_streams.windows (window_name, stream, kind, size, slide, gap, group_columns, aggregates, target_table)
        VALUES (create_window.window_name, stream_name, create_window.kind, create_window.size, CASE WHEN create_window.kind = 'tumbling' THEN create_window.size ELSE create_window.slide END,
                create_window.gap, create_window.group_columns, create_window.aggregates, create_window.target_table);
    EXECUTE format('CREATE TABLE %s AS SELECT * FROM (%s) q WHERE false', create_window.target_table,
        eggdb_streams.window_sql(create_window.window_name, CASE WHEN create_window.kind = 'session' THEN timestamptz 'infinity' ELSE timestamptz '2000-01-02 00:00+00' END, timestamptz '2000-01-01 00:00+00'));
END
$$;
CREATE FUNCTION eggdb_streams.emit(wanted_window text, watermark timestamptz) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    job record;
    source record;
    first_start timestamptz;
    last_start timestamptz;
    oldest timestamptz;
    still_open timestamptz;
    emitted bigint := 0;
BEGIN
    SELECT * INTO job FROM eggdb_streams.windows w WHERE w.window_name = wanted_window;
    SELECT * INTO source FROM eggdb_streams.streams s WHERE s.stream = job.stream;
    IF watermark = '-infinity' THEN
        RETURN 0;
    END IF;
    IF job.kind = 'session' THEN
        EXECUTE format('INSERT INTO %s SELECT * FROM (%s) q WHERE q.window_end <= %L::timestamptz AND q.window_end > %L::timestamptz',
            job.target_table, eggdb_streams.window_sql(job.window_name, job.open_from, NULL), watermark, coalesce(job.emitted_until, '-infinity'));
        GET DIAGNOSTICS emitted = ROW_COUNT;
        EXECUTE format('SELECT min(q.window_start) FROM (%s) q WHERE q.window_end > %L::timestamptz', eggdb_streams.window_sql(job.window_name, job.open_from, NULL), watermark) INTO still_open;
        UPDATE eggdb_streams.windows SET emitted_until = watermark, open_from = coalesce(still_open, watermark) WHERE window_name = wanted_window;
        RETURN emitted;
    END IF;
    IF job.emitted_until IS NULL THEN
        EXECUTE format('SELECT min(%I) FROM %I.%I', source.time_column, source.schema_name, source.table_name) INTO oldest;
        IF oldest IS NULL THEN
            RETURN 0;
        END IF;
        first_start := time_bucket(job.slide, oldest) - job.size + job.slide;
    ELSE
        first_start := time_bucket(job.slide, job.emitted_until - job.size) + job.slide;
    END IF;
    last_start := time_bucket(job.slide, watermark - job.size);
    IF first_start > last_start THEN
        RETURN 0;
    END IF;
    EXECUTE format('INSERT INTO %s %s', job.target_table, eggdb_streams.window_sql(job.window_name, first_start, last_start));
    GET DIAGNOSTICS emitted = ROW_COUNT;
    UPDATE eggdb_streams.windows SET emitted_until = last_start + job.size WHERE window_name = wanted_window;
    RETURN emitted;
END
$$;
CREATE FUNCTION eggdb_streams.advance(stream_name text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    source record;
    newest timestamptz;
    mark timestamptz;
    job record;
    emitted bigint := 0;
BEGIN
    SELECT * INTO source FROM eggdb_streams.streams s WHERE s.stream = stream_name;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'there is no stream %', stream_name USING ERRCODE = '42704';
    END IF;
    EXECUTE format('SELECT max(%I) FROM %I.%I', source.time_column, source.schema_name, source.table_name) INTO newest;
    mark := source.watermark;
    IF newest IS NOT NULL AND newest - source.allowed_lateness > mark THEN
        mark := newest - source.allowed_lateness;
        UPDATE eggdb_streams.streams SET watermark = mark WHERE stream = stream_name;
    END IF;
    FOR job IN SELECT w.window_name FROM eggdb_streams.windows w WHERE w.stream = stream_name ORDER BY w.window_name LOOP
        emitted := emitted + eggdb_streams.emit(job.window_name, mark);
    END LOOP;
    FOR job IN SELECT j.join_name FROM eggdb_streams.joins j WHERE j.left_stream = stream_name OR j.right_stream = stream_name ORDER BY j.join_name LOOP
        emitted := emitted + eggdb_streams.emit_join(job.join_name);
    END LOOP;
    RETURN emitted;
END
$$;
CREATE FUNCTION eggdb_streams.join_sql(wanted_join text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    joined record;
    left_source record;
    right_source record;
BEGIN
    SELECT * INTO joined FROM eggdb_streams.joins j WHERE j.join_name = wanted_join;
    SELECT * INTO left_source FROM eggdb_streams.streams s WHERE s.stream = joined.left_stream;
    SELECT * INTO right_source FROM eggdb_streams.streams s WHERE s.stream = joined.right_stream;
    RETURN format('SELECT %1$s FROM %2$I.%3$I l JOIN %4$I.%5$I r ON (%6$s) AND r.%8$I >= l.%7$I - %9$L::interval AND r.%8$I <= l.%7$I + %9$L::interval WHERE greatest(l.%7$I, r.%8$I) > $from AND greatest(l.%7$I, r.%8$I) <= $until',
        joined.select_list, left_source.schema_name, left_source.table_name, right_source.schema_name, right_source.table_name, joined.join_condition,
        left_source.time_column, right_source.time_column, joined.within);
END
$$;
CREATE FUNCTION eggdb_streams.create_join(join_name text, left_stream text, right_stream text, join_condition text, within interval, select_list text, target_table text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO eggdb_streams.joins (join_name, left_stream, right_stream, within, join_condition, select_list, target_table)
        VALUES (create_join.join_name, create_join.left_stream, create_join.right_stream, create_join.within, create_join.join_condition, create_join.select_list, create_join.target_table);
    EXECUTE format('CREATE TABLE %s AS %s', create_join.target_table,
        replace(replace(eggdb_streams.join_sql(create_join.join_name), '$from', quote_literal('infinity') || '::timestamptz'), '$until', quote_literal('-infinity') || '::timestamptz'));
END
$$;
CREATE FUNCTION eggdb_streams.emit_join(wanted_join text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    joined record;
    mark timestamptz;
    emitted bigint;
BEGIN
    SELECT * INTO joined FROM eggdb_streams.joins j WHERE j.join_name = wanted_join;
    SELECT min(s.watermark) INTO mark FROM eggdb_streams.streams s WHERE s.stream IN (joined.left_stream, joined.right_stream);
    IF mark IS NULL OR mark <= joined.emitted_until THEN
        RETURN 0;
    END IF;
    EXECUTE format('INSERT INTO %s %s', joined.target_table,
        replace(replace(eggdb_streams.join_sql(wanted_join), '$from', quote_literal(joined.emitted_until::text) || '::timestamptz'), '$until', quote_literal(mark::text) || '::timestamptz'));
    GET DIAGNOSTICS emitted = ROW_COUNT;
    UPDATE eggdb_streams.joins SET emitted_until = mark WHERE join_name = wanted_join;
    RETURN emitted;
END
$$;
CREATE FUNCTION eggdb_streams.match_recognize(query text, partition_by text, order_by text, pattern text, definitions jsonb) RETURNS TABLE (partition_key text, match_number bigint, first_row jsonb, last_row jsonb, matched_rows bigint, classifiers text[]) LANGUAGE plpgsql AS $$
DECLARE
    flag_list text;
    found_row record;
    current_key text;
    bodies jsonb := '[]'::jsonb;
    flags jsonb := '[]'::jsonb;
    found jsonb;
    counted bigint := 0;
    started boolean := false;
BEGIN
    SELECT string_agg(format('%L, coalesce((%s), false)', upper(d.key), d.value), ', ') INTO flag_list FROM jsonb_each_text(definitions) d;
    FOR found_row IN EXECUTE format('SELECT coalesce((%s)::text, %L) AS part, to_jsonb(q) AS body, jsonb_build_object(%s) AS flags FROM (%s) q ORDER BY 1, %s',
            coalesce(nullif(partition_by, ''), 'NULL'), '', coalesce(flag_list, ''), query, order_by) LOOP
        IF started AND found_row.part IS DISTINCT FROM current_key THEN
            FOR found IN SELECT m.value AS matched FROM jsonb_array_elements(row_pattern_matches(flags, pattern)) m LOOP
                counted := counted + 1;
                partition_key := current_key;
                match_number := counted;
                first_row := bodies -> ((found.matched ->> 'start')::integer);
                last_row := bodies -> ((found.matched ->> 'end')::integer);
                matched_rows := (found.matched ->> 'end')::bigint - (found.matched ->> 'start')::bigint + 1;
                classifiers := ARRAY(SELECT c.value FROM jsonb_array_elements_text(found.matched -> 'classifiers') c);
                RETURN NEXT;
            END LOOP;
            bodies := '[]'::jsonb;
            flags := '[]'::jsonb;
        END IF;
        started := true;
        current_key := found_row.part;
        bodies := bodies || jsonb_build_array(found_row.body);
        flags := flags || jsonb_build_array(found_row.flags);
    END LOOP;
    IF started THEN
        FOR found IN SELECT m.value AS matched FROM jsonb_array_elements(row_pattern_matches(flags, pattern)) m LOOP
            counted := counted + 1;
            partition_key := current_key;
            match_number := counted;
            first_row := bodies -> ((found.matched ->> 'start')::integer);
            last_row := bodies -> ((found.matched ->> 'end')::integer);
            matched_rows := (found.matched ->> 'end')::bigint - (found.matched ->> 'start')::bigint + 1;
            classifiers := ARRAY(SELECT c.value FROM jsonb_array_elements_text(found.matched -> 'classifiers') c);
            RETURN NEXT;
        END LOOP;
    END IF;
END
$$;
CREATE FUNCTION eggdb_streams.replay_late(stream_name text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    source record;
    late record;
    job record;
    first_start timestamptz;
    last_start timestamptz;
    replayed bigint := 0;
BEGIN
    SELECT * INTO source FROM eggdb_streams.streams s WHERE s.stream = stream_name;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'there is no stream %', stream_name USING ERRCODE = '42704';
    END IF;
    PERFORM set_config('eggdb_streams.replaying', 'on', true);
    FOR late IN SELECT l.late_id, l.event_time, l.event FROM eggdb_streams.late_events l WHERE l.stream = stream_name AND l.event_time IS NOT NULL ORDER BY l.late_id LOOP
        EXECUTE format('INSERT INTO %I.%I SELECT * FROM jsonb_populate_record(NULL::%I.%I, %L::jsonb)', source.schema_name, source.table_name, source.schema_name, source.table_name, late.event::text);
        FOR job IN SELECT w.window_name, w.size, w.slide, w.target_table FROM eggdb_streams.windows w
                WHERE w.stream = stream_name AND w.kind IN ('tumbling', 'hopping') AND w.emitted_until IS NOT NULL AND late.event_time < w.emitted_until ORDER BY w.window_name LOOP
            first_start := time_bucket(job.slide, late.event_time) - job.size + job.slide;
            last_start := least(time_bucket(job.slide, late.event_time), job.emitted_until - job.size);
            EXECUTE format('DELETE FROM %s WHERE window_start >= %L::timestamptz AND window_start <= %L::timestamptz', job.target_table, first_start, last_start);
            EXECUTE format('INSERT INTO %s %s', job.target_table, eggdb_streams.window_sql(job.window_name, first_start, last_start));
        END LOOP;
        DELETE FROM eggdb_streams.late_events l WHERE l.late_id = late.late_id;
        replayed := replayed + 1;
    END LOOP;
    PERFORM set_config('eggdb_streams.replaying', 'off', true);
    RETURN replayed;
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
    fn windows_close_when_the_watermark_passes_and_late_events_go_aside() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "SET TimeZone = 'UTC'",
            "CREATE EXTENSION eggdb_streams",
            "CREATE TABLE clicks (user_id text NOT NULL, at timestamptz NOT NULL, amount integer NOT NULL)",
            "SELECT eggdb_streams.create_stream('clicks', 'public', 'clicks', 'at')",
            "SELECT eggdb_streams.create_window('clicks per hour', 'clicks', 'tumbling', 'count(*) AS events, sum(amount) AS total', 'clicks_hourly', interval '1 hour')",
            "SELECT eggdb_streams.create_window('visits', 'clicks', 'session', 'count(*) AS events', 'clicks_sessions', NULL, NULL, interval '30 minutes', ARRAY['user_id'])",
            "INSERT INTO clicks VALUES ('u1', '2026-11-12 10:05+00', 1), ('u1', '2026-11-12 10:30+00', 2), ('u1', '2026-11-12 11:10+00', 4), ('u2', '2026-11-12 12:20+00', 8)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT eggdb_streams.advance('clicks') AS emitted").unwrap(), "emitted\n4\nSELECT 1");
        assert_eq!(
            run(&mut session, "SELECT extract(hour FROM window_start) AS hour, events, total FROM clicks_hourly ORDER BY window_start").unwrap(),
            "hour|events|total\n10|2|3\n11|1|4\nSELECT 2"
        );
        assert_eq!(
            run(&mut session, "SELECT user_id, events FROM clicks_sessions ORDER BY window_start").unwrap(),
            "user_id|events\nu1|2\nu1|1\nSELECT 2"
        );
        run(&mut session, "INSERT INTO clicks VALUES ('u3', '2026-11-12 11:30+00', 16)").unwrap();
        assert_eq!(run(&mut session, "SELECT count(*) AS late FROM eggdb_streams.late_events").unwrap(), "late\n1\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT count(*) AS kept FROM clicks").unwrap(), "kept\n4\nSELECT 1");
    }

    #[test]
    fn interval_joins_pair_events_and_late_events_correct_closed_windows() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "SET TimeZone = 'UTC'",
            "CREATE EXTENSION eggdb_streams",
            "CREATE TABLE orders (order_id integer, at timestamptz NOT NULL)",
            "CREATE TABLE payments (order_id integer, at timestamptz NOT NULL, amount integer)",
            "SELECT eggdb_streams.create_stream('orders', 'public', 'orders', 'at')",
            "SELECT eggdb_streams.create_stream('payments', 'public', 'payments', 'at')",
            "SELECT eggdb_streams.create_join('paid orders', 'orders', 'payments', 'l.order_id = r.order_id', interval '10 minutes', 'l.order_id, r.amount', 'paid_orders')",
            "SELECT eggdb_streams.create_window('payments per hour', 'payments', 'tumbling', 'count(*) AS payments', 'payments_hourly', interval '1 hour')",
            "INSERT INTO orders VALUES (1, '2026-11-12 10:00+00'), (2, '2026-11-12 10:05+00'), (3, '2026-11-12 12:00+00')",
            "INSERT INTO payments VALUES (1, '2026-11-12 10:04+00', 30), (2, '2026-11-12 10:40+00', 20), (3, '2026-11-12 12:01+00', 10)",
            "SELECT eggdb_streams.advance('orders')",
            "SELECT eggdb_streams.advance('payments')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT order_id, amount FROM paid_orders ORDER BY order_id").unwrap(), "order_id|amount\n1|30\nSELECT 1");
        run(&mut session, "INSERT INTO payments VALUES (9, '2026-11-12 10:50+00', 5)").unwrap();
        assert_eq!(run(&mut session, "SELECT payments FROM payments_hourly ORDER BY window_start").unwrap(), "payments\n2\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT eggdb_streams.replay_late('payments') AS replayed").unwrap(), "replayed\n1\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT payments FROM payments_hourly ORDER BY window_start").unwrap(), "payments\n3\nSELECT 1");
    }

    #[test]
    fn match_recognize_finds_v_shapes_per_partition() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_streams",
            "CREATE TABLE ticks (symbol text, at integer, price integer)",
            "INSERT INTO ticks VALUES ('EGG', 1, 10), ('EGG', 2, 8), ('EGG', 3, 7), ('EGG', 4, 9), ('EGG', 5, 12), ('HEN', 1, 5), ('HEN', 2, 6)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT partition_key, first_row ->> 'at' AS starts, last_row ->> 'at' AS ends, classifiers FROM eggdb_streams.match_recognize('SELECT symbol, at, price, lag(price) OVER (PARTITION BY symbol ORDER BY at) AS previous FROM ticks', 'symbol', 'at', 'strt down+ up+', '{\"down\": \"price < previous\", \"up\": \"price > previous\"}')").unwrap(),
            "partition_key|starts|ends|classifiers\nEGG|1|5|{STRT,DOWN,DOWN,UP,UP}\nSELECT 1"
        );
    }

    #[test]
    fn the_match_recognize_clause_measures_each_match() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE TABLE ticks (symbol text, at integer, price integer)",
            "INSERT INTO ticks VALUES ('EGG', 1, 10), ('EGG', 2, 8), ('EGG', 3, 7), ('EGG', 4, 9), ('EGG', 5, 12), ('HEN', 1, 5), ('HEN', 2, 6)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(
                &mut session,
                "SELECT * FROM ticks MATCH_RECOGNIZE (PARTITION BY symbol ORDER BY at \
                 MEASURES FIRST(at) AS start_at, LAST(DOWN.price) AS bottom, LAST(at) AS end_at, COUNT(DOWN.*) AS downs, \
                 MATCH_NUMBER() AS match_no, CLASSIFIER() AS last_kind, STRT.price AS opening, MAX(UP.price) - MIN(DOWN.price) AS rebound \
                 ONE ROW PER MATCH AFTER MATCH SKIP PAST LAST ROW PATTERN (STRT DOWN+ UP{1,3}) \
                 DEFINE DOWN AS price < PREV(price), UP AS price > PREV(price)) AS mr"
            )
            .unwrap(),
            "symbol|start_at|bottom|end_at|downs|match_no|last_kind|opening|rebound\nEGG|1|7|5|2|1|UP|10|5\nSELECT 1"
        );
        assert!(run(&mut session, "SELECT * FROM ticks MATCH_RECOGNIZE (ORDER BY at ALL ROWS PER MATCH PATTERN (A) DEFINE A AS price > 1) AS mr").is_err());
    }
}
