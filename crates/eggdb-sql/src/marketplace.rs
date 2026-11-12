/// The statements `CREATE EXTENSION eggdb_marketplace` runs: listings of shared tables and
/// views inside one EggDB, which roles find, ask for and are granted read access to by the
/// listing's provider, with no payments and no outside marketplace (D52).
pub(crate) const MARKETPLACE_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_marketplace;
CREATE TABLE eggdb_marketplace.listings (listing text PRIMARY KEY, title text NOT NULL, description text NOT NULL DEFAULT '', provider text NOT NULL DEFAULT current_user, terms text NOT NULL DEFAULT '', approval text NOT NULL DEFAULT 'manual' CHECK (approval IN ('manual', 'automatic')), published_at timestamptz NOT NULL DEFAULT now(), withdrawn_at timestamptz);
CREATE TABLE eggdb_marketplace.listing_objects (listing text NOT NULL REFERENCES eggdb_marketplace.listings (listing), schema_name text NOT NULL, object_name text NOT NULL, PRIMARY KEY (listing, schema_name, object_name));
CREATE TABLE eggdb_marketplace.requests (request_id bigserial PRIMARY KEY, listing text NOT NULL REFERENCES eggdb_marketplace.listings (listing), consumer text NOT NULL DEFAULT current_user, purpose text NOT NULL DEFAULT '', accepted_terms boolean NOT NULL, requested_at timestamptz NOT NULL DEFAULT now(), state text NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'granted', 'refused', 'revoked')), decided_by text, decided_at timestamptz);
CREATE FUNCTION eggdb_marketplace.publish(listing_name text, listing_title text, objects text[], listing_description text DEFAULT '', listing_terms text DEFAULT '', approval_mode text DEFAULT 'manual') RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    qualified text;
    parts text[];
BEGIN
    IF coalesce(array_length(objects, 1), 0) = 0 THEN
        RAISE EXCEPTION 'a listing needs at least one table or view' USING ERRCODE = '22023';
    END IF;
    INSERT INTO eggdb_marketplace.listings (listing, title, description, terms, approval) VALUES (listing_name, listing_title, listing_description, listing_terms, approval_mode);
    FOREACH qualified IN ARRAY objects LOOP
        parts := string_to_array(qualified, '.');
        IF array_length(parts, 1) = 1 THEN
            parts := ARRAY['public', parts[1]];
        END IF;
        IF array_length(parts, 1) <> 2 OR NOT EXISTS (SELECT 1 FROM information_schema.tables t WHERE t.table_schema = parts[1] AND t.table_name = parts[2]) THEN
            RAISE EXCEPTION 'there is no table or view %', qualified USING ERRCODE = '42P01';
        END IF;
        INSERT INTO eggdb_marketplace.listing_objects (listing, schema_name, object_name) VALUES (listing_name, parts[1], parts[2]);
    END LOOP;
END
$$;
CREATE FUNCTION eggdb_marketplace.grant_listing(request bigint, granting boolean) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    asked record;
    shared record;
BEGIN
    SELECT r.listing, r.consumer INTO asked FROM eggdb_marketplace.requests r WHERE r.request_id = request;
    FOR shared IN SELECT o.schema_name, o.object_name FROM eggdb_marketplace.listing_objects o WHERE o.listing = asked.listing ORDER BY o.schema_name, o.object_name LOOP
        IF granting THEN
            EXECUTE format('GRANT USAGE ON SCHEMA %I TO %I', shared.schema_name, asked.consumer);
            EXECUTE format('GRANT SELECT ON %I.%I TO %I', shared.schema_name, shared.object_name, asked.consumer);
        ELSE
            EXECUTE format('REVOKE SELECT ON %I.%I FROM %I', shared.schema_name, shared.object_name, asked.consumer);
        END IF;
    END LOOP;
END
$$;
CREATE FUNCTION eggdb_marketplace.request_access(listing_name text, request_purpose text, accept_terms boolean) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    offered record;
    created bigint;
BEGIN
    SELECT l.approval, l.withdrawn_at INTO offered FROM eggdb_marketplace.listings l WHERE l.listing = listing_name;
    IF NOT FOUND OR offered.withdrawn_at IS NOT NULL THEN
        RAISE EXCEPTION 'listing % is not offered', listing_name USING ERRCODE = '42704';
    END IF;
    IF NOT accept_terms THEN
        RAISE EXCEPTION 'access to listing % needs its terms accepted', listing_name USING ERRCODE = '42501';
    END IF;
    IF EXISTS (SELECT 1 FROM eggdb_marketplace.requests r WHERE r.listing = listing_name AND r.consumer = current_user AND r.state IN ('pending', 'granted')) THEN
        RAISE EXCEPTION 'listing % is already requested or granted', listing_name USING ERRCODE = '42710';
    END IF;
    INSERT INTO eggdb_marketplace.requests (listing, purpose, accepted_terms) VALUES (listing_name, request_purpose, accept_terms) RETURNING requests.request_id INTO created;
    IF offered.approval = 'automatic' THEN
        PERFORM eggdb_marketplace.grant_listing(created, true);
        UPDATE eggdb_marketplace.requests SET state = 'granted', decided_by = current_user, decided_at = now() WHERE request_id = created;
    END IF;
    RETURN created;
