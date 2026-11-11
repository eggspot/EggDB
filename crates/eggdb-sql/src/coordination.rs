pub(crate) const COORDINATION_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_coordination;
CREATE TABLE eggdb_coordination.fencing (only_row boolean PRIMARY KEY DEFAULT true CHECK (only_row), last_token bigint NOT NULL);
INSERT INTO eggdb_coordination.fencing (last_token) VALUES (0);
CREATE TABLE eggdb_coordination.leases (lease_name text PRIMARY KEY, holder text NOT NULL, fencing_token bigint NOT NULL, expires_at timestamptz NOT NULL);
CREATE TABLE eggdb_coordination.flags (flag_key text PRIMARY KEY, enabled boolean NOT NULL DEFAULT true, variants jsonb NOT NULL, default_variant text NOT NULL, targeting jsonb NOT NULL DEFAULT '[]'::jsonb);
CREATE FUNCTION eggdb_coordination.acquire(wanted text, candidate text, ttl interval) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    current_holder text;
    current_token bigint;
    current_expiry timestamptz;
    token bigint;
BEGIN
    IF ttl <= interval '0' THEN
        RAISE EXCEPTION 'a lease needs a positive time to live' USING ERRCODE = '22023';
    END IF;
    SELECT l.holder, l.fencing_token, l.expires_at INTO current_holder, current_token, current_expiry FROM eggdb_coordination.leases l WHERE l.lease_name = wanted FOR UPDATE;
    IF current_holder IS NULL THEN
        UPDATE eggdb_coordination.fencing SET last_token = last_token + 1 RETURNING last_token INTO token;
        INSERT INTO eggdb_coordination.leases VALUES (wanted, candidate, token, now() + ttl);
        RETURN token;
    END IF;
    IF current_holder = candidate AND current_expiry > now() THEN
        UPDATE eggdb_coordination.leases SET expires_at = now() + ttl WHERE lease_name = wanted;
        RETURN current_token;
    END IF;
    IF current_expiry > now() THEN
        RETURN NULL;
    END IF;
    UPDATE eggdb_coordination.fencing SET last_token = last_token + 1 RETURNING last_token INTO token;
    UPDATE eggdb_coordination.leases SET holder = candidate, fencing_token = token, expires_at = now() + ttl WHERE lease_name = wanted;
    RETURN token;
END
$$;
CREATE FUNCTION eggdb_coordination.release(wanted text, candidate text) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM eggdb_coordination.leases WHERE lease_name = wanted AND holder = candidate;
    RETURN FOUND;
