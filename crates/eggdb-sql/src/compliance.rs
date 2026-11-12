/// The statements `CREATE EXTENSION eggdb_compliance` runs: evidence for SOC 2, HIPAA and PCI
/// DSS audits, gathered from the catalog and EggDB's own governance extensions and mapped to the
/// controls auditors ask about, kept per run (D26).
pub(crate) const COMPLIANCE_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_compliance;
CREATE TABLE eggdb_compliance.controls (framework text NOT NULL CHECK (framework IN ('SOC 2', 'HIPAA', 'PCI DSS')), control text NOT NULL, title text NOT NULL, check_name text NOT NULL, PRIMARY KEY (framework, control, check_name));
INSERT INTO eggdb_compliance.controls VALUES
    ('SOC 2', 'CC6.1', 'Logical access security', 'public_grants'),
    ('SOC 2', 'CC6.1', 'Logical access security', 'login_expiry'),
    ('SOC 2', 'CC6.3', 'Role-based access and least privilege', 'privileged_roles'),
    ('SOC 2', 'C1.1', 'Confidential information is identified', 'personal_data_inventory'),
    ('SOC 2', 'C1.2', 'Confidential information is disposed of', 'retention'),
    ('HIPAA', '164.312(a)(1)', 'Access control', 'privileged_roles'),
    ('HIPAA', '164.312(a)(1)', 'Access control', 'public_grants'),
    ('HIPAA', '164.312(c)(1)', 'Integrity', 'tamper_evidence'),
    ('HIPAA', '164.308(a)(1)(ii)(A)', 'Risk analysis', 'personal_data_inventory'),
    ('PCI DSS', '7.2.1', 'An access control model is defined', 'public_grants'),
    ('PCI DSS', '7.2.2', 'Access is assigned by job classification and least privilege', 'privileged_roles'),
    ('PCI DSS', '8.3.9', 'Passwords used alone are changed at least every 90 days', 'login_expiry'),
    ('PCI DSS', '3.2.1', 'Account data storage is kept to a minimum by retention', 'retention'),
    ('PCI DSS', '10.3.2', 'Audit records are protected from modification', 'tamper_evidence'),
    ('PCI DSS', '12.5.2', 'The scope, including where account data is stored, is documented', 'personal_data_inventory');
CREATE TABLE eggdb_compliance.runs (run_id bigserial PRIMARY KEY, run_at timestamptz NOT NULL DEFAULT now(), run_by text NOT NULL DEFAULT current_user, database_name text NOT NULL DEFAULT current_database());
CREATE TABLE eggdb_compliance.results (run_id bigint NOT NULL REFERENCES eggdb_compliance.runs (run_id), check_name text NOT NULL, status text NOT NULL CHECK (status IN ('pass', 'review', 'fail')), evidence jsonb NOT NULL, PRIMARY KEY (run_id, check_name));
CREATE FUNCTION eggdb_compliance.has_table(schema_name text, table_name text) RETURNS boolean LANGUAGE plpgsql AS $$
BEGIN
    RETURN EXISTS (SELECT 1 FROM information_schema.tables t WHERE t.table_schema = has_table.schema_name AND t.table_name = has_table.table_name);
END
$$;
CREATE FUNCTION eggdb_compliance.collect() RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    current_run bigint;
    listed jsonb;
    counted bigint;
    sensitive bigint;
    broken bigint;
    chain record;
    checked bigint := 0;
    pending bigint;
