/// The statements `CREATE EXTENSION timescaledb` runs: hypertables recorded by their time column,
/// `drop_chunks` deleting rows older than an interval, and retention and continuous-aggregate
/// refresh policies run by the pg_cron scheduler (D200).
pub(crate) const TIMESCALE_SCRIPT: &str = r#"
CREATE EXTENSION IF NOT EXISTS pg_cron;
CREATE SCHEMA _timescaledb_catalog;
CREATE SCHEMA timescaledb_information;
CREATE TABLE _timescaledb_catalog.hypertable (id bigserial PRIMARY KEY, schema_name text NOT NULL, table_name text NOT NULL, time_column text NOT NULL, chunk_time_interval interval NOT NULL, UNIQUE (schema_name, table_name));
CREATE TABLE _timescaledb_catalog.policy (job_id bigint PRIMARY KEY, proc_name text NOT NULL, schema_name text NOT NULL, table_name text NOT NULL, drop_after interval, schedule_interval interval NOT NULL);
CREATE FUNCTION _timescaledb_catalog.split(relation text) RETURNS text[] LANGUAGE plpgsql AS $$
BEGIN
    IF position('.' IN relation) > 0 THEN
        RETURN ARRAY[split_part(relation, '.', 1), split_part(relation, '.', 2)];
    END IF;
    RETURN ARRAY[current_schema(), relation];
END
$$;
CREATE FUNCTION _timescaledb_catalog.cron_of(every interval) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    minutes bigint := greatest(1, (extract(epoch FROM every) / 60)::bigint);
BEGIN
    IF minutes < 60 THEN
        RETURN '*/' || minutes || ' * * * *';
    ELSIF minutes < 1440 THEN
        RETURN '0 */' || greatest(1, minutes / 60) || ' * * *';
    END IF;
    RETURN '0 0 * * *';
END
$$;
CREATE FUNCTION public.create_hypertable(relation text, time_column_name text, chunk_time_interval interval DEFAULT interval '7 days', if_not_exists boolean DEFAULT false, migrate_data boolean DEFAULT false) RETURNS TABLE (hypertable_id bigint, schema_name text, table_name text, created boolean) LANGUAGE plpgsql AS $$
DECLARE
    parts text[] := _timescaledb_catalog.split(relation);
    found bigint;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM information_schema.columns c WHERE c.table_schema = parts[1] AND c.table_name = parts[2] AND c.column_name = time_column_name) THEN
        RAISE EXCEPTION 'column "%" does not exist in table %.%', time_column_name, parts[1], parts[2] USING ERRCODE = '42703';
    END IF;
    SELECT h.id INTO found FROM _timescaledb_catalog.hypertable h WHERE h.schema_name = parts[1] AND h.table_name = parts[2];
    IF found IS NOT NULL THEN
        IF NOT if_not_exists THEN
            RAISE EXCEPTION 'table "%" is already a hypertable', parts[2] USING ERRCODE = '42710';
        END IF;
        RETURN QUERY SELECT found, parts[1], parts[2], false;
        RETURN;
    END IF;
    INSERT INTO _timescaledb_catalog.hypertable (schema_name, table_name, time_column, chunk_time_interval) VALUES (parts[1], parts[2], time_column_name, chunk_time_interval) RETURNING id INTO found;
    RETURN QUERY SELECT found, parts[1], parts[2], true;
END
$$;
CREATE FUNCTION public.drop_chunks(relation text, older_than interval) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    parts text[] := _timescaledb_catalog.split(relation);
    column_name text;
    removed bigint;
BEGIN
    SELECT h.time_column INTO column_name FROM _timescaledb_catalog.hypertable h WHERE h.schema_name = parts[1] AND h.table_name = parts[2];
    IF column_name IS NULL THEN
        RAISE EXCEPTION 'table "%" is not a hypertable', relation USING ERRCODE = '42704';
    END IF;
    EXECUTE format('DELETE FROM %I.%I WHERE %I < now() - %L::interval', parts[1], parts[2], column_name, older_than);
    GET DIAGNOSTICS removed = ROW_COUNT;
    RETURN removed;
