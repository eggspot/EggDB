pub(crate) const TENANCY_SCRIPT: &str = r"
CREATE SCHEMA eggdb_tenancy;
CREATE TABLE eggdb_tenancy.tenants (
    tenant text PRIMARY KEY,
    max_rows bigint,
    created_at timestamptz NOT NULL DEFAULT now(),
    max_bytes bigint
);
CREATE TABLE eggdb_tenancy.isolated (
    table_schema text NOT NULL,
    table_name text NOT NULL,
    tenant_column text NOT NULL,
    PRIMARY KEY (table_schema, table_name)
);
CREATE FUNCTION eggdb_tenancy.enforce_quota() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    column_name text;
    tenant_value text;
    allowed bigint;
    used bigint;
    allowed_bytes bigint;
    used_bytes bigint;
    added_bytes bigint;
BEGIN
    SELECT i.tenant_column INTO column_name FROM eggdb_tenancy.isolated i WHERE i.table_schema = TG_TABLE_SCHEMA AND i.table_name = TG_TABLE_NAME;
    tenant_value := to_jsonb(NEW) ->> column_name;
    IF NOT coalesce((SELECT r.rolsuper FROM pg_catalog.pg_roles r WHERE r.rolname = current_user), false)
        AND tenant_value IS DISTINCT FROM current_setting('eggdb.tenant', true) THEN
        RAISE EXCEPTION 'new row violates row-level security policy for table %', TG_TABLE_NAME USING ERRCODE = '42501';
    END IF;
    SELECT t.max_bytes INTO allowed_bytes FROM eggdb_tenancy.tenants t WHERE t.tenant = tenant_value;
    IF allowed_bytes IS NOT NULL THEN
        added_bytes := octet_length(to_jsonb(NEW)::text);
        IF TG_OP = 'UPDATE' AND tenant_value IS NOT DISTINCT FROM to_jsonb(OLD) ->> column_name THEN
            added_bytes := added_bytes - octet_length(to_jsonb(OLD)::text);
        END IF;
        IF added_bytes > 0 THEN
            EXECUTE format('SELECT coalesce(sum(octet_length(to_jsonb(r)::text)), 0) FROM %I.%I r WHERE %I = $1', TG_TABLE_SCHEMA, TG_TABLE_NAME, column_name) INTO used_bytes USING tenant_value;
            IF used_bytes + added_bytes > allowed_bytes THEN
                RAISE EXCEPTION 'tenant % has reached its quota of % bytes in %', tenant_value, allowed_bytes, TG_TABLE_NAME USING ERRCODE = '53400';
            END IF;
        END IF;
    END IF;
    SELECT t.max_rows INTO allowed FROM eggdb_tenancy.tenants t WHERE t.tenant = tenant_value;
    IF allowed IS NOT NULL AND (TG_OP = 'INSERT' OR tenant_value IS DISTINCT FROM to_jsonb(OLD) ->> column_name) THEN
        EXECUTE format('SELECT count(*) FROM %I.%I WHERE %I = $1', TG_TABLE_SCHEMA, TG_TABLE_NAME, column_name) INTO used USING tenant_value;
        IF used >= allowed THEN
            RAISE EXCEPTION 'tenant % has reached its quota of % rows in %', tenant_value, allowed, TG_TABLE_NAME USING ERRCODE = '53400';
        END IF;
    END IF;
    RETURN NEW;
END
$$;
CREATE FUNCTION eggdb_tenancy.refuse_truncate() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT coalesce((SELECT r.rolsuper FROM pg_catalog.pg_roles r WHERE r.rolname = current_user), false) THEN
        RAISE EXCEPTION 'table % holds the rows of every tenant: only a superuser may truncate it', TG_TABLE_NAME USING ERRCODE = '42501', HINT = 'Delete the rows of your tenant instead.';
    END IF;
    RETURN NULL;