BEGIN
    INSERT INTO eggdb_compliance.runs DEFAULT VALUES RETURNING runs.run_id INTO current_run;
    SELECT coalesce(jsonb_agg(jsonb_build_object('role', r.rolname, 'superuser', r.rolsuper, 'create_role', r.rolcreaterole, 'bypass_rls', r.rolbypassrls) ORDER BY r.rolname), '[]'::jsonb)
        INTO listed FROM pg_catalog.pg_roles r WHERE r.rolsuper OR r.rolcreaterole OR r.rolbypassrls;
    INSERT INTO eggdb_compliance.results VALUES (current_run, 'privileged_roles', 'review', jsonb_build_object('roles', listed));
    SELECT coalesce(jsonb_agg(jsonb_build_object('role', r.rolname, 'valid_until', r.rolvaliduntil) ORDER BY r.rolname), '[]'::jsonb), count(*)
        INTO listed, counted FROM pg_catalog.pg_roles r WHERE r.rolcanlogin AND (r.rolvaliduntil IS NULL OR r.rolvaliduntil > now() + interval '90 days');
    INSERT INTO eggdb_compliance.results VALUES (current_run, 'login_expiry', CASE WHEN counted = 0 THEN 'pass' ELSE 'review' END,
        jsonb_build_object('login_roles_without_expiry_within_90_days', listed));
    SELECT coalesce(jsonb_agg(jsonb_build_object('table', p.table_schema || '.' || p.table_name, 'privilege', p.privilege_type) ORDER BY p.table_schema, p.table_name, p.privilege_type), '[]'::jsonb), count(*)
        INTO listed, counted FROM information_schema.table_privileges p
        WHERE p.grantee = 'PUBLIC' AND p.table_schema NOT IN ('pg_catalog', 'information_schema');
    sensitive := 0;
    IF counted > 0 AND eggdb_compliance.has_table('eggdb_privacy', 'classifications') THEN
        EXECUTE 'SELECT count(*) FROM information_schema.table_privileges p WHERE p.grantee = ''PUBLIC'' AND EXISTS (SELECT 1 FROM eggdb_privacy.classifications c WHERE c.schema_name = p.table_schema AND c.table_name = p.table_name)' INTO sensitive;
    END IF;
    IF counted > 0 AND eggdb_compliance.has_table('eggdb_catalog', 'tags') THEN
        EXECUTE format('SELECT %s + count(*) FROM information_schema.table_privileges p WHERE p.grantee = %L AND EXISTS (SELECT 1 FROM eggdb_catalog.tags g WHERE g.schema_name = p.table_schema AND g.table_name = p.table_name)', sensitive, 'PUBLIC') INTO sensitive;
    END IF;
    INSERT INTO eggdb_compliance.results VALUES (current_run, 'public_grants', CASE WHEN counted = 0 THEN 'pass' WHEN sensitive > 0 THEN 'fail' ELSE 'review' END,
        jsonb_build_object('granted_to_public', listed, 'on_classified_tables', sensitive));
    listed := '{}'::jsonb;
    IF eggdb_compliance.has_table('eggdb_privacy', 'classifications') THEN
        EXECUTE 'SELECT jsonb_build_object(''classified_columns'', coalesce(jsonb_agg(jsonb_build_object(''column'', c.schema_name || ''.'' || c.table_name || ''.'' || c.column_name, ''category'', c.category) ORDER BY c.schema_name, c.table_name, c.column_name, c.category), ''[]''::jsonb)) FROM eggdb_privacy.classifications c' INTO listed;
    END IF;
    IF eggdb_compliance.has_table('eggdb_catalog', 'tags') THEN
        EXECUTE format('SELECT %L::jsonb || jsonb_build_object(''tagged'', coalesce(jsonb_agg(jsonb_build_object(''asset'', g.schema_name || ''.'' || g.table_name || CASE WHEN g.column_name = '''' THEN '''' ELSE ''.'' || g.column_name END, ''term'', g.term) ORDER BY g.schema_name, g.table_name, g.column_name, g.term), ''[]''::jsonb)) FROM eggdb_catalog.tags g', listed::text) INTO listed;
    END IF;
    INSERT INTO eggdb_compliance.results VALUES (current_run, 'personal_data_inventory', CASE WHEN listed = '{}'::jsonb THEN 'fail' ELSE 'review' END,
        CASE WHEN listed = '{}'::jsonb THEN jsonb_build_object('note', 'neither eggdb_privacy classifications nor eggdb_catalog tags are installed') ELSE listed END);
    IF eggdb_compliance.has_table('eggdb_ledger', 'chains') THEN
        listed := '[]'::jsonb;
        broken := 0;
        FOR chain IN EXECUTE 'SELECT c.table_schema AS schema_name, c.table_name AS table_name, c.last_seq AS rows FROM eggdb_ledger.chains c ORDER BY 1, 2' LOOP
            EXECUTE format('SELECT eggdb_ledger.verify(%L, %L)', chain.schema_name, chain.table_name) INTO counted;
            broken := broken + counted;
            listed := listed || jsonb_build_array(jsonb_build_object('table', chain.schema_name || '.' || chain.table_name, 'rows', chain.rows, 'broken_rows', counted));
            checked := checked + 1;
        END LOOP;
        INSERT INTO eggdb_compliance.results VALUES (current_run, 'tamper_evidence', CASE WHEN checked = 0 THEN 'review' WHEN broken = 0 THEN 'pass' ELSE 'fail' END, jsonb_build_object('ledger_tables', listed));
    ELSE
        INSERT INTO eggdb_compliance.results VALUES (current_run, 'tamper_evidence', 'review', jsonb_build_object('note', 'eggdb_ledger is not installed'));
    END IF;
    IF eggdb_compliance.has_table('eggdb_retention', 'rules') THEN
        EXECUTE 'SELECT coalesce(jsonb_agg(jsonb_build_object(''table'', r.schema_name || ''.'' || r.table_name, ''retain'', r.retain::text) ORDER BY r.schema_name, r.table_name), ''[]''::jsonb), count(*) FROM eggdb_retention.rules r' INTO listed, counted;
        EXECUTE 'SELECT count(*) FROM eggdb_retention.erasure_requests e WHERE e.erased_at IS NULL AND e.requested_at < now() - interval ''30 days'' AND e.waiting_for IS NULL' INTO pending;
        INSERT INTO eggdb_compliance.results VALUES (current_run, 'retention', CASE WHEN pending > 0 THEN 'fail' WHEN counted > 0 THEN 'pass' ELSE 'review' END,
            jsonb_build_object('rules', listed, 'erasure_requests_overdue', pending));
    ELSE
        INSERT INTO eggdb_compliance.results VALUES (current_run, 'retention', 'review', jsonb_build_object('note', 'eggdb_retention is not installed'));
    END IF;
    RETURN current_run;
