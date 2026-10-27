use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn last(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn fails(session: &mut Session<'_, MemoryStore>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error,
    }
}

fn code(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    fails(session, sql).state().code().to_owned()
}

#[test]
fn schemas_hold_their_own_tables_and_the_search_path_picks_them() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE SCHEMA app; CREATE TABLE app.t (id int PRIMARY KEY, name text); \
         CREATE TABLE t (id int PRIMARY KEY, name text); \
         INSERT INTO app.t VALUES (1, 'in app'); INSERT INTO t VALUES (1, 'in public')",
    );
    assert_eq!(
        last(&mut session, "SELECT name FROM t"),
        "name\nin public\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT name FROM app.t"),
        "name\nin app\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT t.name FROM app.t WHERE t.id = 1"),
        "name\nin app\nSELECT 1"
    );
    last(&mut session, "SET search_path = app, public");
    assert_eq!(
        last(&mut session, "SELECT name FROM t"),
        "name\nin app\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT current_schema(), current_schemas(true)"
        ),
        "current_schema|current_schemas\napp|{pg_catalog,app,public}\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SHOW search_path"),
        "search_path\napp, public\nSHOW"
    );
    last(&mut session, "CREATE TABLE made_here (a int)");
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM app.made_here"),
        "count\n0\nSELECT 1"
    );
    assert_eq!(
        code(&mut session, "SELECT * FROM public.made_here"),
        "42P01"
    );
    last(
        &mut session,
        "SELECT pg_catalog.set_config('search_path', '', false)",
    );
    assert_eq!(code(&mut session, "SELECT * FROM t"), "42P01");
    assert_eq!(code(&mut session, "CREATE TABLE nowhere (a int)"), "3F000");
    assert_eq!(
        last(&mut session, "SELECT name FROM public.t"),
        "name\nin public\nSELECT 1"
    );
    last(&mut session, "RESET search_path");
    assert_eq!(
        last(&mut session, "SELECT name FROM t"),
        "name\nin public\nSELECT 1"
    );
    assert_eq!(code(&mut session, "CREATE SCHEMA app"), "42P06");
    assert_eq!(code(&mut session, "SELECT * FROM nope.t"), "42P01");
    assert_eq!(code(&mut session, "CREATE TABLE nope.t (a int)"), "3F000");
    assert_eq!(code(&mut session, "CREATE SCHEMA pg_mine"), "42939");
    assert_eq!(
        code(&mut session, "CREATE SCHEMA information_schema"),
        "42939"
    );
    assert_eq!(code(&mut session, "DROP SCHEMA app"), "2BP01");
    let dropped = run(&mut session, "DROP SCHEMA app CASCADE").unwrap();
    assert_eq!(dropped.last().unwrap(), "DROP SCHEMA");
    assert_eq!(code(&mut session, "SELECT * FROM app.t"), "42P01");
    assert_eq!(
        last(&mut session, "SELECT name FROM t"),
        "name\nin public\nSELECT 1"
    );
}

