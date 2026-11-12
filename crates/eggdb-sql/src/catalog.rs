pub(crate) const CATALOG_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_catalog;
CREATE TABLE eggdb_catalog.terms (term text PRIMARY KEY, definition text NOT NULL, steward text);
CREATE TABLE eggdb_catalog.tags (schema_name text NOT NULL, table_name text NOT NULL, column_name text NOT NULL DEFAULT '', term text NOT NULL REFERENCES eggdb_catalog.terms (term), PRIMARY KEY (schema_name, table_name, column_name, term));
CREATE TABLE eggdb_catalog.quality_rules (rule_name text PRIMARY KEY, schema_name text NOT NULL, table_name text NOT NULL, row_condition text NOT NULL, description text);
CREATE TABLE eggdb_catalog.quality_results (rule_name text NOT NULL, checked_at timestamptz NOT NULL DEFAULT now(), checked_rows bigint NOT NULL, failed_rows bigint NOT NULL);
CREATE TABLE eggdb_catalog.lineage (source_asset text NOT NULL, target_asset text NOT NULL, job_name text NOT NULL DEFAULT '', PRIMARY KEY (source_asset, target_asset, job_name));
CREATE TABLE eggdb_catalog.column_lineage (source_asset text NOT NULL, source_column text NOT NULL, target_asset text NOT NULL, target_column text NOT NULL, job_name text NOT NULL DEFAULT '', PRIMARY KEY (source_asset, source_column, target_asset, target_column, job_name));
CREATE FUNCTION eggdb_catalog.capture_lineage(job text, statement text) RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
    found jsonb := eggdb_column_lineage(statement);
    target text := found ->> 'target';
    entry record;
    source text;
    captured integer := 0;
BEGIN
    FOR entry IN SELECT c.value AS body FROM jsonb_array_elements(found -> 'columns') c LOOP
        FOR source IN SELECT s.value FROM jsonb_array_elements_text(entry.body -> 'sources') s LOOP
            INSERT INTO eggdb_catalog.lineage VALUES (regexp_replace(source, '\.[^.]*$', ''), target, job) ON CONFLICT DO NOTHING;
            INSERT INTO eggdb_catalog.column_lineage VALUES (regexp_replace(source, '\.[^.]*$', ''), regexp_replace(source, '^.*\.', ''), target, entry.body ->> 'column', job) ON CONFLICT DO NOTHING;
            captured := captured + 1;
        END LOOP;
    END LOOP;
    RETURN captured;
END
$$;
CREATE FUNCTION eggdb_catalog.check_quality() RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    rule record;
    total bigint;
    failed bigint;
    broken bigint := 0;
BEGIN
    FOR rule IN SELECT * FROM eggdb_catalog.quality_rules ORDER BY rule_name LOOP
        EXECUTE format('SELECT count(*), coalesce(sum(CASE WHEN coalesce((%s), false) THEN 0 ELSE 1 END), 0) FROM %I.%I', rule.row_condition, rule.schema_name, rule.table_name) INTO total, failed;
        INSERT INTO eggdb_catalog.quality_results (rule_name, checked_rows, failed_rows) VALUES (rule.rule_name, total, failed);
        IF failed > 0 THEN
            broken := broken + 1;
        END IF;
    END LOOP;
    RETURN broken;
END
$$;
CREATE FUNCTION eggdb_catalog.upstream(asset text) RETURNS TABLE (upstream_asset text, hops integer) LANGUAGE plpgsql AS $$
BEGIN
    RETURN QUERY WITH RECURSIVE walked (reached, distance) AS (
        SELECT l.source_asset, 1 FROM eggdb_catalog.lineage l WHERE l.target_asset = asset
        UNION
        SELECT l.source_asset, w.distance + 1 FROM eggdb_catalog.lineage l JOIN walked w ON l.target_asset = w.reached WHERE w.distance < 100
    ) SELECT reached, min(distance) FROM walked GROUP BY reached ORDER BY 2, 1;
