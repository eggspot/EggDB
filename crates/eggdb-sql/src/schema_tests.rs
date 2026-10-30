use crate::memory_store::MemoryStore;
use crate::tests::{fuzz, render};
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
        "CREATE TABLE t (a int); INSERT INTO t VALUES (1); CREATE VIEW v1 AS SELECT a FROM t",
    );
    for level in 2..=32 {
        last(
            &mut session,
            &format!("CREATE VIEW v{level} AS SELECT a FROM v{}", level - 1),
        );
    }
    assert_eq!(last(&mut session, "SELECT a FROM v31"), "a\n1\nSELECT 1");
    assert_eq!(last(&mut session, "SELECT a FROM v32"), "a\n1\nSELECT 1");
    let error = fails(&mut session, "CREATE VIEW v33 AS SELECT a FROM v32");
    assert_eq!(error.state().code(), "54001", "{error}");
    let error = fails(&mut session, "SELECT * FROM (SELECT a FROM v32) AS deeper");
    assert_eq!(error.state().code(), "54001", "{error}");
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
    assert_eq!(code(&mut session, "CREATE EXTENSION hstore"), "0A000");
}

#[test]
fn schema_names_are_postgresql_only() {
    let store = MemoryStore::default();
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    assert!(run(&mut mysql, "SELECT current_schemas(true)").is_err());
    assert!(run(&mut mysql, "SELECT set_config('a.b', 'c', false)").is_err());
    let mut sqlite = Session::with_dialect(&store, Dialect::Sqlite);
    assert_eq!(code(&mut sqlite, "SELECT * FROM temp.t"), "42P01");
    assert!(run(&mut sqlite, "CREATE SCHEMA s").is_err());
}

#[test]
fn mysql_views_and_comments_follow_mysql() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    last(
        &mut session,
        "CREATE TABLE t (id int PRIMARY KEY, name varchar(20) COMMENT 'the name') COMMENT='people'",
    );
    last(&mut session, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");
    last(
        &mut session,
        "CREATE ALGORITHM=MERGE DEFINER=`root`@`%` SQL SECURITY INVOKER VIEW v AS SELECT name FROM t WHERE id > 1",
    );
    assert_eq!(last(&mut session, "SELECT * FROM v"), "name\nb\nSELECT 1");
    last(
        &mut session,
        "CREATE OR REPLACE VIEW v AS SELECT name, id FROM t",
    );
    assert_eq!(
        last(&mut session, "SHOW FULL TABLES"),
        "Tables_in_eggdb|Table_type\nt|BASE TABLE\nv|VIEW\nSELECT 2"
    );
    let created = last(&mut session, "SHOW CREATE VIEW v");
    assert!(
        created.contains("VIEW `v` AS SELECT name, id FROM t"),
        "{created}"
    );
    let table = last(&mut session, "SHOW CREATE TABLE t");
    assert!(
        table.contains("COMMENT 'the name'") && table.contains("COMMENT='people'"),
        "{table}"
    );
    let full = last(&mut session, "SHOW FULL COLUMNS FROM t");
    assert!(full.contains("|the name"), "{full}");
    assert_eq!(
        fails(&mut session, "CREATE VIEW v AS SELECT 1").vendor_code(),
        Some(1050)
    );
    assert_eq!(
        fails(&mut session, "DROP VIEW missing").vendor_code(),
        Some(1051)
    );
    last(&mut session, "DROP VIEW IF EXISTS missing, v");
    assert_eq!(
        fails(&mut session, "SELECT * FROM v").vendor_code(),
        Some(1146)
    );
}

#[test]
fn sqlite_views_follow_sqlite() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    last(
        &mut session,
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)",
    );
    last(&mut session, "INSERT INTO t VALUES (1, 'x')");
    last(
        &mut session,
        "CREATE VIEW IF NOT EXISTS main.v AS SELECT b FROM t",
    );
    last(
        &mut session,
        "CREATE VIEW IF NOT EXISTS v AS SELECT a FROM t",
    );
    assert_eq!(last(&mut session, "SELECT * FROM main.v"), "b\nx\nSELECT 1");
    let error = fails(&mut session, "CREATE VIEW v AS SELECT 1");
    assert_eq!(error.message(), "view v already exists");
    assert!(
        fails(&mut session, "CREATE OR REPLACE VIEW w AS SELECT 1")
            .message()
            .contains("syntax")
    );
    last(&mut session, "DROP VIEW v");
    assert_eq!(
        fails(&mut session, "SELECT * FROM v").message(),
        "no such table: v"
    );
    assert_eq!(last(&mut session, "CREATE VIEW temp.w AS SELECT 1 AS one"), "CREATE VIEW");
    assert_eq!(last(&mut session, "SELECT one FROM w"), "one\n1\nSELECT 1");
}

