/// The statements `CREATE EXTENSION eggdb_shredding` runs: crypto-shredding, with a key per
/// data subject that seals the subject's values, held under a key-encryption key the operator
/// keeps outside the database, so destroying one subject's key leaves its values unreadable
/// wherever they were copied, unless a hold keeps the key (D26, D53, D246).
pub(crate) const SHREDDING_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_shredding;
CREATE TABLE eggdb_shredding.subject_keys (subject text PRIMARY KEY, wrapped_key bytea, created_at timestamptz NOT NULL DEFAULT now(), shredded_at timestamptz, CHECK ((wrapped_key IS NULL) = (shredded_at IS NOT NULL)));
CREATE TABLE eggdb_shredding.holds (hold_name text NOT NULL, subject text NOT NULL, reason text NOT NULL, placed_at timestamptz NOT NULL DEFAULT now(), lifted_at timestamptz, PRIMARY KEY (hold_name, subject));
CREATE TABLE eggdb_shredding.shred_log (subject text NOT NULL, requested_at timestamptz NOT NULL DEFAULT now(), requested_by text NOT NULL DEFAULT current_user, outcome text NOT NULL);
CREATE FUNCTION eggdb_shredding.wrapping_key() RETURNS bytea LANGUAGE plpgsql AS $$
DECLARE
    given text := current_setting('eggdb_shredding.wrapping_key', true);
BEGIN
    IF given IS NULL OR length(given) <> 64 THEN
        RAISE EXCEPTION 'set eggdb_shredding.wrapping_key to the 32-byte key-encryption key in hex, kept outside the database' USING ERRCODE = '55000';
    END IF;
    RETURN decode(given, 'hex');
END
$$;
CREATE FUNCTION eggdb_shredding.subject_key(wanted text, create_missing boolean) RETURNS bytea LANGUAGE plpgsql AS $$
DECLARE
    stored record;
    fresh bytea;
BEGIN
    SELECT k.wrapped_key, k.shredded_at INTO stored FROM eggdb_shredding.subject_keys k WHERE k.subject = wanted;
    IF FOUND THEN
        IF stored.shredded_at IS NOT NULL THEN
            RETURN NULL;
        END IF;
        RETURN eggdb_unseal(stored.wrapped_key, eggdb_shredding.wrapping_key());
    END IF;
    IF NOT create_missing THEN
        RETURN NULL;
    END IF;
    fresh := gen_random_bytes(32);
    INSERT INTO eggdb_shredding.subject_keys (subject, wrapped_key) VALUES (wanted, eggdb_seal(fresh, eggdb_shredding.wrapping_key()));
    RETURN fresh;
END
$$;
CREATE FUNCTION eggdb_shredding.seal(subject text, plaintext text) RETURNS bytea LANGUAGE plpgsql AS $$
DECLARE
    key bytea := eggdb_shredding.subject_key(subject, true);
BEGIN
    IF key IS NULL THEN
        RAISE EXCEPTION 'subject % was shredded: nothing more can be sealed for it', subject USING ERRCODE = '55000';
    END IF;
    RETURN eggdb_seal(convert_to(plaintext, 'UTF8'), key);
END
$$;
CREATE FUNCTION eggdb_shredding.unseal(subject text, sealed bytea) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    key bytea := eggdb_shredding.subject_key(subject, false);
BEGIN
    IF key IS NULL OR sealed IS NULL THEN
        RETURN NULL;
    END IF;
    RETURN convert_from(eggdb_unseal(sealed, key), 'UTF8');
END
$$;
CREATE FUNCTION eggdb_shredding.place_hold(hold_name text, subject text, reason text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO eggdb_shredding.holds (hold_name, subject, reason) VALUES (place_hold.hold_name, place_hold.subject, place_hold.reason);
END
$$;
CREATE FUNCTION eggdb_shredding.lift_hold(lifted text, subject text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    UPDATE eggdb_shredding.holds h SET lifted_at = now() WHERE h.hold_name = lifted AND h.subject = lift_hold.subject AND h.lifted_at IS NULL;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'there is no active hold % on %', lifted, subject USING ERRCODE = '22023';
    END IF;
END
$$;
CREATE FUNCTION eggdb_shredding.shred(subject text) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE
    hold text;
    changed bigint;
BEGIN
    SELECT h.hold_name INTO hold FROM eggdb_shredding.holds h WHERE h.subject = shred.subject AND h.lifted_at IS NULL ORDER BY h.hold_name LIMIT 1;
    IF hold IS NOT NULL THEN
        INSERT INTO eggdb_shredding.shred_log (subject, outcome) VALUES (shred.subject, 'waiting for hold ' || hold);
        RETURN false;
    END IF;
    UPDATE eggdb_shredding.subject_keys k SET wrapped_key = NULL, shredded_at = now() WHERE k.subject = shred.subject AND k.shredded_at IS NULL;
    GET DIAGNOSTICS changed = ROW_COUNT;
    INSERT INTO eggdb_shredding.shred_log (subject, outcome) VALUES (shred.subject, CASE WHEN changed > 0 THEN 'shredded' ELSE 'no key' END);
    RETURN changed > 0;
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
    fn a_shredded_subject_can_no_longer_be_read_unless_held() {
        crate::memory_store::use_seeded_random_bytes();
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_shredding",
            "SET eggdb_shredding.wrapping_key = '000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f'",
            "CREATE TABLE profiles (subject text, name bytea)",
            "INSERT INTO profiles VALUES ('user-1', eggdb_shredding.seal('user-1', 'Ada')), ('user-2', eggdb_shredding.seal('user-2', 'Bo'))",
            "SELECT eggdb_shredding.place_hold('case-9', 'user-2', 'litigation')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT eggdb_shredding.unseal(subject, name) AS name FROM profiles ORDER BY subject").unwrap(), "name\nAda\nBo\nSELECT 2");
        assert_eq!(run(&mut session, "SELECT eggdb_shredding.shred('user-1') AS one, eggdb_shredding.shred('user-2') AS two").unwrap(), "one|two\nt|f\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT eggdb_shredding.unseal(subject, name) AS name FROM profiles ORDER BY subject").unwrap(), "name\nNULL\nBo\nSELECT 2");
        assert!(run(&mut session, "SELECT eggdb_shredding.seal('user-1', 'again')").is_err());
    }
}