END
$$;
CREATE FUNCTION eggdb_catalog.column_lineage_facet(job text, asset text) RETURNS jsonb LANGUAGE sql AS $$
    SELECT CASE WHEN count(*) = 0 THEN '{}'::jsonb ELSE jsonb_build_object('columnLineage', jsonb_build_object(
        '_producer', 'https://github.com/eggspot/EggDB',
        '_schemaURL', 'https://openlineage.io/spec/facets/1-0-1/ColumnLineageDatasetFacet.json',
        'fields', jsonb_object_agg(f.target_column, f.inputs))) END
    FROM (
        SELECT l.target_column, jsonb_build_object('inputFields', jsonb_agg(jsonb_build_object('namespace', 'eggdb', 'name', l.source_asset, 'field', l.source_column) ORDER BY l.source_asset, l.source_column)) AS inputs
        FROM eggdb_catalog.column_lineage l WHERE l.job_name = job AND l.target_asset = asset GROUP BY l.target_column
    ) f
$$;
CREATE FUNCTION eggdb_catalog.openlineage_event(job text, event_type text) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE
    inputs jsonb;
    outputs jsonb;
BEGIN
    IF event_type NOT IN ('START', 'RUNNING', 'COMPLETE', 'ABORT', 'FAIL', 'OTHER') THEN
        RAISE EXCEPTION 'OpenLineage event type % is not START, RUNNING, COMPLETE, ABORT, FAIL or OTHER', event_type USING ERRCODE = '22023';
    END IF;
    SELECT coalesce(jsonb_agg(jsonb_build_object('namespace', 'eggdb', 'name', a) ORDER BY a), '[]'::jsonb) INTO inputs
        FROM (SELECT DISTINCT source_asset AS a FROM eggdb_catalog.lineage WHERE job_name = job) s;
    SELECT coalesce(jsonb_agg(jsonb_build_object('namespace', 'eggdb', 'name', a, 'facets', eggdb_catalog.column_lineage_facet(job, a)) ORDER BY a), '[]'::jsonb) INTO outputs
        FROM (SELECT DISTINCT target_asset AS a FROM eggdb_catalog.lineage WHERE job_name = job) t;
    RETURN jsonb_build_object(
        'eventType', event_type,
        'eventTime', to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
        'producer', 'https://github.com/eggspot/EggDB',
        'schemaURL', 'https://openlineage.io/spec/2-0-2/OpenLineage.json#/$defs/RunEvent',
        'run', jsonb_build_object('runId', gen_random_uuid()::text),
        'job', jsonb_build_object('namespace', 'eggdb', 'name', job),
        'inputs', inputs,
        'outputs', outputs);
END
$$;
CREATE TABLE eggdb_catalog.profiles (schema_name text NOT NULL, table_name text NOT NULL, column_name text NOT NULL, profiled_at timestamptz NOT NULL DEFAULT now(), row_count bigint NOT NULL, null_count bigint NOT NULL, distinct_count bigint NOT NULL, min_value text, max_value text, average_length double precision, top_values jsonb NOT NULL, PRIMARY KEY (schema_name, table_name, column_name));
CREATE FUNCTION eggdb_catalog.profile(schema_name text, table_name text, top_n integer DEFAULT 5) RETURNS TABLE (column_name text, row_count bigint, null_count bigint, distinct_count bigint, min_value text, max_value text) LANGUAGE plpgsql AS $$
DECLARE
    found_column record;
    counted bigint;
    nulls bigint;
    distinct_values bigint;
    lowest text;
    highest text;
    average double precision;
    top jsonb;