#[test]
fn oracle_views_and_comments_follow_oracle() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    last(
        &mut session,
        "CREATE TABLE t (id NUMBER(9) PRIMARY KEY, name VARCHAR2(20))",
    );
    last(&mut session, "INSERT INTO t VALUES (1, 'a')");
    last(
        &mut session,
        "CREATE OR REPLACE FORCE VIEW v AS SELECT name FROM t WITH READ ONLY",
    );
    assert_eq!(
        last(&mut session, "SELECT name FROM v"),
        "NAME\na\nSELECT 1"
    );
    last(&mut session, "COMMENT ON TABLE t IS 'people'");
    last(&mut session, "COMMENT ON COLUMN t.name IS 'their name'");
    assert_eq!(
        last(
            &mut session,
            "SELECT table_name, table_type, comments FROM user_tab_comments ORDER BY table_name"
        ),
        "TABLE_NAME|TABLE_TYPE|COMMENTS\nT|TABLE|people\nV|VIEW|NULL\nSELECT 2"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT comments FROM user_col_comments WHERE table_name = 'T' AND column_name = 'NAME'"
        ),
        "COMMENTS\ntheir name\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT view_name, text FROM user_views"),
        "VIEW_NAME|TEXT\nV|SELECT name FROM t\nSELECT 1"
    );
    assert_eq!(
        fails(&mut session, "SELECT * FROM other.t").vendor_code(),
        Some(942)
    );
    assert_eq!(
        fails(&mut session, "CREATE VIEW v AS SELECT 1 FROM dual").vendor_code(),
        Some(955)
    );
    last(&mut session, "DROP VIEW v");
}

struct Seeded(u64);

