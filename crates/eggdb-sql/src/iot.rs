/// The statements `CREATE EXTENSION eggdb_iot` runs: devices described by W3C Web of Things
/// Thing Descriptions, whose property readings and events are checked against the
/// description's data schemas as they are recorded (D57).
pub(crate) const IOT_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_iot;
CREATE TABLE eggdb_iot.things (thing_id text PRIMARY KEY, title text NOT NULL, description jsonb NOT NULL, registered_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE eggdb_iot.readings (thing_id text NOT NULL REFERENCES eggdb_iot.things (thing_id) ON DELETE CASCADE, property text NOT NULL, value jsonb NOT NULL, observed_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX readings_by_property ON eggdb_iot.readings (thing_id, property, observed_at);
CREATE TABLE eggdb_iot.events (thing_id text NOT NULL REFERENCES eggdb_iot.things (thing_id) ON DELETE CASCADE, event text NOT NULL, data jsonb, occurred_at timestamptz NOT NULL DEFAULT now());
CREATE FUNCTION eggdb_iot.schema_problem(data_schema jsonb, value jsonb, depth integer DEFAULT 0) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    wanted text := data_schema ->> 'type';
    actual text := jsonb_typeof(value);
    choice record;
    allowed boolean;
    member record;
    inner_problem text;
BEGIN
    IF data_schema IS NULL OR jsonb_typeof(data_schema) <> 'object' THEN
        RETURN NULL;
    END IF;
    IF depth > 10 THEN
        RETURN 'the data schema nests too deeply';
    END IF;
    IF data_schema ? 'const' AND value <> data_schema -> 'const' THEN
        RETURN format('must be %s', data_schema -> 'const');
    END IF;
    IF jsonb_typeof(data_schema -> 'enum') = 'array' THEN
        allowed := false;
        FOR choice IN SELECT e.value AS option FROM jsonb_array_elements(data_schema -> 'enum') e LOOP
            IF choice.option = value THEN
                allowed := true;
            END IF;
        END LOOP;
        IF NOT allowed THEN
            RETURN format('must be one of %s', data_schema -> 'enum');
        END IF;
    END IF;
    IF wanted IS NULL THEN
        RETURN NULL;
    END IF;
    IF wanted = 'integer' THEN
        IF actual <> 'number' OR (value #>> '{}')::numeric <> trunc((value #>> '{}')::numeric) THEN
            RETURN 'must be an integer';
        END IF;
    ELSIF wanted = 'null' THEN
        IF actual <> 'null' THEN
            RETURN 'must be null';
        END IF;
    ELSIF wanted IN ('number', 'string', 'boolean', 'object', 'array') THEN
        IF actual <> wanted THEN
            RETURN format('must be a %s, not a %s', wanted, actual);
        END IF;
    ELSE
        RETURN format('the data schema has an unknown type %s', wanted);
    END IF;
    IF wanted IN ('integer', 'number') THEN
        IF data_schema ? 'minimum' AND (value #>> '{}')::numeric < (data_schema ->> 'minimum')::numeric THEN
            RETURN format('must be at least %s', data_schema ->> 'minimum');
        END IF;
        IF data_schema ? 'maximum' AND (value #>> '{}')::numeric > (data_schema ->> 'maximum')::numeric THEN
            RETURN format('must be at most %s', data_schema ->> 'maximum');
        END IF;
    ELSIF wanted = 'string' THEN
        IF data_schema ? 'minLength' AND length(value #>> '{}') < (data_schema ->> 'minLength')::integer THEN
            RETURN format('must have at least %s characters', data_schema ->> 'minLength');
        END IF;
        IF data_schema ? 'maxLength' AND length(value #>> '{}') > (data_schema ->> 'maxLength')::integer THEN
            RETURN format('must have at most %s characters', data_schema ->> 'maxLength');
        END IF;
    ELSIF wanted = 'array' THEN
        IF data_schema ? 'minItems' AND jsonb_array_length(value) < (data_schema ->> 'minItems')::integer THEN
            RETURN format('must have at least %s items', data_schema ->> 'minItems');
        END IF;
        IF data_schema ? 'maxItems' AND jsonb_array_length(value) > (data_schema ->> 'maxItems')::integer THEN
            RETURN format('must have at most %s items', data_schema ->> 'maxItems');
        END IF;
        IF jsonb_typeof(data_schema -> 'items') = 'object' THEN
            FOR member IN SELECT e.value AS item, e.ordinality AS position FROM jsonb_array_elements(value) WITH ORDINALITY AS e(value, ordinality) LOOP
                inner_problem := eggdb_iot.schema_problem(data_schema -> 'items', member.item, depth + 1);
                IF inner_problem IS NOT NULL THEN
                    RETURN format('item %s %s', member.position, inner_problem);
                END IF;
            END LOOP;
        END IF;
    ELSIF wanted = 'object' THEN
        IF jsonb_typeof(data_schema -> 'required') = 'array' THEN
            FOR member IN SELECT r.value AS name FROM jsonb_array_elements_text(data_schema -> 'required') r LOOP
                IF NOT value ? member.name THEN
                    RETURN format('needs %s', member.name);
                END IF;
            END LOOP;
        END IF;
        IF jsonb_typeof(data_schema -> 'properties') = 'object' THEN
            FOR member IN SELECT p.key AS name, p.value AS rule FROM jsonb_each(data_schema -> 'properties') p LOOP
                IF value ? member.name THEN
                    inner_problem := eggdb_iot.schema_problem(member.rule, value -> member.name, depth + 1);
                    IF inner_problem IS NOT NULL THEN
                        RETURN format('%s %s', member.name, inner_problem);
                    END IF;
                END IF;
            END LOOP;
        END IF;
    END IF;
    RETURN NULL;
END
$$;
CREATE FUNCTION eggdb_iot.description_problems(td jsonb) RETURNS TABLE (path text, problem text) LANGUAGE plpgsql AS $$
DECLARE
    contexts jsonb := CASE jsonb_typeof(td -> '@context') WHEN 'array' THEN td -> '@context' ELSE jsonb_build_array(td -> '@context') END;
    affordance_kind text;
    affordance record;
    found_form record;
BEGIN
    IF jsonb_typeof(td) IS DISTINCT FROM 'object' THEN
        path := '$';
        problem := 'a Thing Description is a JSON object';
        RETURN NEXT;
        RETURN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM jsonb_array_elements(contexts) c WHERE c.value #>> '{}' IN ('https://www.w3.org/2019/wot/td/v1', 'https://www.w3.org/2022/wot/td/v1.1')) THEN
        path := '@context';
        problem := 'must name the TD 1.0 or 1.1 context';
        RETURN NEXT;
    END IF;
    IF jsonb_typeof(td -> 'title') IS DISTINCT FROM 'string' THEN
        path := 'title';
        problem := 'is required and must be a string';
        RETURN NEXT;
    END IF;
    IF jsonb_typeof(td -> 'securityDefinitions') IS DISTINCT FROM 'object' THEN
        path := 'securityDefinitions';
        problem := 'is required and must be an object';
        RETURN NEXT;
    END IF;
    IF jsonb_typeof(td -> 'security') NOT IN ('string', 'array') OR td -> 'security' IS NULL THEN
        path := 'security';
        problem := 'is required and must name security definitions';
        RETURN NEXT;
    ELSE
        FOR affordance IN SELECT s.value AS name FROM jsonb_array_elements_text(CASE jsonb_typeof(td -> 'security') WHEN 'array' THEN td -> 'security' ELSE jsonb_build_array(td -> 'security') END) s LOOP
            IF NOT coalesce(td -> 'securityDefinitions' ? affordance.name, false) THEN
                path := 'security';
                problem := format('names %s, which securityDefinitions does not define', affordance.name);
                RETURN NEXT;
            END IF;
        END LOOP;
    END IF;
    FOREACH affordance_kind IN ARRAY ARRAY['properties', 'actions', 'events'] LOOP
        IF td ? affordance_kind AND jsonb_typeof(td -> affordance_kind) <> 'object' THEN
            path := affordance_kind;
            problem := 'must be an object';
            RETURN NEXT;
            CONTINUE;
        END IF;
        FOR affordance IN SELECT a.key AS name, a.value AS body FROM jsonb_each(coalesce(td -> affordance_kind, '{}'::jsonb)) a LOOP
            IF jsonb_typeof(affordance.body -> 'forms') IS DISTINCT FROM 'array' OR jsonb_array_length(affordance.body -> 'forms') = 0 THEN
                path := affordance_kind || '.' || affordance.name || '.forms';
                problem := 'needs at least one form';
                RETURN NEXT;
                CONTINUE;
            END IF;
            FOR found_form IN SELECT f.value AS body, f.ordinality AS position FROM jsonb_array_elements(affordance.body -> 'forms') WITH ORDINALITY AS f(value, ordinality) LOOP
                IF jsonb_typeof(found_form.body -> 'href') IS DISTINCT FROM 'string' THEN
                    path := affordance_kind || '.' || affordance.name || '.forms[' || (found_form.position - 1) || '].href';
                    problem := 'is required';
                    RETURN NEXT;
                END IF;
            END LOOP;
        END LOOP;
    END LOOP;
END
$$;
CREATE FUNCTION eggdb_iot.register_thing(td jsonb) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    problems text;
    identifier text;
BEGIN
    SELECT string_agg(p.path || ' ' || p.problem, '; ') INTO problems FROM eggdb_iot.description_problems(td) p;
    IF problems IS NOT NULL THEN
        RAISE EXCEPTION 'the Thing Description is not valid: %', problems USING ERRCODE = '22023';
    END IF;
    identifier := coalesce(td ->> 'id', 'urn:eggdb:thing:' || md5(td ->> 'title'));
    INSERT INTO eggdb_iot.things (thing_id, title, description) VALUES (identifier, td ->> 'title', td)
        ON CONFLICT (thing_id) DO UPDATE SET title = EXCLUDED.title, description = EXCLUDED.description, updated_at = now();
    RETURN identifier;
END
$$;
CREATE FUNCTION eggdb_iot.record_reading(wanted_thing text, property_name text, value jsonb, observed_at timestamptz DEFAULT now()) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    affordance jsonb;
    problem text;
BEGIN
    SELECT t.description -> 'properties' -> property_name INTO affordance FROM eggdb_iot.things t WHERE t.thing_id = wanted_thing;
    IF affordance IS NULL THEN
        RAISE EXCEPTION 'thing % has no property %', wanted_thing, property_name USING ERRCODE = '42704';
    END IF;
    IF (affordance ->> 'writeOnly') = 'true' THEN
        RAISE EXCEPTION 'property % of thing % is write-only, so it has no readings', property_name, wanted_thing USING ERRCODE = '22023';
    END IF;
    problem := eggdb_iot.schema_problem(affordance, value);
    IF problem IS NOT NULL THEN
        RAISE EXCEPTION 'property % of thing % %', property_name, wanted_thing, problem USING ERRCODE = '22023';
    END IF;
    INSERT INTO eggdb_iot.readings (thing_id, property, value, observed_at) VALUES (wanted_thing, property_name, value, record_reading.observed_at);
END
$$;
CREATE FUNCTION eggdb_iot.emit_event(wanted_thing text, event_name text, data jsonb DEFAULT NULL) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    affordance jsonb;
    problem text;
BEGIN
    SELECT t.description -> 'events' -> event_name INTO affordance FROM eggdb_iot.things t WHERE t.thing_id = wanted_thing;
    IF affordance IS NULL THEN
        RAISE EXCEPTION 'thing % has no event %', wanted_thing, event_name USING ERRCODE = '42704';
    END IF;
    IF data IS NOT NULL THEN
        problem := eggdb_iot.schema_problem(affordance -> 'data', data);
        IF problem IS NOT NULL THEN
            RAISE EXCEPTION 'event % of thing % %', event_name, wanted_thing, problem USING ERRCODE = '22023';
        END IF;
    END IF;
    INSERT INTO eggdb_iot.events (thing_id, event, data) VALUES (wanted_thing, event_name, emit_event.data);
END
$$;
CREATE FUNCTION eggdb_iot.latest(wanted_thing text) RETURNS jsonb LANGUAGE plpgsql AS $$
BEGIN
    RETURN coalesce((SELECT jsonb_object_agg(newest.property, newest.value) FROM (
        SELECT r.property, (array_agg(r.value ORDER BY r.observed_at DESC))[1] AS value FROM eggdb_iot.readings r WHERE r.thing_id = wanted_thing GROUP BY r.property) newest), '{}'::jsonb);
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

    const LAMP: &str = r#"{"@context": "https://www.w3.org/2022/wot/td/v1.1", "id": "urn:dev:lamp-1", "title": "Lamp", "securityDefinitions": {"nosec_sc": {"scheme": "nosec"}}, "security": "nosec_sc", "properties": {"brightness": {"type": "integer", "minimum": 0, "maximum": 100, "forms": [{"href": "https://lamp.example/brightness"}]}}, "events": {"overheated": {"data": {"type": "number"}, "forms": [{"href": "https://lamp.example/oh"}]}}}"#;

    #[test]
    fn readings_follow_the_thing_descriptions_schemas() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        run(&mut session, "CREATE EXTENSION eggdb_iot").unwrap();
        assert_eq!(run(&mut session, &format!("SELECT eggdb_iot.register_thing('{LAMP}') AS id")).unwrap(), "id\nurn:dev:lamp-1\nSELECT 1");
        run(&mut session, "SELECT eggdb_iot.record_reading('urn:dev:lamp-1', 'brightness', '42')").unwrap();
        assert_eq!(run(&mut session, "SELECT eggdb_iot.record_reading('urn:dev:lamp-1', 'brightness', '142')").unwrap_err().state().code(), "22023");
        assert_eq!(run(&mut session, "SELECT eggdb_iot.record_reading('urn:dev:lamp-1', 'brightness', '4.5')").unwrap_err().state().code(), "22023");
        assert_eq!(run(&mut session, "SELECT eggdb_iot.latest('urn:dev:lamp-1') AS now").unwrap(), "now\n{\"brightness\": 42}\nSELECT 1");
        assert_eq!(
            run(&mut session, "SELECT path FROM eggdb_iot.description_problems('{\"@context\": \"https://www.w3.org/2019/wot/td/v1\", \"title\": \"x\", \"securityDefinitions\": {}, \"security\": \"missing\", \"actions\": {\"go\": {}}}') ORDER BY path").unwrap(),
            "path\nactions.go.forms\nsecurity\nSELECT 2"
        );
    }
}
