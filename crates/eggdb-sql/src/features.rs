/// The statements `CREATE EXTENSION eggdb_features` runs: feature views over SQL queries,
/// materialized into an online table of each entity's latest features, and point-in-time reads.
pub(crate) const FEATURES_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_features;
CREATE TABLE eggdb_features.feature_views (view_name text PRIMARY KEY, source_query text NOT NULL, entity_column text NOT NULL, time_column text NOT NULL, ttl interval, materialized_at timestamptz);
CREATE TABLE eggdb_features.online (view_name text NOT NULL, entity_key text NOT NULL, features jsonb NOT NULL, event_time timestamptz NOT NULL, written_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (view_name, entity_key));
CREATE FUNCTION eggdb_features.define_view(view_name text, source_query text, entity_column text, time_column text, ttl interval DEFAULT NULL) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO eggdb_features.feature_views (view_name, source_query, entity_column, time_column, ttl)
        VALUES (define_view.view_name, define_view.source_query, define_view.entity_column, define_view.time_column, define_view.ttl)
        ON CONFLICT (view_name) DO UPDATE SET source_query = EXCLUDED.source_query, entity_column = EXCLUDED.entity_column, time_column = EXCLUDED.time_column, ttl = EXCLUDED.ttl;
END
$$;
CREATE FUNCTION eggdb_features.materialize(wanted text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    definition record;
    found record;
    entity text;
    happened timestamptz;
    written bigint := 0;
BEGIN
    SELECT * INTO definition FROM eggdb_features.feature_views v WHERE v.view_name = wanted;
    IF definition IS NULL THEN
        RAISE EXCEPTION 'feature view % does not exist', wanted USING ERRCODE = '42704';
    END IF;
    FOR found IN EXECUTE format('SELECT to_jsonb(q) AS row FROM (%s) q', definition.source_query) LOOP
        entity := found.row ->> definition.entity_column;
        happened := (found.row ->> definition.time_column)::timestamptz;
        IF entity IS NULL OR happened IS NULL THEN
            CONTINUE;
        END IF;
        INSERT INTO eggdb_features.online (view_name, entity_key, features, event_time)
            VALUES (wanted, entity, found.row - definition.entity_column - definition.time_column, happened)
            ON CONFLICT (view_name, entity_key) DO UPDATE SET features = EXCLUDED.features, event_time = EXCLUDED.event_time, written_at = now()
            WHERE EXCLUDED.event_time >= eggdb_features.online.event_time;
        written := written + 1;
    END LOOP;
    UPDATE eggdb_features.feature_views SET materialized_at = now() WHERE view_name = wanted;
    RETURN written;
END
$$;
CREATE FUNCTION eggdb_features.online_features(wanted text, entity text) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE
    kept_for interval;
    stored jsonb;
    happened timestamptz;
BEGIN
    SELECT v.ttl INTO kept_for FROM eggdb_features.feature_views v WHERE v.view_name = wanted;
    SELECT o.features, o.event_time INTO stored, happened FROM eggdb_features.online o WHERE o.view_name = wanted AND o.entity_key = entity;
    IF stored IS NULL OR (kept_for IS NOT NULL AND happened < now() - kept_for) THEN
        RETURN NULL;
    END IF;
    RETURN stored || jsonb_build_object('event_time', happened);
END
$$;
CREATE FUNCTION eggdb_features.features_at(wanted text, entity text, at_time timestamptz) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE
    definition record;
    found jsonb;
BEGIN
    SELECT * INTO definition FROM eggdb_features.feature_views v WHERE v.view_name = wanted;
    IF definition IS NULL THEN
        RAISE EXCEPTION 'feature view % does not exist', wanted USING ERRCODE = '42704';
    END IF;
    EXECUTE format(
        'SELECT to_jsonb(q) FROM (%s) q WHERE (to_jsonb(q) ->> %L) = $1 AND (to_jsonb(q) ->> %L)::timestamptz <= $2 AND ($3::interval IS NULL OR (to_jsonb(q) ->> %L)::timestamptz > $2 - $3::interval) ORDER BY (to_jsonb(q) ->> %L)::timestamptz DESC LIMIT 1',
        definition.source_query, definition.entity_column, definition.time_column, definition.time_column, definition.time_column)
        INTO found USING entity, at_time, definition.ttl;
    IF found IS NULL THEN
        RETURN NULL;
    END IF;
    RETURN found - definition.entity_column;
END
$$;
"#;