impl Seeded {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

const VIEW_QUERIES: [&str; 8] = [
    "SELECT a, b, c FROM t WHERE b > 3",
    "SELECT t.a, u.d FROM t JOIN u ON u.b = t.b",
    "SELECT t.a, u.d FROM t LEFT JOIN u ON u.b = t.b WHERE t.c <> 'x'",
    "SELECT b, count(*) AS n, sum(a) AS total FROM t GROUP BY b",
    "SELECT a FROM t WHERE b IN (SELECT b FROM u WHERE d > 2)",
    "SELECT b FROM t UNION SELECT b FROM u",
    "SELECT DISTINCT c FROM t",
    "SELECT * FROM t WHERE a % 2 = 0",
];

fn ordered(session: &mut Session<'_, MemoryStore>, from: &str) -> String {
    let columns = last(session, &format!("SELECT * FROM {from} LIMIT 0"));
    let width = columns
        .lines()
        .next()
        .map_or(1, |line| line.split('|').count());
    let order: Vec<String> = (1..=width).map(|position| position.to_string()).collect();
    last(
        session,
        &format!("SELECT * FROM {from} ORDER BY {}", order.join(", ")),
    )
}

#[test]
fn a_view_answers_as_its_query_on_random_data_with_and_without_indexes() {
    for seed in 1..=6u64 {
        for indexed in [false, true] {
            let store = MemoryStore::default();
            let mut session = Session::new(&store);
            let mut random = Seeded(seed * 7919);
            last(
                &mut session,
                "CREATE TABLE t (a integer PRIMARY KEY, b integer, c text); \
                 CREATE TABLE u (id integer PRIMARY KEY, b integer, d integer)",
            );
            if indexed {
                last(
                    &mut session,
                    "CREATE INDEX t_b ON t (b); CREATE INDEX u_b ON u (b)",
                );
            }
            for id in 0..60 {
                let b = random.below(8);
                let c = ["x", "y", "z"][random.below(3) as usize];
                last(
                    &mut session,
                    &format!("INSERT INTO t VALUES ({id}, {b}, '{c}')"),
                );
                if random.below(2) == 0 {
                    let d = random.below(5);
                    last(
                        &mut session,
                        &format!("INSERT INTO u VALUES ({id}, {b}, {d})"),
                    );
                }
            }
            for (number, query) in VIEW_QUERIES.iter().enumerate() {
                last(&mut session, &format!("CREATE VIEW v{number} AS {query}"));
                let through_view = ordered(&mut session, &format!("v{number}"));
                let inline = ordered(&mut session, &format!("({query}) AS inline_query"));
                assert_eq!(
                    through_view, inline,
                    "seed {seed}, indexed {indexed}: {query}"
                );
                let filtered = last(
                    &mut session,
                    &format!(
                        "SELECT count(*) FROM v{number} AS v JOIN t ON t.a >= 0 WHERE t.a < 5"
                    ),
                );
                let filtered_inline = last(
                    &mut session,
                    &format!(
                        "SELECT count(*) FROM ({query}) AS v JOIN t ON t.a >= 0 WHERE t.a < 5"
                    ),
                );
                assert_eq!(filtered, filtered_inline, "{query}");
            }
            session.check_indexes().unwrap();
        }
    }
}

#[test]
fn hostile_schema_view_and_comment_statements_never_panic() {
    for dialect in [
        Dialect::PostgreSql,
        Dialect::MySql,
        Dialect::Sqlite,
        Dialect::Oracle,
    ] {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        let _ = run(&mut session, "CREATE TABLE t (a int PRIMARY KEY, b int)");
        let fragments = [
            "CREATE SCHEMA",
            "CREATE SCHEMA IF NOT EXISTS s",
            "DROP SCHEMA",
            "s",
            "s.t",
            "s.v",
            "public.t",
            "main.t",
            "temp.t",
            "EGGDB.t",
            "\"\u{0}\"",
            "\"a\u{1}b\"",
            "CASCADE",
            "RESTRICT",
            "ALTER SCHEMA s RENAME TO r",
            "ALTER TABLE t SET SCHEMA s",
            "SET search_path = s, public",
            "SET search_path TO ''",
            "SELECT set_config('search_path', 's', false)",
            "SELECT current_schema()",
            "SELECT current_schemas(false)",
            "CREATE VIEW v AS",
            "CREATE OR REPLACE VIEW v",
            "CREATE OR REPLACE FORCE VIEW v",
            "CREATE ALGORITHM=MERGE DEFINER=`a`@`b` SQL SECURITY DEFINER VIEW v",
            "CREATE VIEW IF NOT EXISTS v",
            "(x, y)",
            "AS SELECT * FROM t",
            "AS SELECT a FROM v",
            "SELECT * FROM v",
            "WITH CHECK OPTION",
            "WITH READ ONLY",
            "DROP VIEW",
            "DROP VIEW IF EXISTS v",
            "ALTER VIEW v RENAME TO w",
            "COMMENT ON TABLE t IS",
            "COMMENT ON COLUMN t.a IS",
            "COMMENT ON VIEW v IS",
            "COMMENT ON SCHEMA s IS",
            "COMMENT ON INDEX i IS",
            "COMMENT ON CONSTRAINT c ON t IS",
            "'text'",
            "NULL",
            "GRANT SELECT ON t TO r",
            "REVOKE ALL ON SCHEMA public FROM PUBLIC",
            "ALTER DEFAULT PRIVILEGES",
            "CREATE EXTENSION IF NOT EXISTS plpgsql",
            "SHOW CREATE VIEW v",
            "SHOW FULL TABLES",
            "SELECT * FROM user_views",
            "SELECT * FROM user_tab_comments",
            "FROM (t JOIN t AS x ON x.a = t.a)",
            "(",
            ")",
            ",",
            ";",
        ];
        fuzz(&mut session, &fragments);
        session.check_indexes().unwrap();
    }
}

#[test]
fn deep_and_wide_view_graphs_stay_bounded() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE t (a int); INSERT INTO t VALUES (1)",
    );
    last(&mut session, "CREATE VIEW w0 AS SELECT a FROM t");
    let create = |level: usize| {
        let previous = level - 1;
        format!(
            "CREATE VIEW w{level} AS SELECT x.a FROM w{previous} x JOIN w{previous} y ON x.a = y.a"
        )
    };
    for level in 1..=9 {
        last(&mut session, &create(level));
    }
    let error = fails(&mut session, &create(10));
    assert_eq!(error.state().code(), "54001", "{error}");
    let error = fails(&mut session, "SELECT * FROM w9 a JOIN w9 b ON a.a = b.a");
    assert_eq!(error.state().code(), "54001", "{error}");
    assert_eq!(last(&mut session, "SELECT a FROM w9"), "a\n1\nSELECT 1");
}