BEGIN
    FOR found_column IN SELECT c.column_name AS name FROM information_schema.columns c WHERE c.table_schema = profile.schema_name AND c.table_name = profile.table_name ORDER BY c.ordinal_position LOOP
        EXECUTE format('SELECT count(*), count(*) - count(%1$I), count(DISTINCT %1$I::text), avg(length(%1$I::text)) FROM %2$I.%3$I', found_column.name, profile.schema_name, profile.table_name)
            INTO counted, nulls, distinct_values, average;
        BEGIN
            EXECUTE format('SELECT min(%1$I)::text, max(%1$I)::text FROM %2$I.%3$I', found_column.name, profile.schema_name, profile.table_name) INTO lowest, highest;
        EXCEPTION WHEN OTHERS THEN
            EXECUTE format('SELECT min(%1$I::text), max(%1$I::text) FROM %2$I.%3$I', found_column.name, profile.schema_name, profile.table_name) INTO lowest, highest;
        END;
        EXECUTE format('SELECT coalesce(jsonb_agg(jsonb_build_object(''value'', v, ''count'', n) ORDER BY n DESC, v), ''[]''::jsonb) FROM (SELECT %1$I::text AS v, count(*) AS n FROM %2$I.%3$I WHERE %1$I IS NOT NULL GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT %4$s) t', found_column.name, profile.schema_name, profile.table_name, greatest(top_n, 0))
            INTO top;
        INSERT INTO eggdb_catalog.profiles (schema_name, table_name, column_name, row_count, null_count, distinct_count, min_value, max_value, average_length, top_values)
            VALUES (profile.schema_name, profile.table_name, found_column.name, counted, nulls, distinct_values, lowest, highest, average, top)
            ON CONFLICT (schema_name, table_name, column_name) DO UPDATE SET profiled_at = now(), row_count = EXCLUDED.row_count, null_count = EXCLUDED.null_count, distinct_count = EXCLUDED.distinct_count,
                min_value = EXCLUDED.min_value, max_value = EXCLUDED.max_value, average_length = EXCLUDED.average_length, top_values = EXCLUDED.top_values;
        column_name := found_column.name;
        row_count := counted;
        null_count := nulls;
        distinct_count := distinct_values;
        min_value := lowest;
        max_value := highest;
        RETURN NEXT;
    END LOOP;
END
$$;
CREATE TABLE eggdb_catalog.match_rules (entity text NOT NULL, column_name text NOT NULL, method text NOT NULL CHECK (method IN ('exact', 'levenshtein', 'similarity', 'soundex')), weight double precision NOT NULL DEFAULT 1 CHECK (weight > 0), PRIMARY KEY (entity, column_name));
CREATE TABLE eggdb_catalog.entity_members (entity text NOT NULL, record_key text NOT NULL, cluster_key text NOT NULL, PRIMARY KEY (entity, record_key));
CREATE FUNCTION eggdb_catalog.resolve_entities(entity text, schema_name text, table_name text, key_column text, threshold double precision DEFAULT 0.8) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    rule record;
    parts text[] := '{}';
    total_weight double precision := 0;
    changed bigint;
    clusters bigint;
