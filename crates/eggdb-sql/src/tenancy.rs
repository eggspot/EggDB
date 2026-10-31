pub(crate) const TENANCY_SCRIPT: &str = r"
CREATE SCHEMA eggdb_tenancy;
CREATE TABLE eggdb_tenancy.tenants (
    tenant text PRIMARY KEY,
    max_rows bigint,
    created_at timestamptz NOT NULL DEFAULT now()
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
BEGIN
    SELECT i.tenant_column INTO column_name FROM eggdb_tenancy.isolated i WHERE i.table_schema = TG_TABLE_SCHEMA AND i.table_name = TG_TABLE_NAME;
    tenant_value := to_jsonb(NEW) ->> column_name;
    SELECT t.max_rows INTO allowed FROM eggdb_tenancy.tenants t WHERE t.tenant = tenant_value;
    IF allowed IS NOT NULL THEN
        EXECUTE format('SELECT count(*) FROM %I.%I WHERE %I = $1', TG_TABLE_SCHEMA, TG_TABLE_NAME, column_name) INTO used USING tenant_value;
        IF used >= allowed THEN
            RAISE EXCEPTION 'tenant % has reached its quota of % rows in %', tenant_value, allowed, TG_TABLE_NAME USING ERRCODE = '53400';
        END IF;
    END IF;
    RETURN NEW;
END
$$;
CREATE FUNCTION eggdb_tenancy.isolate(schema_name text, table_name text, tenant_column text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    EXECUTE format('ALTER TABLE %I.%I ENABLE ROW LEVEL SECURITY', schema_name, table_name);
    EXECUTE format('ALTER TABLE %I.%I FORCE ROW LEVEL SECURITY', schema_name, table_name);
    EXECUTE format('CREATE POLICY eggdb_tenant ON %I.%I USING (%I = current_setting(%L)) WITH CHECK (%I = current_setting(%L))', schema_name, table_name, tenant_column, 'eggdb.tenant', tenant_column, 'eggdb.tenant');
    EXECUTE format('CREATE TRIGGER eggdb_tenant_quota BEFORE INSERT ON %I.%I FOR EACH ROW EXECUTE FUNCTION eggdb_tenancy.enforce_quota()', schema_name, table_name);
    INSERT INTO eggdb_tenancy.isolated (table_schema, table_name, tenant_column) VALUES (schema_name, table_name, tenant_column);
END
$$;
";

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::tests::render;
    use crate::{Session, SqlError};

    fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<String, SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(render(&result)))?;
        Ok(outputs.pop().unwrap_or_default())
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
            "GRANT USAGE ON SEQUENCE notes_id_seq TO app",
            "GRANT USAGE ON SCHEMA eggdb_tenancy TO app",
            "GRANT SELECT ON eggdb_tenancy.tenants, eggdb_tenancy.isolated TO app",
            "SET ROLE app",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert!(run(&mut session, "SELECT count(*) FROM notes").is_err(), "no tenant chosen must fail closed");
        run(&mut session, "SET eggdb.tenant = 'acme'").unwrap();
        run(&mut session, "INSERT INTO notes (tenant, body) VALUES ('acme', 'one'), ('acme', 'two')").unwrap();
        assert_eq!(run(&mut session, "INSERT INTO notes (tenant, body) VALUES ('acme', 'three')").unwrap_err().state().code(), "53400");
        assert!(run(&mut session, "INSERT INTO notes (tenant, body) VALUES ('globex', 'sneaky')").is_err());
        assert_eq!(run(&mut session, "SELECT count(*) AS n FROM notes").unwrap(), "n\n2\nSELECT 1");
        run(&mut session, "SET eggdb.tenant = 'globex'").unwrap();
        assert_eq!(run(&mut session, "SELECT count(*) AS n FROM notes").unwrap(), "n\n0\nSELECT 1");
        run(&mut session, "INSERT INTO notes (tenant, body) VALUES ('globex', 'mine')").unwrap();
        assert_eq!(run(&mut session, "SELECT body FROM notes").unwrap(), "body\nmine\nSELECT 1");
    }
}