fn overwrite(store: &MemoryStore, key: &[u8], value: &[u8]) {
    use crate::store::{KeyValueStore, KeyValueWrite};
    let mut writer = store.begin_write().unwrap();
    writer.put(key, value).unwrap();
    writer.commit().unwrap();
}

#[test]
fn damaged_schema_view_and_comment_records_are_errors_never_panics() {
    use crate::encoding::{
        CommentKind, comment_key, dependency_key, schema_key, sequence_key, table_key,
    };
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE SCHEMA app; CREATE TABLE app.t (a int); CREATE VIEW app.v AS SELECT a FROM app.t; \
         COMMENT ON TABLE app.t IS 'x'",
    );
    let mut view = {
        use crate::store::{KeyValueRead, KeyValueStore};
        store
            .begin_read()
            .unwrap()
            .get(&table_key("app\0v"))
            .unwrap()
            .unwrap()
    };
    let last_byte = view.len() - 1;
    view[last_byte] ^= 0xFF;
    overwrite(&store, &table_key("app\0v"), &view);
    let error = fails(&mut session, "SELECT * FROM app.v");
    assert!(
        ["XX001", "0A000"].contains(&error.state().code()),
        "{error}"
    );
    overwrite(
        &store,
        &table_key("app\0v"),
        &[8, 0, 0, 1, b' ', 1, 1, b'a', 0, 0, 0],
    );
    assert_eq!(code(&mut session, "SELECT * FROM app.v"), "XX001");
    overwrite(&store, &schema_key("app"), &[1, 2, 3]);
    assert_eq!(code(&mut session, "CREATE TABLE app.u (a int)"), "XX001");
    overwrite(&store, &schema_key("app"), &[1]);
    overwrite(
        &store,
        &comment_key("app\0t", CommentKind::Column, "a"),
        &[1, 0xC3, 0x28],
    );
    let listed = session.tables();
    assert!(listed.is_err_and(|error| error.state().code() == "XX001"));
    overwrite(
        &store,
        &comment_key("app\0t", CommentKind::Column, "a"),
        &[1, b'y'],
    );
    overwrite(&store, &dependency_key("app\0t", "\u{0}\u{0}"), &[1]);
    let error = fails(&mut session, "DROP TABLE app.t CASCADE");
    assert!(
        ["XX001", "0A000"].contains(&error.state().code()),
        "{error}"
    );
    overwrite(&store, &table_key("app\0\0z"), &[1, 0, 0, 0, 0, 0, 0]);
    let error = fails(&mut session, "DROP SCHEMA app CASCADE");
    assert_eq!(error.state().code(), "XX001", "{error}");
    overwrite(&store, &sequence_key("bad\0s"), &[1]);
    let _ = run(&mut session, "SELECT nextval('bad.s')");
}

#[test]
fn schema_and_view_changes_roll_back_as_one_transaction() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "CREATE TABLE t (a int)");
    last(
        &mut session,
        "BEGIN; CREATE SCHEMA app; CREATE TABLE app.t (a int); CREATE VIEW v AS SELECT a FROM app.t; \
         COMMENT ON VIEW v IS 'gone'; ROLLBACK",
    );
    assert_eq!(code(&mut session, "SELECT * FROM v"), "42P01");
    assert_eq!(code(&mut session, "SELECT * FROM app.t"), "42P01");
    last(&mut session, "CREATE SCHEMA app");
    assert_eq!(
        last(&mut session, "CREATE VIEW v AS SELECT a FROM t"),
        "CREATE VIEW"
    );
    let failed = run(
        &mut session,
        "BEGIN; DROP VIEW v; DROP SCHEMA app; CREATE TABLE app.t (a int); SELECT 1/0",
    );
    assert!(failed.is_err());
    last(&mut session, "ROLLBACK");
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM v"),
        "count\n0\nSELECT 1"
    );
    last(&mut session, "CREATE TABLE app.t (a int)");
}

#[test]
fn a_view_cannot_keep_a_parameter_of_the_statement_that_created_it() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let parameter = crate::Parameter {
        declared_type: None,
        value: Some("1".into()),
    };
    let error = session
        .execute_prepared("CREATE VIEW v AS SELECT $1 AS a", &[parameter], &mut |_| {
            true
        })
        .unwrap_err();
    assert_eq!(error.state().code(), "42P02", "{error}");
    assert_eq!(code(&mut session, "SELECT * FROM v"), "42P01");
}