BEGIN
    FOR rule IN SELECT r.column_name, r.method, r.weight FROM eggdb_catalog.match_rules r WHERE r.entity = resolve_entities.entity ORDER BY r.column_name LOOP
        total_weight := total_weight + rule.weight;
        parts := parts || format('%s * coalesce(%s, 0)', rule.weight, CASE rule.method
            WHEN 'exact' THEN format('(a.%1$I = b.%1$I)::integer', rule.column_name)
            WHEN 'levenshtein' THEN format('1.0 - levenshtein(a.%1$I::text, b.%1$I::text)::double precision / greatest(length(a.%1$I::text), length(b.%1$I::text), 1)', rule.column_name)
            WHEN 'similarity' THEN format('similarity(a.%1$I::text, b.%1$I::text)', rule.column_name)
            ELSE format('(soundex(a.%1$I::text) = soundex(b.%1$I::text))::integer', rule.column_name)
        END);
    END LOOP;
    IF total_weight = 0 THEN
        RAISE EXCEPTION 'entity % has no match rules', entity USING ERRCODE = '42704';
    END IF;
    DELETE FROM eggdb_catalog.entity_members m WHERE m.entity = resolve_entities.entity;
    EXECUTE format('INSERT INTO eggdb_catalog.entity_members (entity, record_key, cluster_key) SELECT %L, %I::text, %I::text FROM %I.%I', entity, key_column, key_column, schema_name, table_name);
    CREATE TEMPORARY TABLE IF NOT EXISTS eggdb_entity_pairs (left_key text, right_key text);
    DELETE FROM eggdb_entity_pairs;
    EXECUTE format('INSERT INTO eggdb_entity_pairs SELECT a.%1$I::text, b.%1$I::text FROM %2$I.%3$I a JOIN %2$I.%3$I b ON a.%1$I::text < b.%1$I::text WHERE (%4$s) / %5$s >= %6$s',
        key_column, schema_name, table_name, array_to_string(parts, ' + '), total_weight, threshold);
    LOOP
        UPDATE eggdb_catalog.entity_members m SET cluster_key = linked.smallest
            FROM (SELECT p.right_key AS member, min(l.cluster_key) AS smallest FROM eggdb_entity_pairs p JOIN eggdb_catalog.entity_members l ON l.entity = resolve_entities.entity AND l.record_key = p.left_key GROUP BY p.right_key
                  UNION ALL
                  SELECT p.left_key, min(r.cluster_key) FROM eggdb_entity_pairs p JOIN eggdb_catalog.entity_members r ON r.entity = resolve_entities.entity AND r.record_key = p.right_key GROUP BY p.left_key) linked
            WHERE m.entity = resolve_entities.entity AND m.record_key = linked.member AND linked.smallest < m.cluster_key;
        GET DIAGNOSTICS changed = ROW_COUNT;
        EXIT WHEN changed = 0;
    END LOOP;
    SELECT count(DISTINCT m.cluster_key) INTO clusters FROM eggdb_catalog.entity_members m WHERE m.entity = resolve_entities.entity;
    RETURN clusters;
END
$$;
CREATE FUNCTION eggdb_catalog.golden_records(entity text, schema_name text, table_name text, key_column text, target_table text, survivorship jsonb DEFAULT '{}') RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    found_column record;
    chosen text[] := '{}';
    rule text;
    made bigint;
BEGIN
    FOR found_column IN SELECT c.column_name AS name FROM information_schema.columns c WHERE c.table_schema = golden_records.schema_name AND c.table_name = golden_records.table_name ORDER BY c.ordinal_position LOOP
        CONTINUE WHEN found_column.name = key_column;
        rule := coalesce(survivorship ->> found_column.name, 'first');
        chosen := chosen || CASE rule
            WHEN 'first' THEN format('(array_agg(t.%1$I ORDER BY t.%1$I IS NULL, t.%2$I::text))[1] AS %1$I', found_column.name, key_column)
            WHEN 'last' THEN format('(array_agg(t.%1$I ORDER BY t.%1$I IS NULL, t.%2$I::text DESC))[1] AS %1$I', found_column.name, key_column)
            WHEN 'longest' THEN format('(array_agg(t.%1$I ORDER BY length(t.%1$I::text) DESC NULLS LAST, t.%2$I::text))[1] AS %1$I', found_column.name, key_column)
            WHEN 'max' THEN format('max(t.%1$I) AS %1$I', found_column.name)
            WHEN 'min' THEN format('min(t.%1$I) AS %1$I', found_column.name)
            ELSE NULL
        END;
        IF chosen[array_length(chosen, 1)] IS NULL THEN
            RAISE EXCEPTION 'unknown survivorship rule % for column %', rule, found_column.name USING ERRCODE = '22023';
        END IF;
    END LOOP;
    EXECUTE format('CREATE TABLE %s AS SELECT m.cluster_key AS golden_key, count(*) AS source_records%s FROM %I.%I t JOIN eggdb_catalog.entity_members m ON m.entity = %L AND m.record_key = t.%I::text GROUP BY m.cluster_key',
        target_table, CASE WHEN coalesce(array_length(chosen, 1), 0) = 0 THEN '' ELSE ', ' || array_to_string(chosen, ', ') END, schema_name, table_name, entity, key_column);
    GET DIAGNOSTICS made = ROW_COUNT;
    RETURN made;
