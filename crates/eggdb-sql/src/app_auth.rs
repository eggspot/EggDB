/// The statements `CREATE EXTENSION eggdb_app_auth` runs: sign-in for an application's end
/// users (sign-up with a password, magic links, and OpenID Connect ID tokens from social and
/// enterprise providers), issuing ES256 JWT sessions that the REST API accepts and that
/// row-level security reads through `eggdb_app_auth.current_user_id()` (D37).
pub(crate) const APP_AUTH_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_app_auth;
CREATE TABLE eggdb_app_auth.settings (setting text PRIMARY KEY, value text NOT NULL);
INSERT INTO eggdb_app_auth.settings VALUES ('issuer', 'eggdb'), ('audience', 'eggdb-app'), ('session_minutes', '60'), ('app_role', 'app_user'), ('magic_link_minutes', '15'), ('most_failures', '5'), ('failure_window_minutes', '15');
CREATE TABLE eggdb_app_auth.signing_keys (kid text PRIMARY KEY, private_key bytea NOT NULL, created_at timestamptz NOT NULL DEFAULT now(), retired_at timestamptz);
CREATE TABLE eggdb_app_auth.users (user_id bigserial PRIMARY KEY, email text UNIQUE, password_hash text, email_confirmed_at timestamptz, created_at timestamptz NOT NULL DEFAULT now(), disabled_at timestamptz);
CREATE TABLE eggdb_app_auth.providers (provider text PRIMARY KEY, issuer text NOT NULL, audience text NOT NULL, jwks jsonb NOT NULL, link_verified_email boolean NOT NULL DEFAULT false, updated_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE eggdb_app_auth.identities (provider text NOT NULL REFERENCES eggdb_app_auth.providers (provider), subject text NOT NULL, user_id bigint NOT NULL REFERENCES eggdb_app_auth.users (user_id), linked_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (provider, subject));
CREATE TABLE eggdb_app_auth.magic_links (token_hash text PRIMARY KEY, email text NOT NULL, created_at timestamptz NOT NULL DEFAULT now(), expires_at timestamptz NOT NULL, used_at timestamptz);
CREATE TABLE eggdb_app_auth.sessions (session_id text PRIMARY KEY, user_id bigint NOT NULL REFERENCES eggdb_app_auth.users (user_id), method text NOT NULL, issued_at timestamptz NOT NULL DEFAULT now(), expires_at timestamptz NOT NULL, revoked_at timestamptz);
CREATE TABLE eggdb_app_auth.failures (email text NOT NULL, failed_at timestamptz NOT NULL DEFAULT now());
CREATE FUNCTION eggdb_app_auth.setting(wanted text) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
    RETURN (SELECT s.value FROM eggdb_app_auth.settings s WHERE s.setting = wanted);
END
$$;
CREATE FUNCTION eggdb_app_auth.rotate_signing_key() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    candidate bytea;
    public_key jsonb;
BEGIN
    LOOP
        candidate := gen_random_bytes(32);
        BEGIN
            public_key := eggdb_jwt_public_jwk(candidate);
            EXIT;
        EXCEPTION WHEN SQLSTATE '22023' THEN
            NULL;
        END;
    END LOOP;
    UPDATE eggdb_app_auth.signing_keys SET retired_at = now() WHERE retired_at IS NULL;
    INSERT INTO eggdb_app_auth.signing_keys (kid, private_key) VALUES (public_key ->> 'kid', candidate);
    RETURN public_key ->> 'kid';
END
$$;
CREATE FUNCTION eggdb_app_auth.jwks() RETURNS jsonb LANGUAGE plpgsql AS $$
BEGIN
    RETURN jsonb_build_object('keys', coalesce((SELECT jsonb_agg(eggdb_jwt_public_jwk(k.private_key) ORDER BY k.created_at DESC)
        FROM eggdb_app_auth.signing_keys k
        WHERE k.retired_at IS NULL OR k.retired_at > now() - (eggdb_app_auth.setting('session_minutes') || ' minutes')::interval), '[]'::jsonb));
END
$$;
CREATE FUNCTION eggdb_app_auth.setup(app_role text DEFAULT 'app_user') RETURNS jsonb LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles r WHERE r.rolname = app_role) THEN
        EXECUTE format('CREATE ROLE %I LOGIN', app_role);
    END IF;
    UPDATE eggdb_app_auth.settings SET value = app_role WHERE setting = 'app_role';
    EXECUTE format('GRANT USAGE ON SCHEMA eggdb_app_auth TO %I', app_role);
    EXECUTE format('GRANT SELECT ON eggdb_app_auth.sessions TO %I', app_role);
    IF NOT EXISTS (SELECT 1 FROM eggdb_app_auth.signing_keys k WHERE k.retired_at IS NULL) THEN
        PERFORM eggdb_app_auth.rotate_signing_key();
    END IF;
    RETURN eggdb_app_auth.jwks();