#[test]
fn generated_names_in_a_schema_carry_no_schema_and_can_be_dropped() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE SCHEMA app; CREATE TABLE app.t (id int PRIMARY KEY, v int UNIQUE, w int CHECK (w > 0)); \
         CREATE INDEX ON app.t (w); CREATE TABLE app.c (id int REFERENCES app.t (id))",
    );
    let tables = session.tables().unwrap();
    let t = tables.iter().find(|table| table.name == "app.t").unwrap();
    let names: Vec<&str> = t.indexes.iter().map(|index| index.name.as_str()).collect();
    assert_eq!(names, ["t_v_key", "t_w_idx"]);
    let statements = t.index_statements_in(Dialect::PostgreSql);
    assert!(
        statements.contains(" ON app.t (") && !statements.contains('\0'),
        "{statements}"
    );
    let refused = fails(&mut session, "INSERT INTO app.c VALUES (9)");
    assert!(refused.message().contains("\"c_id_fkey\""), "{refused}");
    let refused = fails(&mut session, "INSERT INTO app.t VALUES (1, 1, 0)");
    assert!(refused.message().contains("\"t_w_check\""), "{refused}");
    let refused = fails(&mut session, "DROP TABLE app.t");
    assert_eq!(
        refused.detail(),
        Some("constraint c_id_fkey on table app.c depends on table app.t")
    );
    last(&mut session, "ALTER TABLE app.c DROP CONSTRAINT c_id_fkey");
    last(&mut session, "ALTER TABLE app.t DROP CONSTRAINT t_pkey");
    last(&mut session, "ALTER TABLE app.t DROP CONSTRAINT t_v_key");
    last(&mut session, "ALTER TABLE app.t DROP CONSTRAINT t_w_check");
    last(&mut session, "DROP INDEX app.t_w_idx");
}

#[test]
fn view_text_is_capped_and_its_expansion_budgeted_per_statement() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE t (a int); INSERT INTO t VALUES (1)",
    );
    let list: Vec<String> = (0..200_000).map(|number| number.to_string()).collect();
    let huge = format!(
        "CREATE VIEW too_big AS SELECT a FROM t WHERE a IN ({})",
        list.join(",")
    );
    assert!(huge.len() > 1 << 20);
    assert_eq!(code(&mut session, &huge), "54000");
    let list: Vec<String> = (0..100_000).map(|number| number.to_string()).collect();
    last(
        &mut session,
        &format!(
            "CREATE VIEW big AS SELECT a FROM t WHERE a IN ({})",
            list.join(",")
        ),
    );
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM big"),
        "count\n1\nSELECT 1"
    );
    let copies: Vec<&str> = (0..120).map(|_| "SELECT a FROM big").collect();
    let error = fails(&mut session, &copies.join(" UNION ALL "));
    assert_eq!(error.state().code(), "54000", "{error}");
    assert!(error.message().contains("bytes of view text"), "{error}");
}

#[test]
fn quoted_current_schema_calls_follow_the_search_path() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE SCHEMA s; CREATE TABLE s.t (a text); SET search_path = s",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT \"current_schema\"(), \"current_schemas\"(false)"
        ),
        "current_schema|current_schemas\ns|{s}\nSELECT 1"
    );
    last(&mut session, "INSERT INTO t VALUES (\"current_schema\"())");
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM t WHERE a = current_schema() AND a IN (SELECT \"current_schema\"())"
        ),
        "count\n1\nSELECT 1"
    );
}

