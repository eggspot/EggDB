/// The statements `CREATE EXTENSION eggdb_cleanroom` runs: data clean rooms whose analyses run
/// only once every member approved their exact text and return only groups of at least the
/// room's minimum size, and private set intersection over commutatively blinded identifiers
/// (`psi_blind`, `psi_reblind`), so members can match customers without showing each other
/// their lists (D52, D245).
pub(crate) const CLEANROOM_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_cleanroom;
CREATE TABLE eggdb_cleanroom.rooms (room text PRIMARY KEY, minimum_group integer NOT NULL DEFAULT 10 CHECK (minimum_group >= 2), created_by text NOT NULL DEFAULT current_user, created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE eggdb_cleanroom.members (room text NOT NULL REFERENCES eggdb_cleanroom.rooms (room), member text NOT NULL, PRIMARY KEY (room, member));
CREATE TABLE eggdb_cleanroom.analyses (room text NOT NULL REFERENCES eggdb_cleanroom.rooms (room), analysis text NOT NULL, query text NOT NULL, proposed_by text NOT NULL DEFAULT current_user, proposed_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (room, analysis));
CREATE TABLE eggdb_cleanroom.approvals (room text NOT NULL, analysis text NOT NULL, member text NOT NULL, approved_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (room, analysis, member), FOREIGN KEY (room, analysis) REFERENCES eggdb_cleanroom.analyses (room, analysis));
CREATE TABLE eggdb_cleanroom.runs (run_id bigserial PRIMARY KEY, room text NOT NULL, analysis text NOT NULL, run_by text NOT NULL DEFAULT current_user, run_at timestamptz NOT NULL DEFAULT now(), rows_returned bigint NOT NULL, rows_suppressed bigint NOT NULL);
CREATE TABLE eggdb_cleanroom.intersection_sets (room text NOT NULL REFERENCES eggdb_cleanroom.rooms (room), member text NOT NULL, stage text NOT NULL CHECK (stage IN ('blinded', 'doubled')), tag bigint NOT NULL, element bytea NOT NULL, PRIMARY KEY (room, member, stage, tag));
CREATE FUNCTION eggdb_cleanroom.require_member(room_name text, member_name text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM eggdb_cleanroom.members m WHERE m.room = room_name AND m.member = member_name) THEN
        RAISE EXCEPTION '% is not a member of clean room %', member_name, room_name USING ERRCODE = '42501';
    END IF;
END
$$;
CREATE FUNCTION eggdb_cleanroom.propose(room_name text, analysis_name text, analysis_query text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    PERFORM eggdb_cleanroom.require_member(room_name, current_user);
    INSERT INTO eggdb_cleanroom.analyses (room, analysis, query) VALUES (room_name, analysis_name, analysis_query);
    INSERT INTO eggdb_cleanroom.approvals (room, analysis, member) VALUES (room_name, analysis_name, current_user);
END
$$;
CREATE FUNCTION eggdb_cleanroom.approve(room_name text, analysis_name text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    PERFORM eggdb_cleanroom.require_member(room_name, current_user);
    INSERT INTO eggdb_cleanroom.approvals (room, analysis, member) VALUES (room_name, analysis_name, current_user) ON CONFLICT (room, analysis, member) DO NOTHING;
END
$$;
CREATE FUNCTION eggdb_cleanroom.run(room_name text, analysis_name text) RETURNS TABLE (result jsonb) LANGUAGE plpgsql AS $$
DECLARE
    wanted text;
    pending text;
    threshold integer;
    found_row record;
    group_size bigint;
    returned bigint := 0;
    suppressed bigint := 0;
BEGIN
    PERFORM eggdb_cleanroom.require_member(room_name, current_user);
    SELECT a.query INTO wanted FROM eggdb_cleanroom.analyses a WHERE a.room = room_name AND a.analysis = analysis_name;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'clean room % has no analysis %', room_name, analysis_name USING ERRCODE = '42704';
    END IF;
    SELECT string_agg(m.member, ', ' ORDER BY m.member) INTO pending FROM eggdb_cleanroom.members m
        WHERE m.room = room_name AND NOT EXISTS (SELECT 1 FROM eggdb_cleanroom.approvals p WHERE p.room = m.room AND p.analysis = analysis_name AND p.member = m.member);
    IF pending IS NOT NULL THEN
        RAISE EXCEPTION 'analysis % waits for the approval of %', analysis_name, pending USING ERRCODE = '42501';
    END IF;
    SELECT r.minimum_group INTO threshold FROM eggdb_cleanroom.rooms r WHERE r.room = room_name;
    FOR found_row IN EXECUTE format('SELECT to_jsonb(q) AS body FROM (%s) q', wanted) LOOP
        IF (found_row.body -> 'n') IS NULL THEN
            RAISE EXCEPTION 'a clean room analysis returns the size of each group in a column named n' USING ERRCODE = '22023';
        END IF;
        group_size := (found_row.body ->> 'n')::bigint;
        IF group_size >= threshold THEN
            result := found_row.body;
            returned := returned + 1;
            RETURN NEXT;
        ELSE
            suppressed := suppressed + 1;
        END IF;
    END LOOP;
    INSERT INTO eggdb_cleanroom.runs (room, analysis, rows_returned, rows_suppressed) VALUES (room_name, analysis_name, returned, suppressed);
END
$$;
CREATE FUNCTION eggdb_cleanroom.publish_set(room_name text, tagged_query text, secret_key bytea) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    found_row record;
    published bigint := 0;
BEGIN
    PERFORM eggdb_cleanroom.require_member(room_name, current_user);
    DELETE FROM eggdb_cleanroom.intersection_sets s WHERE s.room = room_name AND s.member = current_user;
    FOR found_row IN EXECUTE format('SELECT t.tag::bigint AS tag, t.identifier::text AS identifier FROM (%s) AS t(tag, identifier) WHERE t.identifier IS NOT NULL', tagged_query) LOOP
        INSERT INTO eggdb_cleanroom.intersection_sets (room, member, stage, tag, element) VALUES (room_name, current_user, 'blinded', found_row.tag, psi_blind(found_row.identifier, secret_key));
        published := published + 1;
    END LOOP;
    RETURN published;
END
$$;
CREATE FUNCTION eggdb_cleanroom.double_set(room_name text, other_member text, secret_key bytea) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    doubled bigint;
BEGIN
    PERFORM eggdb_cleanroom.require_member(room_name, current_user);
    PERFORM eggdb_cleanroom.require_member(room_name, other_member);
    DELETE FROM eggdb_cleanroom.intersection_sets s WHERE s.room = room_name AND s.member = other_member AND s.stage = 'doubled';
    INSERT INTO eggdb_cleanroom.intersection_sets (room, member, stage, tag, element)
        SELECT s.room, s.member, 'doubled', s.tag, psi_reblind(s.element, secret_key) FROM eggdb_cleanroom.intersection_sets s
        WHERE s.room = room_name AND s.member = other_member AND s.stage = 'blinded';
    GET DIAGNOSTICS doubled = ROW_COUNT;
    RETURN doubled;
END
$$;
CREATE FUNCTION eggdb_cleanroom.intersection(room_name text, other_member text) RETURNS TABLE (tag bigint) LANGUAGE plpgsql AS $$
DECLARE
    matched bigint;
    threshold integer;
BEGIN
    PERFORM eggdb_cleanroom.require_member(room_name, current_user);
    SELECT r.minimum_group INTO threshold FROM eggdb_cleanroom.rooms r WHERE r.room = room_name;
    SELECT count(*) INTO matched FROM eggdb_cleanroom.intersection_sets mine
        WHERE mine.room = room_name AND mine.member = current_user AND mine.stage = 'doubled'
          AND EXISTS (SELECT 1 FROM eggdb_cleanroom.intersection_sets theirs WHERE theirs.room = room_name AND theirs.member = other_member AND theirs.stage = 'doubled' AND theirs.element = mine.element);
    IF matched < threshold THEN
        RAISE EXCEPTION 'the intersection has fewer than the % rows clean room % allows', threshold, room_name USING ERRCODE = '42501';
    END IF;
    RETURN QUERY SELECT mine.tag FROM eggdb_cleanroom.intersection_sets mine
        WHERE mine.room = room_name AND mine.member = current_user AND mine.stage = 'doubled'
          AND EXISTS (SELECT 1 FROM eggdb_cleanroom.intersection_sets theirs WHERE theirs.room = room_name AND theirs.member = other_member AND theirs.stage = 'doubled' AND theirs.element = mine.element)
        ORDER BY mine.tag;
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
    fn analyses_need_every_approval_and_small_groups_are_suppressed() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_cleanroom",
            "CREATE TABLE visits (country text, spend integer)",
            "INSERT INTO visits SELECT CASE WHEN g.n <= 12 THEN 'VN' ELSE 'SE' END, g.n FROM generate_series(1, 15) AS g(n)",
            "INSERT INTO eggdb_cleanroom.rooms (room, minimum_group) VALUES ('retail', 10)",
            "INSERT INTO eggdb_cleanroom.members VALUES ('retail', current_user), ('retail', 'partner')",
            "SELECT eggdb_cleanroom.propose('retail', 'spend by country', 'SELECT country, count(*) AS n, sum(spend) AS spend FROM visits GROUP BY country')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT * FROM eggdb_cleanroom.run('retail', 'spend by country')").unwrap_err().state().code(), "42501");
        run(&mut session, "INSERT INTO eggdb_cleanroom.approvals (room, analysis, member) VALUES ('retail', 'spend by country', 'partner')").unwrap();
        assert_eq!(
            run(&mut session, "SELECT result ->> 'country' AS country, result ->> 'n' AS n FROM eggdb_cleanroom.run('retail', 'spend by country')").unwrap(),
            "country|n\nVN|12\nSELECT 1"
        );
        assert_eq!(run(&mut session, "SELECT rows_returned, rows_suppressed FROM eggdb_cleanroom.runs").unwrap(), "rows_returned|rows_suppressed\n1|1\nSELECT 1");
    }

    #[test]
    fn blinded_identifiers_meet_only_when_both_hold_them() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        let alice = "'\\x0707070707070707070707070707070707070707070707070707070707070707'::bytea";
        let bob = "'\\x0909090909090909090909090909090909090909090909090909090909090909'::bytea";
        assert_eq!(
            run(&mut session, &format!("SELECT psi_reblind(psi_blind('egg@eggspot.app', {alice}), {bob}) = psi_reblind(psi_blind('egg@eggspot.app', {bob}), {alice}) AS same, psi_reblind(psi_blind('hen@eggspot.app', {alice}), {bob}) = psi_reblind(psi_blind('egg@eggspot.app', {bob}), {alice}) AS other")).unwrap(),
            "same|other\nt|f\nSELECT 1"
        );
        assert!(run(&mut session, "SELECT psi_blind('egg', '\\x01'::bytea)").is_err());
    }
}