END
$$;
CREATE TABLE eggdb_catalog.schema_subjects (subject text PRIMARY KEY, compatibility text NOT NULL DEFAULT 'BACKWARD' CHECK (compatibility IN ('NONE', 'BACKWARD', 'BACKWARD_TRANSITIVE', 'FORWARD', 'FORWARD_TRANSITIVE', 'FULL', 'FULL_TRANSITIVE')));
CREATE TABLE eggdb_catalog.schema_versions (subject text NOT NULL REFERENCES eggdb_catalog.schema_subjects (subject), version integer NOT NULL, definition jsonb NOT NULL, registered_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (subject, version));
CREATE FUNCTION eggdb_catalog.describe_table(schema_name text, table_name text) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE
    fields jsonb;
BEGIN
    SELECT jsonb_agg(jsonb_build_object('name', c.column_name, 'type', c.data_type, 'required', c.is_nullable = 'NO') ORDER BY c.ordinal_position) INTO fields
        FROM information_schema.columns c WHERE c.table_schema = describe_table.schema_name AND c.table_name = describe_table.table_name;
    IF fields IS NULL THEN
        RAISE EXCEPTION 'table %.% does not exist', schema_name, table_name USING ERRCODE = '42P01';
    END IF;
    RETURN jsonb_build_object('fields', fields);
END
$$;
CREATE FUNCTION eggdb_catalog.schema_differences(reader jsonb, writer jsonb) RETURNS TABLE (field text, problem text) LANGUAGE plpgsql AS $$
DECLARE
    wanted_row record;
    wanted jsonb;
    given jsonb;
    required boolean;
BEGIN
    FOR wanted_row IN SELECT f.value AS definition FROM jsonb_array_elements(coalesce(reader -> 'fields', '[]'::jsonb)) f LOOP
        wanted := wanted_row.definition;
        required := coalesce((wanted ->> 'required')::boolean, false) AND (wanted -> 'default') IS NULL;
        given := NULL;
        SELECT w.value INTO given FROM jsonb_array_elements(coalesce(writer -> 'fields', '[]'::jsonb)) w WHERE w.value ->> 'name' = wanted ->> 'name';
        field := wanted ->> 'name';
        IF given IS NULL THEN
            IF required THEN
                problem := 'required field missing';
                RETURN NEXT;
            END IF;
        ELSIF (given ->> 'type') IS DISTINCT FROM (wanted ->> 'type') THEN
            problem := format('type %s, expected %s', given ->> 'type', wanted ->> 'type');
            RETURN NEXT;
        ELSIF required AND NOT coalesce((given ->> 'required')::boolean, false) THEN
            problem := 'may be null, expected required';
            RETURN NEXT;
        END IF;
    END LOOP;
END
$$;
CREATE FUNCTION eggdb_catalog.register_schema(subject text, definition jsonb) RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
    mode text;
    found_version integer;
    previous record;
    issues text;
