/// The statements `CREATE EXTENSION eggdb_pipelines` runs: SQL models built in dependency order
/// from `{{ ref('model') }}`, with tests and lineage, and task pipelines with dependencies,
/// retries with exponential backoff and backfills, advanced by the pg_cron scheduler (D200).
pub(crate) const PIPELINES_SCRIPT: &str = r#"
CREATE EXTENSION IF NOT EXISTS pg_cron;
CREATE SCHEMA eggdb_pipelines;
CREATE TABLE eggdb_pipelines.models (model_name text PRIMARY KEY, schema_name text NOT NULL DEFAULT 'public', body text NOT NULL, materialized text NOT NULL DEFAULT 'view' CHECK (materialized IN ('view', 'table')), tests jsonb NOT NULL DEFAULT '[]');
CREATE TABLE eggdb_pipelines.model_runs (model_name text NOT NULL, built_at timestamptz NOT NULL DEFAULT now(), status text NOT NULL, row_count bigint, message text);
CREATE TABLE eggdb_pipelines.test_results (model_name text NOT NULL, test text NOT NULL, checked_at timestamptz NOT NULL DEFAULT now(), failures bigint NOT NULL);
CREATE TABLE eggdb_pipelines.lineage (source_asset text NOT NULL, target_asset text NOT NULL, PRIMARY KEY (source_asset, target_asset));
CREATE FUNCTION eggdb_pipelines.references(body text) RETURNS text[] LANGUAGE plpgsql AS $$
DECLARE
    found text[];
    names text[] := '{}';
BEGIN
    FOR found IN SELECT regexp_matches(body, '\{\{\s*ref\(\s*''([^'']+)''\s*\)\s*\}\}', 'g') LOOP
        IF NOT found[1] = ANY(names) THEN
            names := names || found[1];
        END IF;
    END LOOP;
    RETURN names;