#[test]
fn set_schema_refuses_index_names_the_target_schema_already_uses() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE SCHEMA s; CREATE TABLE s.a (x int); CREATE INDEX i ON s.a (x); \
         CREATE TABLE b (x int); CREATE INDEX i ON b (x)",
    );
    assert_eq!(code(&mut session, "ALTER TABLE b SET SCHEMA s"), "42P07");
    last(
        &mut session,
        "CREATE TABLE p (id int PRIMARY KEY); CREATE TABLE s.p_pkey (x int)",
    );
    assert_eq!(code(&mut session, "ALTER TABLE p SET SCHEMA s"), "42P07");
    last(&mut session, "DROP INDEX i; ALTER TABLE b SET SCHEMA s");
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM s.b"),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn a_rolled_back_block_restores_the_search_path() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(&mut session, "CREATE SCHEMA q; SET search_path = public");
    last(&mut session, "BEGIN; SET search_path = q; ROLLBACK");
    assert_eq!(
        last(&mut session, "SHOW search_path"),
        "search_path\npublic\nSHOW"
    );
    last(&mut session, "BEGIN; SET search_path = q; COMMIT");
    assert_eq!(
        last(&mut session, "SHOW search_path"),
        "search_path\nq\nSHOW"
    );
    assert!(run(&mut session, "BEGIN; SET search_path = public; SELECT 1/0").is_err());
    last(&mut session, "ROLLBACK");
    assert_eq!(
        last(&mut session, "SHOW search_path"),
        "search_path\nq\nSHOW"
    );
}

#[test]
fn star_views_over_a_recreated_table_match_columns_by_name_in_mysql_and_again_in_sqlite() {
    let store = MemoryStore::default();
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    last(
        &mut mysql,
        "CREATE TABLE t (a INT, b TEXT); CREATE VIEW v AS SELECT * FROM t; \
         CREATE VIEW w (x, y) AS SELECT * FROM t; DROP TABLE t; \
         CREATE TABLE t (b TEXT, a INT, c INT); INSERT INTO t VALUES ('x', 5, 7)",
    );
    assert_eq!(last(&mut mysql, "SELECT * FROM v"), "a|b\n5|x\nSELECT 1");
    assert_eq!(last(&mut mysql, "SELECT * FROM w"), "x|y\n5|x\nSELECT 1");
    last(&mut mysql, "DROP TABLE t; CREATE TABLE t (b TEXT)");
    assert_eq!(
        fails(&mut mysql, "SELECT * FROM v").vendor_code(),
        Some(1356)
    );
    let store = MemoryStore::default();
    let mut sqlite = Session::with_dialect(&store, Dialect::Sqlite);
    last(
        &mut sqlite,
        "CREATE TABLE t (a INT, b TEXT); CREATE VIEW v AS SELECT * FROM t; DROP TABLE t; \
         CREATE TABLE t (b TEXT, a INT); INSERT INTO t VALUES ('x', 5)",
    );
    assert_eq!(last(&mut sqlite, "SELECT * FROM v"), "b|a\nx|5\nSELECT 1");
    assert_eq!(last(&mut sqlite, "SELECT a FROM v"), "a\n5\nSELECT 1");
}

#[test]
fn a_sqlite_star_view_with_a_column_list_renames_by_position_and_fails_when_the_shape_changes() {
    let store = MemoryStore::default();
    let mut sqlite = Session::with_dialect(&store, Dialect::Sqlite);
    last(
        &mut sqlite,
        "CREATE TABLE t (a INT, b TEXT); INSERT INTO t VALUES (1, 'p'); \
         CREATE VIEW w (x, y) AS SELECT * FROM t",
    );
    assert_eq!(last(&mut sqlite, "SELECT * FROM w"), "x|y\n1|p\nSELECT 1");
    assert_eq!(last(&mut sqlite, "SELECT x FROM w"), "x\n1\nSELECT 1");
    last(
        &mut sqlite,
        "DROP TABLE t; CREATE TABLE t (c TEXT, d INT); INSERT INTO t VALUES ('q', 2)",
    );
    assert_eq!(
        last(&mut sqlite, "SELECT x, y FROM w"),
        "x|y\nq|2\nSELECT 1"
    );
    last(&mut sqlite, "ALTER TABLE t ADD COLUMN e INT");
    let error = fails(&mut sqlite, "SELECT * FROM w");
    assert_eq!(error.message(), "expected 2 columns for 'w' but got 3");
    assert_eq!(error.vendor_code(), Some(1));
    last(&mut sqlite, "DROP TABLE t; CREATE TABLE t (a INT)");
    assert_eq!(
        fails(&mut sqlite, "SELECT x FROM w").message(),
        "expected 2 columns for 'w' but got 1"
    );
    last(
        &mut sqlite,
        "DROP TABLE t; CREATE TABLE t (a INT, b TEXT, c INT); INSERT INTO t VALUES (1, 'p', 3); \
         CREATE VIEW v AS SELECT * FROM t",
    );
    assert_eq!(
        last(&mut sqlite, "SELECT * FROM v"),
        "a|b|c\n1|p|3\nSELECT 1"
    );
}