BEGIN
    IF jsonb_typeof(register_schema.definition -> 'fields') IS DISTINCT FROM 'array' THEN
        RAISE EXCEPTION 'a schema needs a fields array' USING ERRCODE = '22023';
    END IF;
    INSERT INTO eggdb_catalog.schema_subjects (subject) VALUES (register_schema.subject) ON CONFLICT (subject) DO NOTHING;
    SELECT s.compatibility INTO mode FROM eggdb_catalog.schema_subjects s WHERE s.subject = register_schema.subject;
    SELECT v.version INTO found_version FROM eggdb_catalog.schema_versions v WHERE v.subject = register_schema.subject AND v.definition = register_schema.definition;
    IF found_version IS NOT NULL THEN
        RETURN found_version;
    END IF;
    FOR previous IN SELECT v.version, v.definition FROM eggdb_catalog.schema_versions v
            WHERE v.subject = register_schema.subject
              AND (mode LIKE '%TRANSITIVE' OR v.version = (SELECT max(m.version) FROM eggdb_catalog.schema_versions m WHERE m.subject = register_schema.subject))
            ORDER BY v.version LOOP
        IF mode LIKE 'BACKWARD%' OR mode LIKE 'FULL%' THEN
            SELECT string_agg(d.field || ': ' || d.problem, '; ') INTO issues FROM eggdb_catalog.schema_differences(register_schema.definition, previous.definition) d;
            IF issues IS NOT NULL THEN
                RAISE EXCEPTION 'the schema cannot read version % of %: %', previous.version, subject, issues USING ERRCODE = '42804';
            END IF;
        END IF;
        IF mode LIKE 'FORWARD%' OR mode LIKE 'FULL%' THEN
            SELECT string_agg(d.field || ': ' || d.problem, '; ') INTO issues FROM eggdb_catalog.schema_differences(previous.definition, register_schema.definition) d;
            IF issues IS NOT NULL THEN
                RAISE EXCEPTION 'version % of % cannot read the schema: %', previous.version, subject, issues USING ERRCODE = '42804';
            END IF;
        END IF;
    END LOOP;
    SELECT coalesce(max(v.version), 0) + 1 INTO found_version FROM eggdb_catalog.schema_versions v WHERE v.subject = register_schema.subject;
    INSERT INTO eggdb_catalog.schema_versions (subject, version, definition) VALUES (register_schema.subject, found_version, register_schema.definition);
    RETURN found_version;
END
$$;
CREATE TABLE eggdb_catalog.contracts (contract_name text PRIMARY KEY, schema_name text NOT NULL, table_name text NOT NULL, subject text REFERENCES eggdb_catalog.schema_subjects (subject), owner text, freshness_column text, freshness_limit interval, minimum_rows bigint CHECK (minimum_rows >= 0));
CREATE TABLE eggdb_catalog.contract_results (contract_name text NOT NULL, checked_at timestamptz NOT NULL DEFAULT now(), clause text NOT NULL, passed boolean NOT NULL, detail text);
CREATE FUNCTION eggdb_catalog.check_contract(contract_name text) RETURNS TABLE (clause text, passed boolean, detail text) LANGUAGE plpgsql AS $$
DECLARE
    agreed record;
    promised jsonb;
    issues text;
    counted bigint;
    failed bigint;
    newest timestamptz;
    rule record;
