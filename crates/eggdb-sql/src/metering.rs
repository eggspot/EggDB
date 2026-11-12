/// The statements `CREATE EXTENSION eggdb_metering` runs: usage per tenant, database, role and
/// statement shape, taken as differences between snapshots of `pg_stat_statements`, priced by
/// rates into chargeback and showback reports, with a `pg_notify` per tenant as the billing
/// hook (D44).
pub(crate) const METERING_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_metering;
CREATE TABLE eggdb_metering.tenants (role_name text PRIMARY KEY, tenant text NOT NULL);
CREATE TABLE eggdb_metering.rates (metric text PRIMARY KEY CHECK (metric IN ('calls', 'exec_ms', 'rows')), unit_price numeric NOT NULL CHECK (unit_price >= 0), currency text NOT NULL DEFAULT 'USD');
CREATE TABLE eggdb_metering.statement_marks (userid bigint NOT NULL, dbid bigint NOT NULL, queryid bigint NOT NULL, calls bigint NOT NULL, exec_ms double precision NOT NULL, rows bigint NOT NULL, PRIMARY KEY (userid, dbid, queryid));
CREATE TABLE eggdb_metering.usage (sampled_at timestamptz NOT NULL DEFAULT now(), tenant text NOT NULL, database_name text NOT NULL, role_name text NOT NULL, queryid bigint NOT NULL, calls bigint NOT NULL, exec_ms double precision NOT NULL, rows bigint NOT NULL);
CREATE INDEX usage_by_tenant ON eggdb_metering.usage (tenant, sampled_at);
CREATE FUNCTION eggdb_metering.snapshot() RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    found_row record;
    previous record;
    added_calls bigint;
    added_ms double precision;
    added_rows bigint;
    recorded bigint := 0;
    summary record;
BEGIN
    FOR found_row IN SELECT s.userid::bigint AS userid, s.dbid::bigint AS dbid, s.queryid, s.calls, s.total_exec_time, s.rows,
            coalesce(r.rolname, s.userid::text) AS role_name, coalesce(d.datname, s.dbid::text) AS database_name
        FROM pg_stat_statements s
        LEFT JOIN pg_roles r ON r.oid = s.userid
        LEFT JOIN pg_database d ON d.oid = s.dbid LOOP
        SELECT m.calls, m.exec_ms, m.rows INTO previous FROM eggdb_metering.statement_marks m
            WHERE m.userid = found_row.userid AND m.dbid = found_row.dbid AND m.queryid = found_row.queryid;
        IF NOT FOUND OR found_row.calls < previous.calls THEN
            added_calls := found_row.calls;
            added_ms := found_row.total_exec_time;
            added_rows := found_row.rows;
        ELSE
            added_calls := found_row.calls - previous.calls;
            added_ms := found_row.total_exec_time - previous.exec_ms;
            added_rows := found_row.rows - previous.rows;
        END IF;
        IF added_calls > 0 THEN
            INSERT INTO eggdb_metering.usage (tenant, database_name, role_name, queryid, calls, exec_ms, rows)
                VALUES (coalesce((SELECT t.tenant FROM eggdb_metering.tenants t WHERE t.role_name = found_row.role_name), found_row.role_name),
                        found_row.database_name, found_row.role_name, found_row.queryid, added_calls, added_ms, added_rows);
            recorded := recorded + 1;
        END IF;
        INSERT INTO eggdb_metering.statement_marks (userid, dbid, queryid, calls, exec_ms, rows)
            VALUES (found_row.userid, found_row.dbid, found_row.queryid, found_row.calls, found_row.total_exec_time, found_row.rows)
            ON CONFLICT (userid, dbid, queryid) DO UPDATE SET calls = EXCLUDED.calls, exec_ms = EXCLUDED.exec_ms, rows = EXCLUDED.rows;
    END LOOP;
    FOR summary IN SELECT u.tenant, sum(u.calls) AS calls, sum(u.exec_ms) AS exec_ms, sum(u.rows) AS rows FROM eggdb_metering.usage u WHERE u.sampled_at = now() GROUP BY u.tenant ORDER BY u.tenant LOOP
        PERFORM pg_notify('eggdb_metering', jsonb_build_object('tenant', summary.tenant, 'sampled_at', now(), 'calls', summary.calls, 'exec_ms', summary.exec_ms, 'rows', summary.rows)::text);
    END LOOP;
    RETURN recorded;
