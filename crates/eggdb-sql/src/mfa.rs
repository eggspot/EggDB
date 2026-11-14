//! `CREATE EXTENSION eggdb_mfa` (D1187): a second factor for roles, time-based one-time
//! passwords from an authenticator app. `eggdb_mfa.enroll_totp(role)` makes a secret and returns
//! the `otpauth://` URI to scan, `eggdb_mfa.remove_totp(role)` ends it; a `password+totp` login
//! rule then takes the role's password and the app's current code, which [`check_totp`] checks.

use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

pub(crate) const MFA_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_mfa;
CREATE TABLE eggdb_mfa.totp (role_name text PRIMARY KEY, secret bytea NOT NULL, last_step bigint, enrolled_at timestamptz NOT NULL DEFAULT now());
CREATE FUNCTION eggdb_mfa.enroll_totp(role_name text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    secret bytea := gen_random_bytes(20);
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles r WHERE r.rolname = enroll_totp.role_name) THEN
        RAISE EXCEPTION 'role "%" does not exist', enroll_totp.role_name USING ERRCODE = '42704';
    END IF;
    DELETE FROM eggdb_mfa.totp t WHERE t.role_name = enroll_totp.role_name;
    INSERT INTO eggdb_mfa.totp (role_name, secret) VALUES (enroll_totp.role_name, secret);
    RETURN eggdb_totp_uri('EggDB', enroll_totp.role_name, secret);
END
$$;
CREATE FUNCTION eggdb_mfa.remove_totp(role_name text) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM eggdb_mfa.totp t WHERE t.role_name = remove_totp.role_name;
    RETURN FOUND;
END
$$;
CREATE VIEW eggdb_mfa.enrolled AS SELECT role_name, enrolled_at FROM eggdb_mfa.totp;
REVOKE ALL ON eggdb_mfa.totp FROM PUBLIC;
"#;

/// The SQL functions this module answers.
pub(crate) const FUNCTIONS: [&str; 1] = ["eggdb_totp_uri"];

fn percent_encoded(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    let [issuer, account, secret] = values else {
        return Some(Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "eggdb_totp_uri takes an issuer, an account and a secret")));
    };
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    let (Some(issuer), Some(account), Value::Bytea(secret)) = (issuer.to_text(), account.to_text(), secret) else {
        return Some(Err(SqlError::new(SqlState::DATATYPE_MISMATCH, "eggdb_totp_uri takes text, text and bytea")));
    };
    Some(Ok(Value::Text(format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={}&period={}",
        percent_encoded(&issuer),
        percent_encoded(&account),
        eggdb_auth::totp::base32(secret),
        percent_encoded(&issuer),
        eggdb_auth::totp::DIGITS,
        eggdb_auth::totp::STEP_SECONDS
    ))))
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Whether `code` is the current one-time password of `role` in the database `store` holds,
/// and was not used before: the matching step is stored in the same statement that checks it
/// was later than the last one used, so a code is accepted once even by two logins at the same
/// moment. `false` when the extension is not installed or the role has no secret.
pub fn check_totp<S: crate::KeyValueStore>(store: &S, role: &str, code: &str, now: i64) -> bool {
    let mut session = crate::Session::new(store);
    let mut found: Option<(Vec<u8>, Option<u64>)> = None;
    let read = session.execute(
        &format!("SELECT secret, last_step FROM eggdb_mfa.totp WHERE role_name = {}", quoted(role)),
        &mut |result| {
            if let Some(row) = result.rows.first()
                && let Some(Value::Bytea(secret)) = row.first()
            {
                let last = row.get(1).and_then(Value::as_integer).and_then(|step| u64::try_from(step).ok());
                found = Some((secret.clone(), last));
            }
        },
    );
    let Some((secret, last)) = found.filter(|_| read.is_ok()) else {
        return false;
    };
    let Some(step) = eggdb_auth::totp::matching_step(&secret, code, now, last) else {
        return false;
    };
    let mut updated = false;
    let stored = session.execute(
        &format!(
            "UPDATE eggdb_mfa.totp SET last_step = {step} WHERE role_name = {} AND (last_step IS NULL OR last_step < {step}) RETURNING 1",
            quoted(role)
        ),
        &mut |result| updated |= !result.rows.is_empty(),
    );
    stored.is_ok() && updated
}

#[cfg(test)]
mod tests {
    use super::check_totp;
    use crate::memory_store::MemoryStore;
    use crate::{Session, Value};

    #[test]
    fn an_enrolled_role_signs_in_with_its_current_code_once() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in ["CREATE EXTENSION eggdb_mfa", "CREATE ROLE erin LOGIN"] {
            session.execute(sql, &mut |_| {}).unwrap();
        }
        let mut uri = String::new();
        session
            .execute("SELECT eggdb_mfa.enroll_totp('erin')", &mut |result| {
                if let Some(Value::Text(text)) = result.rows.first().and_then(|row| row.first()) {
                    uri = text.clone();
                }
            })
            .unwrap();
        assert!(uri.starts_with("otpauth://totp/EggDB:erin?secret="), "{uri}");
        let mut secret = Vec::new();
        session
            .execute("SELECT secret FROM eggdb_mfa.totp WHERE role_name = 'erin'", &mut |result| {
                if let Some(Value::Bytea(bytes)) = result.rows.first().and_then(|row| row.first()) {
                    secret = bytes.clone();
                }
            })
            .unwrap();
        assert!(uri.contains(&eggdb_auth::totp::base32(&secret)));
        let now = 1_900_000_000;
        let code = format!("{:06}", eggdb_auth::totp::hotp(&secret, eggdb_auth::totp::step_at(now), 6));
        assert!(check_totp(&store, "erin", &code, now));
        assert!(!check_totp(&store, "erin", &code, now + 1));
        assert!(!check_totp(&store, "erin", "000000", now + 300));
        assert!(!check_totp(&store, "nobody", &code, now));
        assert!(session.execute("SELECT eggdb_mfa.enroll_totp('missing')", &mut |_| {}).is_err());
    }
}