BEGIN
    SELECT * INTO agreed FROM eggdb_catalog.contracts c WHERE c.contract_name = check_contract.contract_name;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'contract % does not exist', check_contract.contract_name USING ERRCODE = '42704';
    END IF;
    IF agreed.subject IS NOT NULL THEN
        SELECT v.definition INTO promised FROM eggdb_catalog.schema_versions v WHERE v.subject = agreed.subject ORDER BY v.version DESC LIMIT 1;
        SELECT string_agg(d.field || ': ' || d.problem, '; ') INTO issues
            FROM eggdb_catalog.schema_differences(promised, eggdb_catalog.describe_table(agreed.schema_name, agreed.table_name)) d;
        clause := 'schema';
        passed := promised IS NOT NULL AND issues IS NULL;
        detail := CASE WHEN promised IS NULL THEN 'the subject has no schema' ELSE issues END;
        INSERT INTO eggdb_catalog.contract_results (contract_name, clause, passed, detail) VALUES (check_contract.contract_name, clause, passed, detail);
        RETURN NEXT;
    END IF;
    IF agreed.freshness_column IS NOT NULL THEN
        EXECUTE format('SELECT max(%I)::timestamptz FROM %I.%I', agreed.freshness_column, agreed.schema_name, agreed.table_name) INTO newest;
        clause := 'freshness';
        passed := newest IS NOT NULL AND newest >= now() - coalesce(agreed.freshness_limit, interval '1 day');
        detail := coalesce('newest row at ' || newest::text, 'no rows');
        INSERT INTO eggdb_catalog.contract_results (contract_name, clause, passed, detail) VALUES (check_contract.contract_name, clause, passed, detail);
        RETURN NEXT;
    END IF;
    IF agreed.minimum_rows IS NOT NULL THEN
        EXECUTE format('SELECT count(*) FROM %I.%I', agreed.schema_name, agreed.table_name) INTO counted;
        clause := 'volume';
        passed := counted >= agreed.minimum_rows;
        detail := format('%s rows, at least %s promised', counted, agreed.minimum_rows);
        INSERT INTO eggdb_catalog.contract_results (contract_name, clause, passed, detail) VALUES (check_contract.contract_name, clause, passed, detail);
        RETURN NEXT;
    END IF;
    FOR rule IN SELECT q.rule_name, q.row_condition FROM eggdb_catalog.quality_rules q WHERE q.schema_name = agreed.schema_name AND q.table_name = agreed.table_name ORDER BY q.rule_name LOOP
        EXECUTE format('SELECT coalesce(sum(CASE WHEN coalesce((%s), false) THEN 0 ELSE 1 END), 0) FROM %I.%I', rule.row_condition, agreed.schema_name, agreed.table_name) INTO failed;
        clause := 'quality: ' || rule.rule_name;
        passed := failed = 0;
        detail := format('%s rows break it', failed);
        INSERT INTO eggdb_catalog.contract_results (contract_name, clause, passed, detail) VALUES (check_contract.contract_name, clause, passed, detail);
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

    fn catalog() -> MemoryStore {
        crate::memory_store::use_seeded_random_bytes();
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_catalog",
            "CREATE TABLE customers (id integer PRIMARY KEY, email text, country text)",
            "INSERT INTO customers VALUES (1, 'a@x.io', 'VN'), (2, NULL, 'SE'), (3, 'c@x.io', 'XX')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        drop(session);
        store
    }

    #[test]
    fn quality_rules_count_the_rows_that_break_them() {
        let store = catalog();
        let mut session = Session::new(&store);
        {
            let sql = "INSERT INTO eggdb_catalog.quality_rules (rule_name, schema_name, table_name, row_condition) VALUES ('email present', 'public', 'customers', 'email IS NOT NULL'), ('known country', 'public', 'customers', 'country IN (''VN'', ''SE'')'), ('ids positive', 'public', 'customers', 'id > 0')";
            run(&mut session, sql).unwrap();
        }
        assert_eq!(
            run(
                &mut session,
                "SELECT eggdb_catalog.check_quality() AS broken"
            )
            .unwrap(),
            "broken\n2\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, "SELECT rule_name, checked_rows, failed_rows FROM eggdb_catalog.quality_results ORDER BY rule_name").unwrap(),
            "rule_name|checked_rows|failed_rows\nemail present|3|1\nids positive|3|0\nknown country|3|1\nSELECT 3"
        );
    }

    #[test]
    fn glossary_terms_tag_columns_and_lineage_walks_upstream() {
        let store = catalog();
        let mut session = Session::new(&store);
        for sql in [
            "INSERT INTO eggdb_catalog.terms VALUES ('PII', 'Personal data under GDPR', 'privacy-team')",
            "INSERT INTO eggdb_catalog.tags VALUES ('public', 'customers', 'email', 'PII')",
            "INSERT INTO eggdb_catalog.lineage VALUES ('crm.contacts', 'public.customers', 'load_crm'), ('public.customers', 'mart.revenue', 'build_mart'), ('public.orders', 'mart.revenue', 'build_mart')",
        ] {
            run(&mut session, sql).unwrap();
        }
        assert!(run(&mut session, "INSERT INTO eggdb_catalog.tags VALUES ('public', 'customers', 'country', 'missing')").is_err());
        assert_eq!(
            run(
                &mut session,
                "SELECT * FROM eggdb_catalog.upstream('mart.revenue')"
            )
            .unwrap(),
            "upstream_asset|hops\npublic.customers|1\npublic.orders|1\ncrm.contacts|2\nSELECT 3"
        );
        assert_eq!(
            run(&mut session, "SELECT e -> 'inputs' AS inputs, e -> 'job' ->> 'name' AS job, e ->> 'eventType' AS type FROM (SELECT eggdb_catalog.openlineage_event('build_mart', 'COMPLETE') AS e) AS event").unwrap(),
            "inputs|job|type\n[{\"name\": \"public.customers\", \"namespace\": \"eggdb\"}, {\"name\": \"public.orders\", \"namespace\": \"eggdb\"}]|build_mart|COMPLETE\nSELECT 1"
        );
        assert!(
            run(
                &mut session,
                "SELECT eggdb_catalog.openlineage_event('build_mart', 'DONE')"
            )
            .is_err()
        );
    }

    #[test]
    fn column_lineage_is_captured_from_statements() {
        let store = catalog();
        let mut session = Session::new(&store);
        assert_eq!(
            run(
                &mut session,
                "SELECT eggdb_catalog.capture_lineage('build_mart', 'INSERT INTO mart.revenue (customer, total) SELECT c.name, sum(o.amount) FROM customers c JOIN orders o ON o.customer_id = c.id GROUP BY c.name') AS captured"
            )
            .unwrap(),
            "captured\n2\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, "SELECT source_asset, source_column, target_column FROM eggdb_catalog.column_lineage ORDER BY 3").unwrap(),
            "source_asset|source_column|target_column\ncustomers|name|customer\norders|amount|total\nSELECT 2"
        );
        let facet = run(
            &mut session,
            "SELECT e -> 'outputs' -> 0 -> 'facets' -> 'columnLineage' -> 'fields' -> 'total' AS total FROM (SELECT eggdb_catalog.openlineage_event('build_mart', 'COMPLETE') AS e) AS event",
        )
        .unwrap();
        assert!(facet.contains("\"field\": \"amount\""), "{facet}");
    }

    #[test]
    fn schema_registry_checks_compatibility_and_contracts_check_tables() {
        let store = catalog();
        let mut session = Session::new(&store);
        let first = r#"{"fields": [{"name": "id", "type": "integer", "required": true}, {"name": "email", "type": "text"}]}"#;
        let breaking = r#"{"fields": [{"name": "id", "type": "integer", "required": true}, {"name": "email", "type": "text"}, {"name": "tier", "type": "text", "required": true}]}"#;
        let widened = r#"{"fields": [{"name": "id", "type": "integer", "required": true}, {"name": "email", "type": "text"}, {"name": "country", "type": "text"}]}"#;
        assert_eq!(run(&mut session, &format!("SELECT eggdb_catalog.register_schema('customers-value', '{first}') AS version")).unwrap(), "version\n1\nSELECT 1");
        assert_eq!(run(&mut session, &format!("SELECT eggdb_catalog.register_schema('customers-value', '{first}') AS version")).unwrap(), "version\n1\nSELECT 1");
        let refused = run(&mut session, &format!("SELECT eggdb_catalog.register_schema('customers-value', '{breaking}')")).unwrap_err();
        assert!(refused.to_string().contains("tier: required field missing"), "{refused}");
        assert_eq!(run(&mut session, &format!("SELECT eggdb_catalog.register_schema('customers-value', '{widened}') AS version")).unwrap(), "version\n2\nSELECT 1");
        for sql in [
            "INSERT INTO eggdb_catalog.contracts (contract_name, schema_name, table_name, subject, owner, minimum_rows) VALUES ('customers feed', 'public', 'customers', 'customers-value', 'crm-team', 5)",
            "INSERT INTO eggdb_catalog.quality_rules (rule_name, schema_name, table_name, row_condition) VALUES ('ids positive', 'public', 'customers', 'id > 0')",
        ] {
            run(&mut session, sql).unwrap();
        }
        assert_eq!(
            run(&mut session, "SELECT clause, passed, detail FROM eggdb_catalog.check_contract('customers feed')").unwrap(),
            "clause|passed|detail\nschema|t|\nvolume|f|3 rows, at least 5 promised\nquality: ids positive|t|0 rows break it\nSELECT 3"
        );
        assert_eq!(run(&mut session, "SELECT count(*) AS kept FROM eggdb_catalog.contract_results").unwrap(), "kept\n3\nSELECT 1");
    }
}