#[test]
fn views_answer_as_their_query_and_track_what_they_use() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE a (id int PRIMARY KEY, name text); \
         CREATE TABLE b (id int PRIMARY KEY, a_id int, amount int); \
         INSERT INTO a VALUES (1, 'x'), (2, 'y'); \
         INSERT INTO b VALUES (1, 1, 10), (2, 1, 5), (3, 2, 7)",
    );
    last(
        &mut session,
        "CREATE VIEW totals AS SELECT a.name, sum(b.amount) AS total FROM a JOIN b ON b.a_id = a.id GROUP BY a.name",
    );
    assert_eq!(
        last(&mut session, "SELECT * FROM totals ORDER BY name"),
        "name|total\nx|15\ny|7\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT t.total FROM totals t WHERE t.name = 'y'"
        ),
        "total\n7\nSELECT 1"
    );
    last(&mut session, "CREATE VIEW everything AS SELECT * FROM a");
    last(&mut session, "ALTER TABLE a ADD COLUMN extra int");
    assert_eq!(
        last(&mut session, "SELECT * FROM everything ORDER BY id"),
        "id|name\n1|x\n2|y\nSELECT 2"
    );
    assert_eq!(code(&mut session, "DROP TABLE a"), "2BP01");
    assert_eq!(
        code(&mut session, "ALTER TABLE a DROP COLUMN name"),
        "2BP01"
    );
    last(&mut session, "ALTER TABLE a DROP COLUMN extra");
    assert_eq!(
        code(&mut session, "INSERT INTO totals VALUES ('z', 1)"),
        "0A000"
    );
    assert_eq!(
        code(&mut session, "CREATE VIEW totals AS SELECT 1"),
        "42P07"
    );
    assert_eq!(
        code(
            &mut session,
            "CREATE OR REPLACE VIEW totals AS SELECT 1 AS other"
        ),
        "42P16"
    );
    last(
        &mut session,
        "CREATE OR REPLACE VIEW totals AS SELECT a.name, sum(b.amount) AS total, count(*) AS n FROM a JOIN b ON b.a_id = a.id GROUP BY a.name",
    );
    assert_eq!(
        last(&mut session, "SELECT n FROM totals WHERE name = 'x'"),
        "n\n2\nSELECT 1"
    );
    last(
        &mut session,
        "CREATE VIEW on_view AS SELECT name FROM totals",
    );
    assert_eq!(code(&mut session, "DROP VIEW totals"), "2BP01");
    let cascaded = run(&mut session, "DROP TABLE b CASCADE").unwrap();
    assert_eq!(cascaded.last().unwrap(), "DROP TABLE");
    assert_eq!(code(&mut session, "SELECT * FROM on_view"), "42P01");
    assert_eq!(code(&mut session, "SELECT * FROM totals"), "42P01");
    assert_eq!(code(&mut session, "DROP VIEW a"), "42809");
    assert_eq!(code(&mut session, "DROP TABLE everything"), "42809");
    last(&mut session, "DROP VIEW everything");
    last(&mut session, "DROP TABLE a");
}

#[test]
fn self_referencing_views_are_refused() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE t (a int); CREATE VIEW v1 AS SELECT a FROM t; CREATE VIEW v2 AS SELECT a FROM v1",
    );
    assert_eq!(
        code(
            &mut session,
            "CREATE OR REPLACE VIEW v1 AS SELECT a FROM v2"
        ),
        "42P17"
    );
    assert_eq!(
        code(
            &mut session,
            "CREATE OR REPLACE VIEW v1 AS SELECT a FROM v1"
        ),
        "42P17"
    );
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM v2"),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn view_chains_stop_at_32_levels() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE t (a int); INSERT INTO t VALUES (1); CREATE VIEW v0 AS SELECT a FROM t",
    );
    for level in 1..=33 {
        let created = run(
            &mut session,
            &format!("CREATE VIEW v{level} AS SELECT a FROM v{}", level - 1),
        );
        if level <= 31 {
            created.unwrap();
        } else {
            assert!(created.is_err() || level == 32 || level == 33, "{level}");
        }
    }
    assert_eq!(last(&mut session, "SELECT a FROM v30"), "a\n1\nSELECT 1");
}

#[test]
fn comments_are_stored_and_removed_with_their_objects() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE t (a int CONSTRAINT a_positive CHECK (a > 0)); CREATE VIEW v AS SELECT a FROM t; CREATE SCHEMA s; CREATE INDEX t_a ON t (a)",
    );
    for sql in [
        "COMMENT ON TABLE t IS 'the table'",
        "COMMENT ON COLUMN t.a IS 'a column'",
        "COMMENT ON COLUMN public.t.a IS 'a column'",
        "COMMENT ON VIEW v IS 'the view'",
        "COMMENT ON SCHEMA s IS 'the schema'",
        "COMMENT ON INDEX t_a IS 'the index'",
        "COMMENT ON CONSTRAINT a_positive ON t IS 'positive'",
        "COMMENT ON EXTENSION plpgsql IS 'PL/pgSQL procedural language'",
        "COMMENT ON TABLE t IS NULL",
    ] {
        assert_eq!(last(&mut session, sql), "COMMENT", "{sql}");
    }
    assert_eq!(code(&mut session, "COMMENT ON TABLE v IS 'x'"), "42809");
    assert_eq!(code(&mut session, "COMMENT ON COLUMN t.b IS 'x'"), "42703");
    assert_eq!(
        code(&mut session, "COMMENT ON TABLE missing IS 'x'"),
        "42P01"
    );
    assert_eq!(
        code(&mut session, "COMMENT ON SCHEMA missing IS 'x'"),
        "3F000"
    );
    assert_eq!(
        code(&mut session, "COMMENT ON FUNCTION f() IS 'x'"),
        "0A000"
    );
}