END
$$;
CREATE FUNCTION eggdb_coordination.leader(wanted text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    found_holder text;
BEGIN
    SELECT l.holder INTO found_holder FROM eggdb_coordination.leases l WHERE l.lease_name = wanted AND l.expires_at > now();
    RETURN found_holder;
END
$$;
CREATE FUNCTION eggdb_coordination.evaluate_flag(wanted text, evaluation_context jsonb) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE
    flag record;
    rule jsonb;
    attribute text;
    chosen text;
BEGIN
    SELECT * INTO flag FROM eggdb_coordination.flags f WHERE f.flag_key = wanted;
    IF NOT FOUND THEN
        RETURN jsonb_build_object('flagKey', wanted, 'reason', 'ERROR', 'errorCode', 'FLAG_NOT_FOUND');
    END IF;
    IF NOT flag.enabled THEN
        RETURN jsonb_build_object('flagKey', wanted, 'value', flag.variants -> flag.default_variant, 'variant', flag.default_variant, 'reason', 'DISABLED');
    END IF;
    IF jsonb_typeof(flag.targeting) = 'object' THEN
        chosen := eggdb_jsonlogic(flag.targeting, coalesce(evaluation_context, '{}'::jsonb) || jsonb_build_object('$flagd', jsonb_build_object('flagKey', wanted, 'timestamp', extract(epoch FROM now())::bigint))) #>> '{}';
        IF chosen IS NOT NULL AND flag.variants ? chosen THEN
            RETURN jsonb_build_object('flagKey', wanted, 'value', flag.variants -> chosen, 'variant', chosen, 'reason', 'TARGETING_MATCH');
        END IF;
        RETURN jsonb_build_object('flagKey', wanted, 'value', flag.variants -> flag.default_variant, 'variant', flag.default_variant, 'reason', 'DEFAULT');
    END IF;
    FOR rule IN SELECT value FROM jsonb_array_elements(flag.targeting) LOOP
        attribute := evaluation_context ->> (rule ->> 'attribute');
        IF attribute IS NOT NULL AND attribute IN (SELECT jsonb_array_elements_text(rule -> 'in')) AND flag.variants ? (rule ->> 'variant') THEN
            RETURN jsonb_build_object('flagKey', wanted, 'value', flag.variants -> (rule ->> 'variant'), 'variant', rule ->> 'variant', 'reason', 'TARGETING_MATCH');
        END IF;
    END LOOP;
    RETURN jsonb_build_object('flagKey', wanted, 'value', flag.variants -> flag.default_variant, 'variant', flag.default_variant, 'reason', CASE WHEN jsonb_array_length(flag.targeting) = 0 THEN 'STATIC' ELSE 'DEFAULT' END);
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

    fn installed() -> MemoryStore {
        let store = MemoryStore::default();
        Session::new(&store)
            .execute("CREATE EXTENSION eggdb_coordination", &mut |_| {})
            .unwrap();
        store
    }

    #[test]
    fn a_lease_has_one_holder_and_each_new_holder_gets_a_larger_fencing_token() {
        let store = installed();
        let mut session = Session::new(&store);
        assert_eq!(run(&mut session, "SELECT eggdb_coordination.acquire('scheduler', 'node-a', interval '30 seconds') AS token").unwrap(), "token\n1\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT eggdb_coordination.acquire('scheduler', 'node-b', interval '30 seconds') AS token").unwrap(), "token\nNULL\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT eggdb_coordination.acquire('scheduler', 'node-a', interval '30 seconds') AS token").unwrap(), "token\n1\nSELECT 1");
        assert_eq!(
            run(
                &mut session,
                "SELECT eggdb_coordination.leader('scheduler') AS leader"
            )
            .unwrap(),
            "leader\nnode-a\nSELECT 1"
        );
        run(
            &mut session,
            "UPDATE eggdb_coordination.leases SET expires_at = now() - interval '1 second'",
        )
        .unwrap();
        assert_eq!(
            run(
                &mut session,
                "SELECT eggdb_coordination.leader('scheduler') AS leader"
            )
            .unwrap(),
            "leader\nNULL\nSELECT 1"
        );
        assert_eq!(run(&mut session, "SELECT eggdb_coordination.acquire('scheduler', 'node-b', interval '30 seconds') AS token").unwrap(), "token\n2\nSELECT 1");
        assert_eq!(
            run(
                &mut session,
                "SELECT eggdb_coordination.release('scheduler', 'node-a') AS released"
            )
            .unwrap(),
            "released\nf\nSELECT 1"
        );
        assert_eq!(
            run(
                &mut session,
                "SELECT eggdb_coordination.release('scheduler', 'node-b') AS released"
            )
            .unwrap(),
            "released\nt\nSELECT 1"
        );
        assert!(
            run(
                &mut session,
                "SELECT eggdb_coordination.acquire('scheduler', 'node-a', interval '0 seconds')"
            )
            .is_err()
        );
    }

    #[test]
    fn flags_resolve_like_openfeature_providers() {
        let store = installed();
        let mut session = Session::new(&store);
        run(&mut session, r##"INSERT INTO eggdb_coordination.flags (flag_key, variants, default_variant, targeting) VALUES ('new-checkout', '{"on": true, "off": false}', 'off', '[{"attribute": "country", "in": ["VN", "SE"], "variant": "on"}]'), ('banner', '{"blue": "#00f"}', 'blue', '[]')"##).unwrap();
        let resolve = |session: &mut Session<'_>, key: &str, context: &str| {
            run(session, &format!("SELECT eggdb_coordination.evaluate_flag('{key}', '{context}'::jsonb) ->> 'reason' AS reason")).unwrap()
        };
        assert_eq!(
            resolve(&mut session, "new-checkout", r#"{"country": "VN"}"#),
            "reason\nTARGETING_MATCH\nSELECT 1"
        );
        assert_eq!(
            resolve(&mut session, "new-checkout", r#"{"country": "FR"}"#),
            "reason\nDEFAULT\nSELECT 1"
        );
        assert_eq!(
            resolve(&mut session, "banner", "{}"),
            "reason\nSTATIC\nSELECT 1"
        );
        assert_eq!(
            resolve(&mut session, "missing", "{}"),
            "reason\nERROR\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, r#"SELECT eggdb_coordination.evaluate_flag('new-checkout', '{"country": "SE"}'::jsonb) -> 'value' AS value"#).unwrap(),
            "value\ntrue\nSELECT 1"
        );
        run(
            &mut session,
            "UPDATE eggdb_coordination.flags SET enabled = false WHERE flag_key = 'new-checkout'",
        )
        .unwrap();
        assert_eq!(
            resolve(&mut session, "new-checkout", r#"{"country": "VN"}"#),
            "reason\nDISABLED\nSELECT 1"
        );
    }
}