END
$$;
CREATE FUNCTION public.add_retention_policy(relation text, drop_after interval, if_not_exists boolean DEFAULT false, schedule_interval interval DEFAULT interval '1 day') RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
    parts text[] := _timescaledb_catalog.split(relation);
    job bigint;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM _timescaledb_catalog.hypertable h WHERE h.schema_name = parts[1] AND h.table_name = parts[2]) THEN
        RAISE EXCEPTION 'table "%" is not a hypertable', relation USING ERRCODE = '42704';
    END IF;
    SELECT p.job_id INTO job FROM _timescaledb_catalog.policy p WHERE p.proc_name = 'policy_retention' AND p.schema_name = parts[1] AND p.table_name = parts[2];
    IF job IS NOT NULL THEN
        IF if_not_exists THEN
            RETURN -1;
        END IF;
        RAISE EXCEPTION 'retention policy already exists for hypertable "%"', relation USING ERRCODE = '42710';
    END IF;
    job := cron.schedule('timescaledb_retention_' || parts[1] || '.' || parts[2], _timescaledb_catalog.cron_of(schedule_interval), format('SELECT public.drop_chunks(%L, %L::interval)', parts[1] || '.' || parts[2], drop_after));
    INSERT INTO _timescaledb_catalog.policy VALUES (job, 'policy_retention', parts[1], parts[2], drop_after, schedule_interval);
    RETURN job;
END
$$;
CREATE FUNCTION public.remove_retention_policy(relation text, if_exists boolean DEFAULT false) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    parts text[] := _timescaledb_catalog.split(relation);
    job bigint;
BEGIN
    DELETE FROM _timescaledb_catalog.policy p WHERE p.proc_name = 'policy_retention' AND p.schema_name = parts[1] AND p.table_name = parts[2] RETURNING p.job_id INTO job;
    IF job IS NULL THEN
        IF NOT if_exists THEN
            RAISE EXCEPTION 'retention policy not found for hypertable "%"', relation USING ERRCODE = '42704';
        END IF;
        RETURN;
    END IF;
    PERFORM cron.unschedule(job);
END
$$;
CREATE FUNCTION public.add_continuous_aggregate_policy(continuous_aggregate text, start_offset interval, end_offset interval, schedule_interval interval, if_not_exists boolean DEFAULT false) RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
    parts text[] := _timescaledb_catalog.split(continuous_aggregate);
    job bigint;
BEGIN
    SELECT p.job_id INTO job FROM _timescaledb_catalog.policy p WHERE p.proc_name = 'policy_refresh_continuous_aggregate' AND p.schema_name = parts[1] AND p.table_name = parts[2];
    IF job IS NOT NULL THEN
        IF if_not_exists THEN
            RETURN -1;
        END IF;
        RAISE EXCEPTION 'continuous aggregate policy already exists for "%"', continuous_aggregate USING ERRCODE = '42710';
    END IF;
    job := cron.schedule('timescaledb_refresh_' || parts[1] || '.' || parts[2], _timescaledb_catalog.cron_of(schedule_interval), format('REFRESH MATERIALIZED VIEW %I.%I', parts[1], parts[2]));
    INSERT INTO _timescaledb_catalog.policy VALUES (job, 'policy_refresh_continuous_aggregate', parts[1], parts[2], NULL, schedule_interval);
    RETURN job;
END
$$;
CREATE FUNCTION public.remove_continuous_aggregate_policy(continuous_aggregate text, if_exists boolean DEFAULT false) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    parts text[] := _timescaledb_catalog.split(continuous_aggregate);
    job bigint;
BEGIN
    DELETE FROM _timescaledb_catalog.policy p WHERE p.proc_name = 'policy_refresh_continuous_aggregate' AND p.schema_name = parts[1] AND p.table_name = parts[2] RETURNING p.job_id INTO job;
    IF job IS NULL THEN
        IF NOT if_exists THEN
            RAISE EXCEPTION 'continuous aggregate policy not found for "%"', continuous_aggregate USING ERRCODE = '42704';
        END IF;
        RETURN;
    END IF;
    PERFORM cron.unschedule(job);
END
$$;
CREATE VIEW timescaledb_information.hypertables AS SELECT h.schema_name AS hypertable_schema, h.table_name AS hypertable_name, current_user AS owner, 1 AS num_dimensions, 0 AS num_chunks, false AS compression_enabled FROM _timescaledb_catalog.hypertable h;
CREATE VIEW timescaledb_information.dimensions AS SELECT h.schema_name AS hypertable_schema, h.table_name AS hypertable_name, 1 AS dimension_number, h.time_column AS column_name, 'Time' AS dimension_type, h.chunk_time_interval AS time_interval FROM _timescaledb_catalog.hypertable h;
CREATE VIEW timescaledb_information.jobs AS SELECT p.job_id, CASE p.proc_name WHEN 'policy_retention' THEN 'Retention Policy' ELSE 'Refresh Continuous Aggregate Policy' END || ' [' || p.job_id || ']' AS application_name, p.schedule_interval, '_timescaledb_functions' AS proc_schema, p.proc_name, p.schema_name AS hypertable_schema, p.table_name AS hypertable_name, coalesce(j.active, false) AS scheduled FROM _timescaledb_catalog.policy p LEFT JOIN cron.job j ON j.jobid = p.job_id;
"#;