END
$$;
CREATE FUNCTION eggdb_metering.chargeback(period_start timestamptz, period_end timestamptz) RETURNS TABLE (tenant text, database_name text, calls bigint, exec_ms double precision, rows bigint, cost numeric, currency text) LANGUAGE plpgsql AS $$
BEGIN
    RETURN QUERY SELECT u.tenant, u.database_name, sum(u.calls)::bigint, sum(u.exec_ms), sum(u.rows)::bigint,
            round(sum(u.calls) * coalesce((SELECT r.unit_price FROM eggdb_metering.rates r WHERE r.metric = 'calls'), 0)
                + sum(u.exec_ms)::numeric * coalesce((SELECT r.unit_price FROM eggdb_metering.rates r WHERE r.metric = 'exec_ms'), 0)
                + sum(u.rows) * coalesce((SELECT r.unit_price FROM eggdb_metering.rates r WHERE r.metric = 'rows'), 0), 6),
            coalesce((SELECT min(r.currency) FROM eggdb_metering.rates r), 'USD')
        FROM eggdb_metering.usage u
        WHERE u.sampled_at >= period_start AND u.sampled_at < period_end
        GROUP BY u.tenant, u.database_name
        ORDER BY u.tenant, u.database_name;
END
$$;
CREATE FUNCTION eggdb_metering.showback(period_start timestamptz, period_end timestamptz) RETURNS TABLE (tenant text, calls bigint, exec_ms double precision, rows bigint, share_of_exec_ms double precision) LANGUAGE plpgsql AS $$
DECLARE
    total_ms double precision;
BEGIN
    SELECT sum(u.exec_ms) INTO total_ms FROM eggdb_metering.usage u WHERE u.sampled_at >= period_start AND u.sampled_at < period_end;
    RETURN QUERY SELECT u.tenant, sum(u.calls)::bigint, sum(u.exec_ms), sum(u.rows)::bigint,
            CASE WHEN coalesce(total_ms, 0) = 0 THEN 0 ELSE sum(u.exec_ms) / total_ms END
        FROM eggdb_metering.usage u
        WHERE u.sampled_at >= period_start AND u.sampled_at < period_end
        GROUP BY u.tenant
        ORDER BY sum(u.exec_ms) DESC, u.tenant;
END
$$;
CREATE FUNCTION eggdb_metering.top_statements(wanted_tenant text, period_start timestamptz, period_end timestamptz, how_many integer DEFAULT 10) RETURNS TABLE (queryid bigint, query text, calls bigint, exec_ms double precision, rows bigint) LANGUAGE plpgsql AS $$
BEGIN
    RETURN QUERY SELECT u.queryid, (SELECT min(s.query) FROM pg_stat_statements s WHERE s.queryid = u.queryid), sum(u.calls)::bigint, sum(u.exec_ms), sum(u.rows)::bigint
        FROM eggdb_metering.usage u
        WHERE u.tenant = wanted_tenant AND u.sampled_at >= period_start AND u.sampled_at < period_end
        GROUP BY u.queryid
        ORDER BY sum(u.exec_ms) DESC, u.queryid
        LIMIT greatest(how_many, 0);
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
    fn usage_is_priced_per_tenant_and_shared_out() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_metering",
            "INSERT INTO eggdb_metering.rates (metric, unit_price) VALUES ('calls', 0.001), ('exec_ms', 0.0001), ('rows', 0)",
            "INSERT INTO eggdb_metering.usage (sampled_at, tenant, database_name, role_name, queryid, calls, exec_ms, rows) VALUES ('2026-11-01 10:00+00', 'acme', 'shop', 'acme_app', 1, 1000, 3000, 50), ('2026-11-01 11:00+00', 'acme', 'shop', 'acme_app', 2, 500, 1000, 10), ('2026-11-01 11:00+00', 'globex', 'shop', 'globex_app', 3, 100, 4000, 5)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT tenant, calls, cost FROM eggdb_metering.chargeback('2026-11-01', '2026-12-01')").unwrap(),
            "tenant|calls|cost\nacme|1500|1.900000\nglobex|100|0.500000\nSELECT 2"
        );
        assert_eq!(
            run(&mut session, "SELECT tenant, share_of_exec_ms FROM eggdb_metering.showback('2026-11-01', '2026-12-01')").unwrap(),
            "tenant|share_of_exec_ms\nacme|0.5\nglobex|0.5\nSELECT 2"
        );
        assert_eq!(run(&mut session, "SELECT eggdb_metering.snapshot() AS recorded").unwrap(), "recorded\n0\nSELECT 1");
    }
}
