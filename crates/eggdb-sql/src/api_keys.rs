//! The statements `CREATE EXTENSION eggdb_api_keys` runs: API keys for roles (D820). A key is
//! shown once when it is made and kept only as its SHA-256; the `apikey` login method checks a
//! key given as the password with [`API_KEY_CHECK`].

pub(crate) const API_KEYS_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_api_keys;
CREATE TABLE eggdb_api_keys.keys (key_id text PRIMARY KEY, role_name text NOT NULL, label text NOT NULL DEFAULT '', key_hash text NOT NULL UNIQUE, created_at timestamptz NOT NULL DEFAULT now(), expires_at timestamptz, revoked_at timestamptz, last_used_at timestamptz);
CREATE FUNCTION eggdb_api_keys.create_key(role_name text, label text DEFAULT '', valid_for interval DEFAULT NULL) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    new_id text := encode(gen_random_bytes(6), 'hex');
    secret text := encode(gen_random_bytes(32), 'hex');
    key text;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles r WHERE r.rolname = create_key.role_name) THEN
        RAISE EXCEPTION 'role "%" does not exist', create_key.role_name USING ERRCODE = '42704';
    END IF;
    key := 'eggdb_' || new_id || '_' || secret;
    INSERT INTO eggdb_api_keys.keys (key_id, role_name, label, key_hash, expires_at)
        VALUES (new_id, create_key.role_name, coalesce(create_key.label, ''), encode(sha256(key), 'hex'),
                CASE WHEN create_key.valid_for IS NULL THEN NULL ELSE now() + create_key.valid_for END);
    RETURN key;
END
$$;
CREATE FUNCTION eggdb_api_keys.revoke_key(wanted text) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
    UPDATE eggdb_api_keys.keys SET revoked_at = now() WHERE key_id = wanted AND revoked_at IS NULL;
    RETURN FOUND;
END
$$;
CREATE FUNCTION eggdb_api_keys.check_key(role_name text, presented text) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
    UPDATE eggdb_api_keys.keys k SET last_used_at = now()
        WHERE k.role_name = check_key.role_name AND k.key_hash = encode(sha256(presented), 'hex')
          AND k.revoked_at IS NULL AND (k.expires_at IS NULL OR k.expires_at > now());
    RETURN FOUND;
END
$$;
CREATE VIEW eggdb_api_keys.listed AS SELECT key_id, role_name, label, created_at, expires_at, revoked_at, last_used_at FROM eggdb_api_keys.keys;
REVOKE ALL ON eggdb_api_keys.keys FROM PUBLIC;
"#;

/// The query a login runs to check an API key: `$1` is the role, `$2` the key given as the
/// password. It answers one row, `t` when the key is the role's, unrevoked and unexpired.
pub const API_KEY_CHECK: &str = "SELECT eggdb_api_keys.check_key($1, $2)";

/// Whether `key` is a live API key of `role` in the database `store` holds, through
/// [`API_KEY_CHECK`]; `false` when the extension is not installed.
pub fn check_api_key<S: crate::KeyValueStore>(store: &S, role: &str, key: &str) -> bool {
    let mut session = crate::Session::new(store);
    let quoted = |text: &str| format!("'{}'", text.replace('\'', "''"));
    let sql = API_KEY_CHECK
        .replace("$1", &quoted(role))
        .replace("$2", &quoted(key));
    let mut accepted = false;
    let ran = session.execute(&sql, &mut |result| {
        accepted = matches!(result.rows.first().and_then(|row| row.first()), Some(crate::Value::Boolean(true)));
    });
    ran.is_ok() && accepted
}

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
    fn keys_are_shown_once_checked_by_hash_and_revoked() {
        crate::memory_store::use_seeded_random_bytes();
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in ["CREATE EXTENSION eggdb_api_keys", "CREATE ROLE reporter LOGIN"] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        let made = run(&mut session, "SELECT eggdb_api_keys.create_key('reporter', 'nightly') AS key").unwrap();
        let key = made.lines().nth(1).unwrap().to_owned();
        assert!(key.starts_with("eggdb_") && key.len() == 6 + 12 + 1 + 64, "{key}");
        assert!(super::check_api_key(&store, "reporter", &key));
        assert!(!super::check_api_key(&store, "someone_else", &key));
        assert!(!super::check_api_key(&store, "reporter", "eggdb_000000000000_00"));
        let stored = run(&mut session, "SELECT count(*) AS plain FROM eggdb_api_keys.keys WHERE key_hash = key_id OR position(key_hash IN label) > 0").unwrap();
        assert_eq!(stored, "plain\n0\nSELECT 1");
        let id = key.split('_').nth(1).unwrap().to_owned();
        assert_eq!(run(&mut session, &format!("SELECT eggdb_api_keys.revoke_key('{id}') AS revoked")).unwrap(), "revoked\nt\nSELECT 1");
        assert!(!super::check_api_key(&store, "reporter", &key));
        assert!(run(&mut session, "SELECT eggdb_api_keys.create_key('nobody')").is_err());
    }
}