END
$$;
CREATE FUNCTION eggdb_compliance.evidence(wanted_framework text DEFAULT NULL) RETURNS TABLE (framework text, control text, title text, check_name text, status text, evidence jsonb) LANGUAGE plpgsql AS $$
DECLARE
    current_run bigint;
BEGIN
    IF wanted_framework IS NOT NULL AND wanted_framework NOT IN ('SOC 2', 'HIPAA', 'PCI DSS') THEN
        RAISE EXCEPTION 'evidence covers SOC 2, HIPAA and PCI DSS, not %', wanted_framework USING ERRCODE = '22023';
    END IF;
    current_run := eggdb_compliance.collect();
    RETURN QUERY SELECT c.framework, c.control, c.title, c.check_name, r.status, r.evidence
        FROM eggdb_compliance.controls c JOIN eggdb_compliance.results r ON r.check_name = c.check_name AND r.run_id = current_run
        WHERE wanted_framework IS NULL OR c.framework = wanted_framework
        ORDER BY c.framework, c.control, c.check_name;
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
    fn evidence_maps_checks_to_each_frameworks_controls() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_compliance",
            "CREATE EXTENSION eggdb_ledger",
            "CREATE TABLE payments (id integer PRIMARY KEY, amount numeric NOT NULL)",
            "SELECT eggdb_ledger.protect('public', 'payments')",
            "INSERT INTO payments (id, amount) VALUES (1, 10)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT control, check_name, status FROM eggdb_compliance.evidence('PCI DSS')").unwrap(),
            "control|check_name|status\n10.3.2|tamper_evidence|pass\n12.5.2|personal_data_inventory|fail\n3.2.1|retention|review\n7.2.1|public_grants|pass\n7.2.2|privileged_roles|review\n8.3.9|login_expiry|review\nSELECT 6"
        );
        assert_eq!(run(&mut session, "SELECT count(*) AS kept FROM eggdb_compliance.results").unwrap(), "kept\n6\nSELECT 1");
        assert!(run(&mut session, "SELECT * FROM eggdb_compliance.evidence('ISO 27001')").is_err());
    }
}