END
$$;
CREATE FUNCTION eggdb_app_auth.issue_session(signed_in_user bigint, sign_in_method text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    signing bytea;
    session_key text := encode(gen_random_bytes(16), 'hex');
    lifetime interval := (eggdb_app_auth.setting('session_minutes') || ' minutes')::interval;
    account record;
BEGIN
    SELECT k.private_key INTO signing FROM eggdb_app_auth.signing_keys k WHERE k.retired_at IS NULL ORDER BY k.created_at DESC LIMIT 1;
    IF signing IS NULL THEN
        RAISE EXCEPTION 'no signing key: call eggdb_app_auth.setup()' USING ERRCODE = '55000';
    END IF;
    SELECT u.user_id, u.email, u.disabled_at INTO account FROM eggdb_app_auth.users u WHERE u.user_id = signed_in_user;
    IF account.disabled_at IS NOT NULL THEN
        RETURN NULL;
    END IF;
    INSERT INTO eggdb_app_auth.sessions (session_id, user_id, method, expires_at) VALUES (session_key, signed_in_user, sign_in_method, now() + lifetime);
    RETURN eggdb_jwt_sign(jsonb_build_object(
        'iss', eggdb_app_auth.setting('issuer'),
        'aud', eggdb_app_auth.setting('audience'),
        'sub', eggdb_app_auth.setting('app_role'),
        'user_id', signed_in_user,
        'email', account.email,
        'sid', session_key,
        'amr', jsonb_build_array(sign_in_method),
        'iat', floor(extract(epoch FROM now()))::bigint,
        'exp', floor(extract(epoch FROM now() + lifetime))::bigint)::text, signing);
END
$$;
CREATE FUNCTION eggdb_app_auth.too_many_failures(wanted_email text) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
    RETURN (SELECT count(*) FROM eggdb_app_auth.failures f
        WHERE f.email = lower(wanted_email) AND f.failed_at > now() - (eggdb_app_auth.setting('failure_window_minutes') || ' minutes')::interval)
        >= eggdb_app_auth.setting('most_failures')::bigint;
END
$$;
CREATE FUNCTION eggdb_app_auth.sign_up(new_email text, new_password text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    created bigint;
BEGIN
    IF new_email IS NULL OR position('@' IN new_email) < 2 THEN
        RAISE EXCEPTION 'an e-mail address is needed' USING ERRCODE = '22023';
    END IF;
    IF new_password IS NULL OR length(new_password) < 8 THEN
        RAISE EXCEPTION 'a password needs at least 8 characters' USING ERRCODE = '22023';
    END IF;
    INSERT INTO eggdb_app_auth.users (email, password_hash) VALUES (lower(new_email), eggdb_password_hash(new_password)) RETURNING users.user_id INTO created;
    RETURN created;
END
$$;
CREATE FUNCTION eggdb_app_auth.sign_in(given_email text, given_password text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    account record;
BEGIN
    IF eggdb_app_auth.too_many_failures(given_email) THEN
        RETURN NULL;
    END IF;
    SELECT u.user_id, u.password_hash INTO account FROM eggdb_app_auth.users u WHERE u.email = lower(given_email);
    IF account.password_hash IS NULL OR NOT eggdb_password_verify(given_password, account.password_hash) THEN
        INSERT INTO eggdb_app_auth.failures (email) VALUES (lower(given_email));
        RETURN NULL;
    END IF;
    RETURN eggdb_app_auth.issue_session(account.user_id, 'pwd');
END
$$;
CREATE FUNCTION eggdb_app_auth.request_magic_link(wanted_email text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    secret text := encode(gen_random_bytes(32), 'hex');
BEGIN
    IF wanted_email IS NULL OR position('@' IN wanted_email) < 2 THEN
        RAISE EXCEPTION 'an e-mail address is needed' USING ERRCODE = '22023';
    END IF;
    INSERT INTO eggdb_app_auth.magic_links (token_hash, email, expires_at)
        VALUES (encode(sha256(secret), 'hex'), lower(wanted_email), now() + (eggdb_app_auth.setting('magic_link_minutes') || ' minutes')::interval);
    RETURN secret;
END
$$;
CREATE FUNCTION eggdb_app_auth.sign_in_with_magic_link(secret text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    link record;
    account bigint;
BEGIN
    SELECT m.token_hash, m.email INTO link FROM eggdb_app_auth.magic_links m
        WHERE m.token_hash = encode(sha256(secret), 'hex') AND m.used_at IS NULL AND m.expires_at > now();
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;
    UPDATE eggdb_app_auth.magic_links SET used_at = now() WHERE token_hash = link.token_hash;
    SELECT u.user_id INTO account FROM eggdb_app_auth.users u WHERE u.email = link.email;
    IF account IS NULL THEN
        INSERT INTO eggdb_app_auth.users (email, email_confirmed_at) VALUES (link.email, now()) RETURNING users.user_id INTO account;
    ELSE
        UPDATE eggdb_app_auth.users SET email_confirmed_at = coalesce(email_confirmed_at, now()) WHERE user_id = account;
    END IF;
    RETURN eggdb_app_auth.issue_session(account, 'otp');
END
$$;
CREATE FUNCTION eggdb_app_auth.sign_in_with_id_token(provider_name text, id_token text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    trusted record;
    claims jsonb;
    account bigint;
    verified_email text;
BEGIN
    SELECT * INTO trusted FROM eggdb_app_auth.providers p WHERE p.provider = provider_name;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'there is no provider %', provider_name USING ERRCODE = '42704';
    END IF;
    claims := eggdb_jwt_verify(id_token, trusted.jwks::text, trusted.issuer, trusted.audience);
    SELECT i.user_id INTO account FROM eggdb_app_auth.identities i WHERE i.provider = provider_name AND i.subject = claims ->> 'sub';
    IF account IS NULL THEN
        verified_email := CASE WHEN (claims ->> 'email_verified') = 'true' THEN lower(claims ->> 'email') END;
        IF verified_email IS NOT NULL AND trusted.link_verified_email THEN
            SELECT u.user_id INTO account FROM eggdb_app_auth.users u WHERE u.email = verified_email;
        END IF;
        IF account IS NULL THEN
            INSERT INTO eggdb_app_auth.users (email, email_confirmed_at)
                VALUES (CASE WHEN verified_email IS NOT NULL AND NOT EXISTS (SELECT 1 FROM eggdb_app_auth.users u WHERE u.email = verified_email) THEN verified_email END,
                        CASE WHEN verified_email IS NOT NULL THEN now() END)
                RETURNING users.user_id INTO account;
        END IF;
        INSERT INTO eggdb_app_auth.identities (provider, subject, user_id) VALUES (provider_name, claims ->> 'sub', account);
    END IF;
    RETURN eggdb_app_auth.issue_session(account, 'oidc');
END
$$;
CREATE FUNCTION eggdb_app_auth.sign_out(ended_session text) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE
    changed bigint;
BEGIN
    UPDATE eggdb_app_auth.sessions SET revoked_at = now() WHERE session_id = ended_session AND revoked_at IS NULL;
    GET DIAGNOSTICS changed = ROW_COUNT;
    RETURN changed > 0;
END
$$;
CREATE FUNCTION eggdb_app_auth.current_user_id() RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    claims jsonb := nullif(current_setting('request.jwt.claims', true), '')::jsonb;
BEGIN
    IF claims IS NULL OR NOT EXISTS (SELECT 1 FROM eggdb_app_auth.sessions s WHERE s.session_id = claims ->> 'sid' AND s.revoked_at IS NULL AND s.expires_at > now()) THEN
        RETURN NULL;
    END IF;
    RETURN (claims ->> 'user_id')::bigint;
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
    fn passwords_and_magic_links_issue_sessions_that_row_security_reads() {
        crate::memory_store::use_seeded_random_bytes();
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_app_auth",
            "SELECT eggdb_app_auth.setup('app_user')",
            "SELECT eggdb_app_auth.sign_up('Ada@Example.com', 'correct horse')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT eggdb_app_auth.sign_in('ada@example.com', 'wrong horse') IS NULL AS refused").unwrap(), "refused\nt\nSELECT 1");
        assert_eq!(
            run(&mut session, "SELECT c ->> 'user_id' AS user_id, c ->> 'sub' AS role FROM (SELECT eggdb_jwt_verify(eggdb_app_auth.sign_in('ada@example.com', 'correct horse'), eggdb_app_auth.jwks()::text, 'eggdb', 'eggdb-app') AS c) t").unwrap(),
            "user_id|role\n1|app_user\nSELECT 1"
        );
        let link = run(&mut session, "SELECT eggdb_app_auth.request_magic_link('bo@example.com') AS link").unwrap();
        let secret = link.lines().nth(1).unwrap().to_owned();
        let token = run(&mut session, &format!("SELECT eggdb_app_auth.sign_in_with_magic_link('{secret}') AS token")).unwrap();
        let token = token.lines().nth(1).unwrap().to_owned();
        assert_eq!(run(&mut session, &format!("SELECT eggdb_app_auth.sign_in_with_magic_link('{secret}') IS NULL AS used")).unwrap(), "used\nt\nSELECT 1");
        run(&mut session, &format!("SELECT set_config('request.jwt.claims', eggdb_jwt_verify('{token}', eggdb_app_auth.jwks()::text, 'eggdb', 'eggdb-app')::text, false)")).unwrap();
        assert_eq!(run(&mut session, "SELECT eggdb_app_auth.current_user_id() AS me").unwrap(), "me\n2\nSELECT 1");
    }
}