END
$$;
CREATE FUNCTION eggdb_marketplace.decide(request bigint, approve boolean) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    asked record;
BEGIN
    SELECT r.state, l.provider INTO asked FROM eggdb_marketplace.requests r JOIN eggdb_marketplace.listings l ON l.listing = r.listing WHERE r.request_id = request;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'there is no request %', request USING ERRCODE = '42704';
    END IF;
    IF asked.provider <> current_user AND NOT coalesce((SELECT r.rolsuper FROM pg_catalog.pg_roles r WHERE r.rolname = current_user), false) THEN
        RAISE EXCEPTION 'only the listing''s provider decides its requests' USING ERRCODE = '42501';
    END IF;
    IF asked.state <> 'pending' THEN
        RAISE EXCEPTION 'request % is already %', request, asked.state USING ERRCODE = '55000';
    END IF;
    IF approve THEN
        PERFORM eggdb_marketplace.grant_listing(request, true);
    END IF;
    UPDATE eggdb_marketplace.requests SET state = CASE WHEN approve THEN 'granted' ELSE 'refused' END, decided_by = current_user, decided_at = now() WHERE request_id = request;
END
$$;
CREATE FUNCTION eggdb_marketplace.revoke_access(request bigint) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    asked record;
BEGIN
    SELECT r.state, l.provider INTO asked FROM eggdb_marketplace.requests r JOIN eggdb_marketplace.listings l ON l.listing = r.listing WHERE r.request_id = request;
    IF NOT FOUND OR asked.state <> 'granted' THEN
        RAISE EXCEPTION 'request % is not granted', request USING ERRCODE = '55000';
    END IF;
    IF asked.provider <> current_user AND NOT coalesce((SELECT r.rolsuper FROM pg_catalog.pg_roles r WHERE r.rolname = current_user), false) THEN
        RAISE EXCEPTION 'only the listing''s provider revokes its access' USING ERRCODE = '42501';
    END IF;
    PERFORM eggdb_marketplace.grant_listing(request, false);
    UPDATE eggdb_marketplace.requests SET state = 'revoked', decided_by = current_user, decided_at = now() WHERE request_id = request;
END
$$;
CREATE FUNCTION eggdb_marketplace.withdraw(listing_name text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    granted record;
    revoked bigint := 0;
BEGIN
    UPDATE eggdb_marketplace.listings SET withdrawn_at = now() WHERE listing = listing_name AND withdrawn_at IS NULL AND (provider = current_user OR coalesce((SELECT r.rolsuper FROM pg_catalog.pg_roles r WHERE r.rolname = current_user), false));
    IF NOT FOUND THEN
        RAISE EXCEPTION 'listing % is not offered by %', listing_name, current_user USING ERRCODE = '42501';
    END IF;
    FOR granted IN SELECT r.request_id FROM eggdb_marketplace.requests r WHERE r.listing = listing_name AND r.state = 'granted' ORDER BY r.request_id LOOP
        PERFORM eggdb_marketplace.revoke_access(granted.request_id);
        revoked := revoked + 1;
    END LOOP;
    UPDATE eggdb_marketplace.requests SET state = 'refused', decided_by = current_user, decided_at = now() WHERE listing = listing_name AND state = 'pending';
    RETURN revoked;
END
$$;
CREATE VIEW eggdb_marketplace.catalog AS
    SELECT l.listing, l.title, l.description, l.provider, l.terms, l.approval, l.published_at,
           (SELECT string_agg(o.schema_name || '.' || o.object_name, ', ' ORDER BY o.schema_name, o.object_name) FROM eggdb_marketplace.listing_objects o WHERE o.listing = l.listing) AS objects,
           (SELECT count(*) FROM eggdb_marketplace.requests r WHERE r.listing = l.listing AND r.state = 'granted') AS subscribers
    FROM eggdb_marketplace.listings l WHERE l.withdrawn_at IS NULL;
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
    fn listings_are_requested_granted_and_withdrawn() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_marketplace",
            "CREATE TABLE prices (sku text, price numeric)",
            "CREATE ROLE analyst LOGIN",
            "SELECT eggdb_marketplace.publish('egg-prices', 'Daily egg prices', ARRAY['public.prices'], 'Wholesale prices by SKU', 'Internal use only', 'automatic')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT listing, objects, subscribers FROM eggdb_marketplace.catalog").unwrap(),
            "listing|objects|subscribers\negg-prices|public.prices|0\nSELECT 1"
        );
        assert_eq!(run(&mut session, "SELECT eggdb_marketplace.request_access('egg-prices', 'pricing model', false)").unwrap_err().state().code(), "42501");
        assert_eq!(run(&mut session, "SELECT eggdb_marketplace.publish('nothing', 'Nothing', ARRAY['public.missing'])").unwrap_err().state().code(), "42P01");
        run(&mut session, "INSERT INTO eggdb_marketplace.requests (listing, consumer, purpose, accepted_terms) VALUES ('egg-prices', 'analyst', 'pricing model', true)").unwrap();
        run(&mut session, "SELECT eggdb_marketplace.decide(1, true)").unwrap();
        assert_eq!(
            run(&mut session, "SELECT has_table_privilege('analyst', 'public.prices', 'SELECT') AS can_read").unwrap(),
            "can_read\nt\nSELECT 1"
        );
        assert_eq!(run(&mut session, "SELECT eggdb_marketplace.withdraw('egg-prices') AS revoked").unwrap(), "revoked\n1\nSELECT 1");
        assert_eq!(
            run(&mut session, "SELECT has_table_privilege('analyst', 'public.prices', 'SELECT') AS can_read").unwrap(),
            "can_read\nf\nSELECT 1"
        );
    }
}
