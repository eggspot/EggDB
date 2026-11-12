/// The statements `CREATE EXTENSION eggdb_secrets` runs: versioned secrets sealed with
/// AES-GCM under a key the caller holds, and short-lived database credentials issued as
/// login roles that expire, in Vault's style (D54).
pub(crate) const SECRETS_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_secrets;
CREATE TABLE eggdb_secrets.secrets (path text NOT NULL, version integer NOT NULL, sealed bytea NOT NULL, created_at timestamptz NOT NULL DEFAULT now(), created_by text NOT NULL DEFAULT current_user, expires_at timestamptz, PRIMARY KEY (path, version));
CREATE TABLE eggdb_secrets.leases (username text PRIMARY KEY, template_role text NOT NULL, issued_at timestamptz NOT NULL DEFAULT now(), issued_by text NOT NULL DEFAULT current_user, expires_at timestamptz NOT NULL, revoked_at timestamptz);
CREATE FUNCTION eggdb_secrets.put(secret_path text, secret_value text, sealing_key bytea, ttl interval DEFAULT NULL) RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
    next_version integer;
BEGIN
    SELECT coalesce(max(s.version), 0) + 1 INTO next_version FROM eggdb_secrets.secrets s WHERE s.path = secret_path;
    INSERT INTO eggdb_secrets.secrets (path, version, sealed, expires_at)
        VALUES (secret_path, next_version, eggdb_seal(convert_to(secret_value, 'UTF8'), sealing_key), CASE WHEN ttl IS NULL THEN NULL ELSE now() + ttl END);
    RETURN next_version;
END
$$;
CREATE FUNCTION eggdb_secrets.get(secret_path text, sealing_key bytea, wanted_version integer DEFAULT NULL) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    stored bytea;
BEGIN
    SELECT s.sealed INTO stored FROM eggdb_secrets.secrets s
        WHERE s.path = secret_path AND (wanted_version IS NULL OR s.version = wanted_version) AND (s.expires_at IS NULL OR s.expires_at > now())
        ORDER BY s.version DESC LIMIT 1;
    IF stored IS NULL THEN
        RETURN NULL;
    END IF;
    RETURN convert_from(eggdb_unseal(stored, sealing_key), 'UTF8');
END
$$;
CREATE FUNCTION eggdb_secrets.destroy(secret_path text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    removed bigint;
BEGIN
    DELETE FROM eggdb_secrets.secrets s WHERE s.path = secret_path;
    GET DIAGNOSTICS removed = ROW_COUNT;
    RETURN removed;
END
$$;
CREATE FUNCTION eggdb_secrets.rotate_key(old_key bytea, new_key bytea) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    found record;
    rotated bigint := 0;
BEGIN
    FOR found IN SELECT s.path, s.version, s.sealed FROM eggdb_secrets.secrets s ORDER BY s.path, s.version LOOP
        UPDATE eggdb_secrets.secrets SET sealed = eggdb_seal(eggdb_unseal(found.sealed, old_key), new_key) WHERE path = found.path AND version = found.version;
        rotated := rotated + 1;
    END LOOP;
    RETURN rotated;
END
$$;
CREATE FUNCTION eggdb_secrets.issue_credentials(template_role text, ttl interval DEFAULT interval '1 hour') RETURNS TABLE (username text, password text, expires_at timestamptz) LANGUAGE plpgsql AS $$
DECLARE
    made_user text := 'v_' || left(template_role, 40) || '_' || substr(replace(gen_random_uuid()::text, '-', ''), 1, 12);
    made_password text := encode(gen_random_bytes(24), 'hex');
    until timestamptz := now() + ttl;
BEGIN
    IF ttl <= interval '0' OR ttl > interval '31 days' THEN
        RAISE EXCEPTION 'credentials live from a second to 31 days' USING ERRCODE = '22023';
    END IF;
    EXECUTE format('CREATE ROLE %I WITH LOGIN PASSWORD %L VALID UNTIL %L IN ROLE %I', made_user, made_password, until::text, template_role);
    INSERT INTO eggdb_secrets.leases (username, template_role, expires_at) VALUES (made_user, template_role, until);
    username := made_user;
    password := made_password;
    expires_at := until;
    RETURN NEXT;
END
$$;
CREATE FUNCTION eggdb_secrets.renew_credentials(lease_user text, ttl interval DEFAULT interval '1 hour') RETURNS timestamptz LANGUAGE plpgsql AS $$
DECLARE
    until timestamptz := now() + ttl;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM eggdb_secrets.leases l WHERE l.username = lease_user AND l.revoked_at IS NULL AND l.expires_at > now()) THEN
        RAISE EXCEPTION 'no live lease for %', lease_user USING ERRCODE = '42704';
    END IF;
    EXECUTE format('ALTER ROLE %I VALID UNTIL %L', lease_user, until::text);
    UPDATE eggdb_secrets.leases SET expires_at = until WHERE username = lease_user;
    RETURN until;
END
$$;
CREATE FUNCTION eggdb_secrets.revoke_credentials(lease_user text) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM eggdb_secrets.leases l WHERE l.username = lease_user AND l.revoked_at IS NULL) THEN
        RETURN false;
    END IF;
    EXECUTE format('DROP ROLE IF EXISTS %I', lease_user);
    UPDATE eggdb_secrets.leases SET revoked_at = now() WHERE username = lease_user;
    RETURN true;
END
$$;
CREATE FUNCTION eggdb_secrets.expire_credentials() RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    expired text;
    dropped bigint := 0;
BEGIN
    FOR expired IN SELECT l.username FROM eggdb_secrets.leases l WHERE l.revoked_at IS NULL AND l.expires_at <= now() ORDER BY l.username LOOP
        EXECUTE format('DROP ROLE IF EXISTS %I', expired);
        UPDATE eggdb_secrets.leases SET revoked_at = now() WHERE username = expired;
        dropped := dropped + 1;
    END LOOP;
    RETURN dropped;
END
$$;
"#;