END
$$;
CREATE FUNCTION eggdb_pipelines.compile(wanted text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    compiled text;
    referenced text;
    target_schema text;
BEGIN
    SELECT m.body INTO compiled FROM eggdb_pipelines.models m WHERE m.model_name = wanted;
    IF compiled IS NULL THEN
        RAISE EXCEPTION 'no model is named %', wanted USING ERRCODE = '42704';
    END IF;
    FOREACH referenced IN ARRAY eggdb_pipelines.references(compiled) LOOP
        SELECT m.schema_name INTO target_schema FROM eggdb_pipelines.models m WHERE m.model_name = referenced;
        IF target_schema IS NULL THEN
            RAISE EXCEPTION 'model % refers to the unknown model %', wanted, referenced USING ERRCODE = '42704';
        END IF;
        compiled := regexp_replace(compiled, '\{\{\s*ref\(\s*''' || referenced || '''\s*\)\s*\}\}', format('%I.%I', target_schema, referenced), 'g');
    END LOOP;
    compiled := regexp_replace(compiled, '\{\{\s*source\(\s*''([^'']+)''\s*,\s*''([^'']+)''\s*\)\s*\}\}', '\1.\2', 'g');
    RETURN compiled;
END
$$;
CREATE FUNCTION eggdb_pipelines.run_models() RETURNS TABLE (model_name text, status text, row_count bigint) LANGUAGE plpgsql AS $$
DECLARE
    built text[] := '{}';
    pending text[];
    progressed boolean;
    candidate record;
    referenced text;
    ready boolean;
    compiled text;
    counted bigint;
    test jsonb;
    failures bigint;
BEGIN
    SELECT coalesce(array_agg(m.model_name ORDER BY m.model_name), '{}') INTO pending FROM eggdb_pipelines.models m;
    LOOP
        progressed := false;
        FOR candidate IN SELECT m.* FROM eggdb_pipelines.models m WHERE m.model_name = ANY(pending) ORDER BY m.model_name LOOP
            ready := true;
            FOREACH referenced IN ARRAY eggdb_pipelines.references(candidate.body) LOOP
                IF NOT referenced = ANY(built) THEN
                    ready := false;
                END IF;
            END LOOP;
            CONTINUE WHEN NOT ready;
            compiled := eggdb_pipelines.compile(candidate.model_name);
            IF candidate.materialized = 'view' THEN
                EXECUTE format('CREATE OR REPLACE VIEW %I.%I AS %s', candidate.schema_name, candidate.model_name, compiled);
                counted := NULL;
            ELSE
                EXECUTE format('DROP TABLE IF EXISTS %I.%I', candidate.schema_name, candidate.model_name);
                EXECUTE format('CREATE TABLE %I.%I AS %s', candidate.schema_name, candidate.model_name, compiled);
                GET DIAGNOSTICS counted = ROW_COUNT;
            END IF;
            DELETE FROM eggdb_pipelines.lineage l WHERE l.target_asset = candidate.schema_name || '.' || candidate.model_name;
            INSERT INTO eggdb_pipelines.lineage (source_asset, target_asset)
                SELECT DISTINCT m.schema_name || '.' || m.model_name, candidate.schema_name || '.' || candidate.model_name
                FROM eggdb_pipelines.models m WHERE m.model_name = ANY(eggdb_pipelines.references(candidate.body));
            IF EXISTS (SELECT 1 FROM information_schema.schemata s WHERE s.schema_name = 'eggdb_catalog') THEN
                EXECUTE 'INSERT INTO eggdb_catalog.lineage (source_asset, target_asset, job_name) SELECT l.source_asset, l.target_asset, ''eggdb_pipelines'' FROM eggdb_pipelines.lineage l WHERE l.target_asset = $1 ON CONFLICT DO NOTHING'
                    USING candidate.schema_name || '.' || candidate.model_name;
            END IF;
            FOR test IN SELECT * FROM jsonb_array_elements(candidate.tests) LOOP
                IF test ->> 'test' = 'not_null' THEN
                    EXECUTE format('SELECT count(*) FROM %I.%I WHERE %I IS NULL', candidate.schema_name, candidate.model_name, test ->> 'column') INTO failures;
                ELSIF test ->> 'test' = 'unique' THEN
                    EXECUTE format('SELECT count(*) FROM (SELECT %I FROM %I.%I WHERE %I IS NOT NULL GROUP BY 1 HAVING count(*) > 1) d', test ->> 'column', candidate.schema_name, candidate.model_name, test ->> 'column') INTO failures;
                ELSIF test ->> 'test' = 'accepted_values' THEN
                    EXECUTE format('SELECT count(*) FROM %I.%I WHERE %I IS NOT NULL AND NOT (%I::text = ANY($1))', candidate.schema_name, candidate.model_name, test ->> 'column', test ->> 'column')
                        INTO failures USING ARRAY(SELECT jsonb_array_elements_text(test -> 'values'));
                ELSE
                    RAISE EXCEPTION 'model % has an unknown test %', candidate.model_name, test ->> 'test' USING ERRCODE = '22023';
                END IF;
                INSERT INTO eggdb_pipelines.test_results (model_name, test, failures) VALUES (candidate.model_name, (test ->> 'test') || ':' || (test ->> 'column'), failures);
            END LOOP;
            INSERT INTO eggdb_pipelines.model_runs (model_name, status, row_count) VALUES (candidate.model_name, 'built', counted);
            built := built || candidate.model_name;
            pending := array_remove(pending, candidate.model_name);
            progressed := true;
            model_name := candidate.model_name;
            status := 'built';
            row_count := counted;
            RETURN NEXT;
        END LOOP;
        EXIT WHEN coalesce(array_length(pending, 1), 0) = 0;
        IF NOT progressed THEN
            RAISE EXCEPTION 'the models % refer to each other in a cycle or to models that cannot be built', pending USING ERRCODE = '42P17';
        END IF;
    END LOOP;
END
$$;
CREATE TABLE eggdb_pipelines.tasks (pipeline text NOT NULL, task_name text NOT NULL, command text NOT NULL, depends_on text[] NOT NULL DEFAULT '{}', max_retries integer NOT NULL DEFAULT 3 CHECK (max_retries >= 0), retry_delay interval NOT NULL DEFAULT interval '1 minute', PRIMARY KEY (pipeline, task_name));
CREATE TABLE eggdb_pipelines.runs (run_id bigserial PRIMARY KEY, pipeline text NOT NULL, logical_date timestamptz NOT NULL, state text NOT NULL DEFAULT 'running', started_at timestamptz NOT NULL DEFAULT now(), finished_at timestamptz);
CREATE TABLE eggdb_pipelines.task_runs (run_id bigint NOT NULL REFERENCES eggdb_pipelines.runs (run_id) ON DELETE CASCADE, task_name text NOT NULL, state text NOT NULL DEFAULT 'pending', attempts integer NOT NULL DEFAULT 0, next_attempt_at timestamptz NOT NULL DEFAULT now(), last_error text, finished_at timestamptz, PRIMARY KEY (run_id, task_name));
CREATE FUNCTION eggdb_pipelines.start_run(pipeline text, logical_date timestamptz DEFAULT now()) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    made bigint;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM eggdb_pipelines.tasks t WHERE t.pipeline = start_run.pipeline) THEN
        RAISE EXCEPTION 'pipeline % has no tasks', pipeline USING ERRCODE = '42704';
    END IF;
    INSERT INTO eggdb_pipelines.runs (pipeline, logical_date) VALUES (start_run.pipeline, start_run.logical_date) RETURNING run_id INTO made;
    INSERT INTO eggdb_pipelines.task_runs (run_id, task_name) SELECT made, t.task_name FROM eggdb_pipelines.tasks t WHERE t.pipeline = start_run.pipeline;
    RETURN made;
END
$$;
CREATE FUNCTION eggdb_pipelines.advance(wanted_run bigint) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    run record;
    due record;
    command text;
    failure text;
    left_open bigint;
    any_failed boolean;
BEGIN
    SELECT * INTO run FROM eggdb_pipelines.runs r WHERE r.run_id = wanted_run;
    IF run IS NULL THEN
        RAISE EXCEPTION 'no run %', wanted_run USING ERRCODE = '42704';
    END IF;
    IF run.state <> 'running' THEN
        RETURN run.state;
    END IF;
    UPDATE eggdb_pipelines.task_runs tr SET state = 'upstream_failed', finished_at = now()
        FROM eggdb_pipelines.tasks t
        WHERE tr.run_id = wanted_run AND t.pipeline = run.pipeline AND t.task_name = tr.task_name AND tr.state IN ('pending', 'retrying')
          AND EXISTS (SELECT 1 FROM eggdb_pipelines.task_runs up WHERE up.run_id = wanted_run AND up.task_name = ANY(t.depends_on) AND up.state IN ('failed', 'upstream_failed'));
    FOR due IN
        SELECT tr.task_name, tr.attempts, t.command, t.max_retries, t.retry_delay FROM eggdb_pipelines.task_runs tr JOIN eggdb_pipelines.tasks t ON t.pipeline = run.pipeline AND t.task_name = tr.task_name
        WHERE tr.run_id = wanted_run AND tr.state IN ('pending', 'retrying') AND tr.next_attempt_at <= now()
          AND NOT EXISTS (SELECT 1 FROM eggdb_pipelines.task_runs up WHERE up.run_id = wanted_run AND up.task_name = ANY(t.depends_on) AND up.state <> 'success')
        ORDER BY tr.task_name
    LOOP
        command := replace(due.command, '{{ logical_date }}', quote_literal(run.logical_date::text));
        BEGIN
            EXECUTE command;
            UPDATE eggdb_pipelines.task_runs SET state = 'success', attempts = attempts + 1, last_error = NULL, finished_at = now() WHERE run_id = wanted_run AND task_name = due.task_name;
        EXCEPTION WHEN OTHERS THEN
            GET STACKED DIAGNOSTICS failure = MESSAGE_TEXT;
            IF due.attempts + 1 > due.max_retries THEN
                UPDATE eggdb_pipelines.task_runs SET state = 'failed', attempts = attempts + 1, last_error = failure, finished_at = now() WHERE run_id = wanted_run AND task_name = due.task_name;
            ELSE
                UPDATE eggdb_pipelines.task_runs SET state = 'retrying', attempts = attempts + 1, last_error = failure,
                    next_attempt_at = now() + due.retry_delay * power(2, due.attempts)
                    WHERE run_id = wanted_run AND task_name = due.task_name;
            END IF;
        END;
    END LOOP;
    SELECT count(*) INTO left_open FROM eggdb_pipelines.task_runs tr WHERE tr.run_id = wanted_run AND tr.state IN ('pending', 'retrying');
    SELECT EXISTS (SELECT 1 FROM eggdb_pipelines.task_runs tr WHERE tr.run_id = wanted_run AND tr.state IN ('failed', 'upstream_failed')) INTO any_failed;
    IF left_open = 0 THEN
        UPDATE eggdb_pipelines.runs SET state = CASE WHEN any_failed THEN 'failed' ELSE 'success' END, finished_at = now() WHERE run_id = wanted_run;
        RETURN CASE WHEN any_failed THEN 'failed' ELSE 'success' END;
    END IF;
    RETURN 'running';
END
$$;
CREATE FUNCTION eggdb_pipelines.tick() RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    open_run bigint;
    advanced bigint := 0;
BEGIN
    FOR open_run IN SELECT r.run_id FROM eggdb_pipelines.runs r WHERE r.state = 'running' ORDER BY r.run_id LOOP
        PERFORM eggdb_pipelines.advance(open_run);
        advanced := advanced + 1;
    END LOOP;
    RETURN advanced;
END
$$;
CREATE FUNCTION eggdb_pipelines.backfill(pipeline text, from_date timestamptz, to_date timestamptz, step interval DEFAULT interval '1 day') RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    at_date timestamptz := from_date;
    started bigint := 0;
BEGIN
    IF step <= interval '0' THEN
        RAISE EXCEPTION 'the backfill step must be positive' USING ERRCODE = '22023';
    END IF;
    WHILE at_date <= to_date LOOP
        PERFORM eggdb_pipelines.start_run(pipeline, at_date);
        started := started + 1;
        IF started > 10000 THEN
            RAISE EXCEPTION 'a backfill starts at most 10000 runs' USING ERRCODE = '54000';
        END IF;
        at_date := at_date + step;
    END LOOP;
    RETURN started;
END
$$;
"#;