#[test]
fn star_views_over_sources_sharing_a_column_name_work_in_mysql_and_oracle() {
    let store = MemoryStore::default();
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    last(
        &mut mysql,
        "CREATE TABLE a (id INT, v INT); CREATE TABLE b (id INT, w INT); \
         INSERT INTO a VALUES (1, 10); INSERT INTO b VALUES (1, 20); \
         CREATE VIEW w (x, y, z, q) AS SELECT * FROM a JOIN b ON a.id = b.id",
    );
    assert_eq!(
        last(&mut mysql, "SELECT * FROM w"),
        "x|y|z|q\n1|10|1|20\nSELECT 1"
    );
    last(
        &mut mysql,
        "DROP TABLE a; DROP TABLE b; CREATE TABLE a (id INT, v INT); \
         CREATE TABLE b (id INT, w INT); INSERT INTO a VALUES (2, 30); INSERT INTO b VALUES (2, 40)",
    );
    assert_eq!(
        last(&mut mysql, "SELECT z, q, x FROM w"),
        "z|q|x\n2|40|2\nSELECT 1"
    );
    last(
        &mut mysql,
        "DROP TABLE b; CREATE TABLE b (w INT, id INT); INSERT INTO b VALUES (50, 2)",
    );
    assert_eq!(
        last(&mut mysql, "SELECT * FROM w"),
        "x|y|z|q\n2|30|2|50\nSELECT 1"
    );
    last(&mut mysql, "DROP TABLE b; CREATE TABLE b (id INT)");
    assert_eq!(
        fails(&mut mysql, "SELECT * FROM w").vendor_code(),
        Some(1356)
    );

    let store = MemoryStore::default();
    let mut oracle = Session::with_dialect(&store, Dialect::Oracle);
    last(
        &mut oracle,
        "CREATE TABLE a (id NUMBER(9), v NUMBER(9)); CREATE TABLE b (id NUMBER(9), w NUMBER(9)); \
         INSERT INTO a VALUES (1, 10); INSERT INTO b VALUES (1, 20); \
         CREATE VIEW w (x, y, z, q) AS SELECT * FROM a JOIN b ON a.id = b.id",
    );
    assert_eq!(
        last(&mut oracle, "SELECT * FROM w"),
        "X|Y|Z|Q\n1|10|1|20\nSELECT 1"
    );
    last(
        &mut oracle,
        "DROP TABLE a; DROP TABLE b; CREATE TABLE a (id NUMBER(9), v NUMBER(9)); \
         CREATE TABLE b (id NUMBER(9), w NUMBER(9)); \
         INSERT INTO a VALUES (2, 30); INSERT INTO b VALUES (2, 40)",
    );
    assert_eq!(
        last(&mut oracle, "SELECT q, z FROM w"),
        "Q|Z\n40|2\nSELECT 1"
    );
    last(&mut oracle, "DROP TABLE b; CREATE TABLE b (id NUMBER(9))");
    assert_eq!(
        fails(&mut oracle, "SELECT * FROM w").vendor_code(),
        Some(4063)
    );
}

#[test]
fn a_mysql_star_view_matches_source_columns_case_insensitively() {
    let store = MemoryStore::default();
    let mut postgres = Session::new(&store);
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    last(
        &mut postgres,
        "CREATE TABLE t (\"ID\" int, \"Name\" text); INSERT INTO t VALUES (1, 'p')",
    );
    last(&mut mysql, "CREATE VIEW v AS SELECT * FROM t");
    assert_eq!(
        last(&mut mysql, "SELECT * FROM v"),
        "ID|Name\n1|p\nSELECT 1"
    );
    last(&mut mysql, "DROP TABLE t");
    last(
        &mut postgres,
        "CREATE TABLE t (name text, id int); INSERT INTO t VALUES ('q', 2)",
    );
    assert_eq!(
        last(&mut mysql, "SELECT * FROM v"),
        "ID|Name\n2|q\nSELECT 1"
    );
    last(&mut mysql, "DROP TABLE t");
    last(
        &mut postgres,
        "CREATE TABLE t (id int, \"ID\" int, \"Name\" text); INSERT INTO t VALUES (3, 4, 'r')",
    );
    assert_eq!(
        last(&mut mysql, "SELECT * FROM v"),
        "ID|Name\n4|r\nSELECT 1"
    );
}