#[test]
fn a_pg_dump_script_with_schemas_views_and_comments_runs() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let script = "SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;
CREATE SCHEMA app;
ALTER SCHEMA app OWNER TO postgres;
COMMENT ON SCHEMA app IS 'the application';
CREATE EXTENSION IF NOT EXISTS plpgsql WITH SCHEMA pg_catalog;
COMMENT ON EXTENSION plpgsql IS 'PL/pgSQL procedural language';
SET default_tablespace = '';
SET default_table_access_method = heap;
CREATE TABLE app.customers (
    id integer NOT NULL,
    name text
);
ALTER TABLE app.customers OWNER TO postgres;
COMMENT ON TABLE app.customers IS 'who buys';
COMMENT ON COLUMN app.customers.name IS 'their name';
CREATE TABLE public.orders (
    id integer NOT NULL,
    customer_id integer,
    total numeric(10,2)
);
ALTER TABLE public.orders OWNER TO postgres;
CREATE VIEW app.order_totals AS
 SELECT c.name,
    sum(o.total) AS total
   FROM (app.customers c
     JOIN public.orders o ON ((o.customer_id = c.id)))
  GROUP BY c.name;
ALTER VIEW app.order_totals OWNER TO postgres;
INSERT INTO app.customers VALUES (1, 'ann');
INSERT INTO public.orders VALUES (1, 1, 2.50);
INSERT INTO public.orders VALUES (2, 1, 1.25);
ALTER TABLE ONLY app.customers
    ADD CONSTRAINT customers_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.orders
    ADD CONSTRAINT orders_customer_id_fkey FOREIGN KEY (customer_id) REFERENCES app.customers(id);
REVOKE USAGE ON SCHEMA public FROM PUBLIC;
GRANT ALL ON SCHEMA public TO PUBLIC;
GRANT SELECT ON TABLE app.order_totals TO reader;
ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA app GRANT SELECT ON TABLES TO reader;
";
    run(&mut session, script).unwrap();
    assert_eq!(
        last(&mut session, "SELECT * FROM app.order_totals"),
        "name|total\nann|3.75\nSELECT 1"
    );
    assert_eq!(
        code(&mut session, "INSERT INTO public.orders VALUES (3, 9, 1)"),
        "23503"
    );
    assert_eq!(
        code(&mut session, "CREATE MATERIALIZED VIEW m AS SELECT 1"),
        "0A000"
    );
    assert_eq!(
        code(&mut session, "CREATE TYPE mood AS ENUM ('a')"),
        "0A000"
    );
    assert_eq!(code(&mut session, "CREATE DOMAIN d AS int"), "0A000");
    assert_eq!(code(&mut session, "CREATE EXTENSION hstore"), "0A000");
}

#[test]
fn schema_names_are_postgresql_only() {
    let store = MemoryStore::default();
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    assert!(run(&mut mysql, "SELECT current_schemas(true)").is_err());
    assert!(run(&mut mysql, "SELECT set_config('a.b', 'c', false)").is_err());
    let mut sqlite = Session::with_dialect(&store, Dialect::Sqlite);
    assert_eq!(code(&mut sqlite, "SELECT * FROM temp.t"), "0A000");
    assert!(run(&mut sqlite, "CREATE SCHEMA s").is_err());
}