END
$$;
CREATE FUNCTION eggdb_tenancy.isolate(schema_name text, table_name text, tenant_column text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    EXECUTE format('ALTER TABLE %I.%I ENABLE ROW LEVEL SECURITY', schema_name, table_name);
    EXECUTE format('CREATE POLICY eggdb_tenant ON %I.%I USING (%I = current_setting(%L, true)) WITH CHECK (true)', schema_name, table_name, tenant_column, 'eggdb.tenant');
    EXECUTE format('CREATE TRIGGER eggdb_tenant_quota BEFORE INSERT OR UPDATE ON %I.%I FOR EACH ROW EXECUTE FUNCTION eggdb_tenancy.enforce_quota()', schema_name, table_name);
    EXECUTE format('CREATE TRIGGER eggdb_tenant_truncate BEFORE TRUNCATE ON %I.%I FOR EACH STATEMENT EXECUTE FUNCTION eggdb_tenancy.refuse_truncate()', schema_name, table_name);
    INSERT INTO eggdb_tenancy.isolated (table_schema, table_name, tenant_column) VALUES (schema_name, table_name, tenant_column);
END
$$;
CREATE TABLE eggdb_tenancy.tenant_schemas (
    tenant text PRIMARY KEY,
    schema_name text NOT NULL UNIQUE,
    template_schema text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE FUNCTION eggdb_tenancy.copy_table(template_schema text, table_name text, target_schema text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    found_column record;
    found_constraint record;
    key_columns text;
    sequence_name text;
BEGIN
    EXECUTE format('CREATE TABLE %I.%I AS SELECT * FROM %I.%I WHERE false', target_schema, table_name, template_schema, table_name);
    FOR found_column IN SELECT c.column_name AS name, c.is_nullable AS nullable, c.column_default AS default_value FROM information_schema.columns c
            WHERE c.table_schema = copy_table.template_schema AND c.table_name = copy_table.table_name ORDER BY c.ordinal_position LOOP
        IF found_column.nullable = 'NO' THEN
            EXECUTE format('ALTER TABLE %I.%I ALTER COLUMN %I SET NOT NULL', target_schema, table_name, found_column.name);
        END IF;
        IF found_column.default_value LIKE 'nextval(%' THEN
            sequence_name := table_name || '_' || found_column.name || '_seq';
            EXECUTE format('CREATE SEQUENCE %I.%I', target_schema, sequence_name);
            EXECUTE format('ALTER TABLE %I.%I ALTER COLUMN %I SET DEFAULT nextval(%L)', target_schema, table_name, found_column.name, quote_ident(target_schema) || '.' || quote_ident(sequence_name));
            EXECUTE format('GRANT USAGE, SELECT ON SEQUENCE %I.%I TO %I', target_schema, sequence_name, target_schema);
        ELSIF found_column.default_value IS NOT NULL THEN
            EXECUTE format('ALTER TABLE %I.%I ALTER COLUMN %I SET DEFAULT %s', target_schema, table_name, found_column.name, found_column.default_value);
        END IF;
    END LOOP;
    FOR found_constraint IN SELECT tc.constraint_name AS name, tc.constraint_type AS kind FROM information_schema.table_constraints tc
            WHERE tc.table_schema = copy_table.template_schema AND tc.table_name = copy_table.table_name AND tc.constraint_type IN ('PRIMARY KEY', 'UNIQUE')
            ORDER BY tc.constraint_type, tc.constraint_name LOOP
        SELECT string_agg(quote_ident(k.column_name), ', ' ORDER BY k.ordinal_position) INTO key_columns FROM information_schema.key_column_usage k
            WHERE k.constraint_schema = copy_table.template_schema AND k.constraint_name = found_constraint.name AND k.table_name = copy_table.table_name;
        EXECUTE format('ALTER TABLE %I.%I ADD %s (%s)', target_schema, table_name, found_constraint.kind, key_columns);
    END LOOP;
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I.%I TO %I', target_schema, table_name, target_schema);
END
$$;
CREATE FUNCTION eggdb_tenancy.create_tenant_schema(tenant text, template_schema text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    found_table record;
    copied bigint := 0;
BEGIN
    IF EXISTS (SELECT 1 FROM eggdb_tenancy.tenant_schemas s WHERE s.tenant = create_tenant_schema.tenant) THEN
        RAISE EXCEPTION 'tenant % already has a schema', tenant USING ERRCODE = '42710';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles r WHERE r.rolname = create_tenant_schema.tenant) THEN
        EXECUTE format('CREATE ROLE %I', tenant);
    END IF;
    EXECUTE format('CREATE SCHEMA %I', tenant);
    EXECUTE format('REVOKE ALL ON SCHEMA %I FROM PUBLIC', tenant);
    EXECUTE format('GRANT USAGE ON SCHEMA %I TO %I', tenant, tenant);
    FOR found_table IN SELECT t.table_name AS name FROM information_schema.tables t WHERE t.table_schema = create_tenant_schema.template_schema AND t.table_type = 'BASE TABLE' ORDER BY t.table_name LOOP
        PERFORM eggdb_tenancy.copy_table(template_schema, found_table.name, tenant);
        copied := copied + 1;
    END LOOP;
    INSERT INTO eggdb_tenancy.tenant_schemas (tenant, schema_name, template_schema) VALUES (create_tenant_schema.tenant, create_tenant_schema.tenant, create_tenant_schema.template_schema);
    RETURN copied;
END
$$;
CREATE FUNCTION eggdb_tenancy.upgrade_tenant_schemas(template_schema text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    tenant_schema record;
    found_table record;
    missing record;
    column_type text;
    changes bigint := 0;
BEGIN
    FOR tenant_schema IN SELECT s.schema_name AS name FROM eggdb_tenancy.tenant_schemas s WHERE s.template_schema = upgrade_tenant_schemas.template_schema ORDER BY s.tenant LOOP
        FOR found_table IN SELECT t.table_name AS name FROM information_schema.tables t WHERE t.table_schema = upgrade_tenant_schemas.template_schema AND t.table_type = 'BASE TABLE' ORDER BY t.table_name LOOP
            IF NOT EXISTS (SELECT 1 FROM information_schema.tables t WHERE t.table_schema = tenant_schema.name AND t.table_name = found_table.name) THEN
                PERFORM eggdb_tenancy.copy_table(template_schema, found_table.name, tenant_schema.name);
                changes := changes + 1;
                CONTINUE;
            END IF;
            FOR missing IN SELECT c.column_name, c.data_type, c.character_maximum_length, c.numeric_precision, c.numeric_scale, c.is_nullable, c.column_default
                    FROM information_schema.columns c
                    WHERE c.table_schema = upgrade_tenant_schemas.template_schema AND c.table_name = found_table.name
                      AND NOT EXISTS (SELECT 1 FROM information_schema.columns d WHERE d.table_schema = tenant_schema.name AND d.table_name = found_table.name AND d.column_name = c.column_name)
                    ORDER BY c.ordinal_position LOOP
                column_type := CASE missing.data_type
                    WHEN 'character varying' THEN coalesce('varchar(' || missing.character_maximum_length || ')', 'varchar')
                    WHEN 'character' THEN 'char(' || coalesce(missing.character_maximum_length, 1) || ')'
                    WHEN 'numeric' THEN coalesce('numeric(' || missing.numeric_precision || ', ' || missing.numeric_scale || ')', 'numeric')
                    WHEN 'ARRAY' THEN NULL
                    WHEN 'USER-DEFINED' THEN NULL
                    ELSE missing.data_type
                END;
                IF column_type IS NULL OR missing.column_default LIKE 'nextval(%' THEN
                    RAISE EXCEPTION 'column %.% cannot be copied to the tenant schemas by upgrade_tenant_schemas: add it by hand', found_table.name, missing.column_name USING ERRCODE = '0A000';
                END IF;
                EXECUTE format('ALTER TABLE %I.%I ADD COLUMN %I %s%s%s', tenant_schema.name, found_table.name, missing.column_name, column_type,
                    CASE WHEN missing.column_default IS NULL THEN '' ELSE ' DEFAULT ' || missing.column_default END,
                    CASE WHEN missing.is_nullable = 'NO' THEN ' NOT NULL' ELSE '' END);
                changes := changes + 1;
            END LOOP;
        END LOOP;
    END LOOP;
    RETURN changes;
END
$$;
CREATE FUNCTION eggdb_tenancy.backup_tenant(tenant text) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE
    found_table record;
    rows jsonb;
    tables jsonb := '{}'::jsonb;
    own_schema text;
BEGIN
    FOR found_table IN SELECT i.table_schema AS schema_name, i.table_name, i.tenant_column FROM eggdb_tenancy.isolated i ORDER BY i.table_schema, i.table_name LOOP
        EXECUTE format('SELECT coalesce(jsonb_agg(to_jsonb(t)), %L::jsonb) FROM %I.%I t WHERE t.%I = %L', '[]', found_table.schema_name, found_table.table_name, found_table.tenant_column, tenant) INTO rows;
        tables := tables || jsonb_build_object(found_table.schema_name || '.' || found_table.table_name, rows);
    END LOOP;
    SELECT s.schema_name INTO own_schema FROM eggdb_tenancy.tenant_schemas s WHERE s.tenant = backup_tenant.tenant;
    IF own_schema IS NOT NULL THEN
        FOR found_table IN SELECT t.table_name FROM information_schema.tables t WHERE t.table_schema = own_schema AND t.table_type = 'BASE TABLE' ORDER BY t.table_name LOOP
            EXECUTE format('SELECT coalesce(jsonb_agg(to_jsonb(t)), %L::jsonb) FROM %I.%I t', '[]', own_schema, found_table.table_name) INTO rows;
            tables := tables || jsonb_build_object(own_schema || '.' || found_table.table_name, rows);
        END LOOP;
    END IF;
    RETURN jsonb_build_object('format', 'eggdb-tenant-backup-1', 'tenant', tenant, 'taken_at', now(), 'schema', own_schema, 'tables', tables);
END
$$;
CREATE FUNCTION eggdb_tenancy.restore_tenant(tenant text, backup jsonb) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    entry record;
    found_schema text;
    found_table text;
    tenant_column text;
    own_schema text;
    restored bigint := 0;
    added bigint;
BEGIN
    IF backup ->> 'format' IS DISTINCT FROM 'eggdb-tenant-backup-1' THEN
        RAISE EXCEPTION 'this is not an EggDB tenant backup' USING ERRCODE = '22023';
    END IF;
    IF backup ->> 'tenant' IS DISTINCT FROM tenant THEN
        RAISE EXCEPTION 'the backup is of tenant %, not %', backup ->> 'tenant', tenant USING ERRCODE = '22023';
    END IF;
    SELECT s.schema_name INTO own_schema FROM eggdb_tenancy.tenant_schemas s WHERE s.tenant = restore_tenant.tenant;
    FOR entry IN SELECT b.key AS qualified, b.value AS rows FROM jsonb_each(backup -> 'tables') b ORDER BY b.key LOOP
        found_schema := split_part(entry.qualified, '.', 1);
        found_table := substr(entry.qualified, length(found_schema) + 2);
        SELECT i.tenant_column INTO tenant_column FROM eggdb_tenancy.isolated i WHERE i.table_schema = found_schema AND i.table_name = found_table;
        IF tenant_column IS NOT NULL THEN
            IF EXISTS (SELECT 1 FROM jsonb_array_elements(entry.rows) r WHERE r.value ->> tenant_column IS DISTINCT FROM tenant) THEN
                RAISE EXCEPTION 'the backup of %.% holds rows of another tenant', found_schema, found_table USING ERRCODE = '22023';
            END IF;
            EXECUTE format('DELETE FROM %I.%I WHERE %I = %L', found_schema, found_table, tenant_column, tenant);
        ELSIF found_schema = own_schema THEN
            EXECUTE format('DELETE FROM %I.%I', found_schema, found_table);
        ELSE
            RAISE EXCEPTION 'table %.% belongs neither to an isolated table nor to the tenant''s schema', found_schema, found_table USING ERRCODE = '42501';
        END IF;
        EXECUTE format('INSERT INTO %I.%I SELECT * FROM jsonb_populate_recordset(NULL::%I.%I, %L::jsonb)', found_schema, found_table, found_schema, found_table, entry.rows::text);
        GET DIAGNOSTICS added = ROW_COUNT;
        restored := restored + added;
    END LOOP;
    RETURN restored;
END
$$;
CREATE FUNCTION eggdb_tenancy.drop_tenant_schema(tenant text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    found_schema text;
BEGIN
    SELECT s.schema_name INTO found_schema FROM eggdb_tenancy.tenant_schemas s WHERE s.tenant = drop_tenant_schema.tenant;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'tenant % has no schema', tenant USING ERRCODE = '42704';
    END IF;
    EXECUTE format('DROP SCHEMA %I CASCADE', found_schema);
    DELETE FROM eggdb_tenancy.tenant_schemas s WHERE s.tenant = drop_tenant_schema.tenant;
END
$$;
";

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
    fn cascading_foreign_keys_change_isolated_tables() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_tenancy",
            "CREATE TABLE projects (id integer PRIMARY KEY, tenant text NOT NULL)",
            "CREATE TABLE tasks (id integer PRIMARY KEY, tenant text NOT NULL, project_id integer REFERENCES projects ON DELETE CASCADE ON UPDATE CASCADE)",
            "SELECT eggdb_tenancy.isolate('public', 'projects', 'tenant')",
            "SELECT eggdb_tenancy.isolate('public', 'tasks', 'tenant')",
            "INSERT INTO eggdb_tenancy.tenants (tenant, max_rows) VALUES ('acme', 10)",
            "CREATE ROLE app LOGIN",
            "GRANT ALL ON projects, tasks TO app",
            "GRANT SELECT ON eggdb_tenancy.tenants, eggdb_tenancy.isolated TO app",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        drop(session);
        crate::memory_store::use_seeded_random_bytes();
        let mut session = Session::new(&store);
        session.identify_role("app", "eggdb").unwrap();
        for sql in [
            "SET eggdb.tenant = 'acme'",
            "INSERT INTO projects VALUES (1, 'acme'), (2, 'acme')",
            "INSERT INTO tasks VALUES (10, 'acme', 1), (20, 'acme', 2)",
            "UPDATE projects SET id = 3 WHERE id = 2",
            "DELETE FROM projects WHERE id = 1",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT id, project_id FROM tasks").unwrap(),
            "id|project_id\n20|3\nSELECT 1"
        );
    }

    #[test]
    fn a_tenant_s_bytes_stay_within_its_byte_quota() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_tenancy",
            "CREATE TABLE notes (id integer PRIMARY KEY, tenant text NOT NULL, body text)",
            "SELECT eggdb_tenancy.isolate('public', 'notes', 'tenant')",
            "INSERT INTO eggdb_tenancy.tenants (tenant, max_bytes) VALUES ('acme', 120)",
            "SET eggdb.tenant = 'acme'",
            "INSERT INTO notes VALUES (1, 'acme', 'short')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        let refused = run(&mut session, "INSERT INTO notes VALUES (2, 'acme', 'a body long enough to pass the quota of bytes')").unwrap_err();
        assert_eq!(refused.state().code(), "53400");
        run(&mut session, "UPDATE notes SET body = 'tiny' WHERE id = 1").unwrap();
    }

    #[test]
    fn each_tenant_sees_and_writes_only_its_own_rows_within_its_quota() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_tenancy",
            "CREATE TABLE notes (id serial PRIMARY KEY, tenant text NOT NULL, body text)",
            "SELECT eggdb_tenancy.isolate('public', 'notes', 'tenant')",
            "INSERT INTO eggdb_tenancy.tenants (tenant, max_rows) VALUES ('acme', 2)",
            "CREATE ROLE app LOGIN",
            "GRANT ALL ON notes TO app",
            "GRANT SELECT ON eggdb_tenancy.tenants, eggdb_tenancy.isolated TO app",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        drop(session);
        crate::memory_store::use_seeded_random_bytes();
        let mut session = Session::new(&store);
        session.identify_role("app", "eggdb").unwrap();
        assert_eq!(
            run(&mut session, "SELECT count(*) AS n FROM notes").unwrap(),
            "n\n0\nSELECT 1",
            "no tenant chosen must fail closed"
        );
        assert!(
            run(
                &mut session,
                "INSERT INTO notes (tenant, body) VALUES ('acme', 'early')"
            )
            .is_err(),
            "no tenant chosen must fail closed"
        );
        run(&mut session, "SET eggdb.tenant = 'acme'").unwrap();
        run(
            &mut session,
            "INSERT INTO notes (tenant, body) VALUES ('acme', 'one'), ('acme', 'two')",
        )
        .unwrap();
        assert_eq!(
            run(
                &mut session,
                "INSERT INTO notes (tenant, body) VALUES ('acme', 'three')"
            )
            .unwrap_err()
            .state()
            .code(),
            "53400"
        );
        assert!(
            run(
                &mut session,
                "INSERT INTO notes (tenant, body) VALUES ('globex', 'sneaky')"
            )
            .is_err()
        );
        assert_eq!(
            run(&mut session, "SELECT count(*) AS n FROM notes").unwrap(),
            "n\n2\nSELECT 1"
        );
        run(&mut session, "SET eggdb.tenant = 'globex'").unwrap();
        assert_eq!(
            run(&mut session, "SELECT count(*) AS n FROM notes").unwrap(),
            "n\n0\nSELECT 1"
        );
        run(
            &mut session,
            "INSERT INTO notes (tenant, body) VALUES ('globex', 'mine')",
        )
        .unwrap();
        assert_eq!(
            run(&mut session, "SELECT body FROM notes").unwrap(),
            "body\nmine\nSELECT 1"
        );
        assert!(
            run(&mut session, "UPDATE notes SET tenant = 'acme'").is_err(),
            "a tenant cannot move its rows into another tenant"
        );
        assert!(
            run(
                &mut session,
                "INSERT INTO notes (id, tenant, body) VALUES (1, 'globex', 'x') ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body"
            )
            .is_err(),
            "an upsert must not reach another tenant's row"
        );
        assert!(
            run(&mut session, "COPY notes (tenant, body) FROM STDIN").is_err(),
            "COPY FROM must not get around the tenant check"
        );
        assert_eq!(
            run(&mut session, "TRUNCATE notes")
                .unwrap_err()
                .state()
                .code(),
            "42501",
            "TRUNCATE must not wipe the other tenants' rows"
        );
        run(&mut session, "SET eggdb.tenant = 'acme'").unwrap();
        assert_eq!(
            run(&mut session, "SELECT body FROM notes ORDER BY id").unwrap(),
            "body\none\ntwo\nSELECT 2"
        );
    }

    #[test]
    fn tenant_schemas_copy_the_template_and_follow_its_changes() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_tenancy",
            "CREATE SCHEMA app_template",
            "CREATE TABLE app_template.notes (id serial PRIMARY KEY, body text NOT NULL)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT eggdb_tenancy.create_tenant_schema('acme', 'app_template') AS tables").unwrap(), "tables\n1\nSELECT 1");
        run(&mut session, "INSERT INTO acme.notes (body) VALUES ('first')").unwrap();
        assert_eq!(run(&mut session, "INSERT INTO acme.notes (id, body) VALUES (1, 'again')").unwrap_err().state().code(), "23505");
        run(&mut session, "ALTER TABLE app_template.notes ADD COLUMN pinned boolean NOT NULL DEFAULT false").unwrap();
        assert_eq!(run(&mut session, "SELECT eggdb_tenancy.upgrade_tenant_schemas('app_template') AS changes").unwrap(), "changes\n1\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT id, body, pinned FROM acme.notes").unwrap(), "id|body|pinned\n1|first|f\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT count(*) AS roles FROM pg_catalog.pg_roles WHERE rolname = 'acme'").unwrap(), "roles\n1\nSELECT 1");
        run(&mut session, "SELECT eggdb_tenancy.drop_tenant_schema('acme')").unwrap();
        assert!(run(&mut session, "SELECT * FROM acme.notes").is_err());
    }

    #[test]
    fn a_tenant_is_backed_up_and_restored_alone() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_tenancy",
            "CREATE TABLE notes (id integer PRIMARY KEY, tenant text NOT NULL, body text)",
            "SELECT eggdb_tenancy.isolate('public', 'notes', 'tenant')",
            "INSERT INTO notes VALUES (1, 'acme', 'one'), (2, 'acme', 'two'), (3, 'globex', 'theirs')",
            "CREATE TABLE saved (backup jsonb)",
            "INSERT INTO saved SELECT eggdb_tenancy.backup_tenant('acme')",
            "DELETE FROM notes WHERE id = 2",
            "UPDATE notes SET body = 'changed' WHERE id = 1",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT eggdb_tenancy.restore_tenant('acme', backup) AS restored FROM saved").unwrap(), "restored\n2\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT id, body FROM notes ORDER BY id").unwrap(), "id|body\n1|one\n2|two\n3|theirs\nSELECT 3");
        assert_eq!(run(&mut session, "SELECT eggdb_tenancy.restore_tenant('globex', backup) FROM saved").unwrap_err().state().code(), "22023");
    }
}
