use super::snapshot::{BUILDS, LOADS};
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session, SqlError};
use std::time::{Duration, Instant};

fn session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::new(store);
    session.identify("eggdb", "shapes");
    session
}

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

fn rows_of(rendered: &str) -> Vec<&str> {
    let lines: Vec<&str> = rendered.lines().collect();
    lines
        .get(1..lines.len().saturating_sub(1))
        .unwrap_or(&[])
        .to_vec()
}

const INDEX_QUERY: &str = "SELECT c2.relname, i.indisprimary, i.indisunique, i.indisclustered, i.indisvalid, pg_catalog.pg_get_indexdef(i.indexrelid, 0, true),
  pg_catalog.pg_get_constraintdef(con.oid, true), contype, condeferrable, condeferred, i.indisreplident, c2.reltablespace
FROM pg_catalog.pg_class c, pg_catalog.pg_class c2, pg_catalog.pg_index i
  LEFT JOIN pg_catalog.pg_constraint con ON (conrelid = i.indrelid AND conindid = i.indexrelid AND contype IN ('p','u','x'))
WHERE c.oid = 'TABLE'::regclass AND c.oid = i.indrelid AND i.indexrelid = c2.oid
ORDER BY i.indisprimary DESC, c2.relname";

const COLUMN_QUERY: &str = "SELECT a.attname,
  pg_catalog.format_type(a.atttypid, a.atttypmod),
  (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true)
   FROM pg_catalog.pg_attrdef d
   WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef),
  a.attnotnull,
  (SELECT c.collname FROM pg_catalog.pg_collation c, pg_catalog.pg_type t
   WHERE c.oid = a.attcollation AND t.oid = a.atttypid AND a.attcollation <> t.typcollation) AS attcollation,
  a.attidentity,
  a.attgenerated
FROM pg_catalog.pg_attribute a
WHERE a.attrelid = 'TABLE'::regclass AND a.attnum > 0 AND NOT a.attisdropped
ORDER BY a.attnum";

fn for_table(query: &str, table: &str) -> String {
    query.replace("TABLE", table)
}

#[test]
fn psql_shapes_over_empty_catalogs_work_on_rows_as_postgresql_16_does() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    last(
        &mut session,
        "CREATE TABLE t (a integer PRIMARY KEY, b text)",
    );
    assert_eq!(
        rows_of(&last(
            &mut session,
            "SELECT (SELECT a.v FROM unnest(x.r) WITH ORDINALITY AS a(v, depth) ORDER BY a.depth DESC LIMIT 1), \
             (SELECT count(*) FROM pg_catalog.pg_partition_ancestors(x.c) WITH ORDINALITY AS a(relid, depth)) \
             FROM (VALUES ('{10,6171}'::oid[], 't'::regclass)) AS x(r, c)"
        )),
        ["6171|0"]
    );
    assert_eq!(
        rows_of(&last(
            &mut session,
            "SELECT (SELECT string_agg(attname, ', ') \
               FROM pg_catalog.generate_series(0, pg_catalog.array_upper(v.k::pg_catalog.int2[], 1)) s, \
                    pg_catalog.pg_attribute \
              WHERE attrelid = v.rel AND attnum = k[s]) \
             FROM (VALUES ('1 2'::int2vector, 't'::regclass)) v(k, rel)"
        )),
        ["a, b"]
    );
    assert_eq!(
        rows_of(&last(
            &mut session,
            "SELECT pg_catalog.array_to_string(array(select rolname from pg_catalog.pg_roles \
             where oid = any (v.r) order by 1),',') FROM (VALUES ('{10,6171}'::oid[])) v(r)"
        )),
        ["eggdb,pg_database_owner"]
    );
    assert_eq!(
        rows_of(&last(
            &mut session,
            "SELECT r = '{0}', r <> '{0}' FROM (VALUES ('{0}'::oid[]), ('{10}'::oid[])) v(r)"
        )),
        ["t|f", "f|t"]
    );
}

#[test]
fn oid_and_reg_types_read_and_show_as_postgresql_16_does() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    last(
        &mut session,
        "CREATE TABLE t (a integer PRIMARY KEY, b text)",
    );
    assert_eq!(
        rows_of(&last(
            &mut session,
            "SELECT 't'::regclass, 't'::regclass::oid = c.oid, '99999'::regclass, 'integer'::regtype, \
             23::regtype, 'int4'::regtype::oid, 'public'::regnamespace, \
             'pg_catalog'::regnamespace::oid, 'eggdb'::regrole::oid, '-1'::oid, 4294967295::oid \
             FROM pg_class c WHERE c.relname = 't'"
        )),
        ["t|t|99999|integer|integer|23|public|11|10|4294967295|4294967295"]
    );
    assert_eq!(
        rows_of(&last(
            &mut session,
            "SELECT format_type(1043, 124), format_type(1042, -1), format_type(1700, 655366), \
             format_type(23, NULL), format_type(9999, -1), pg_get_userbyid(10), pg_get_userbyid(999), \
             pg_encoding_to_char(6), 'abc'::name = 'abc'::text, ('1 2'::int2vector)[1], \
             '1 2'::int2vector::int2[]"
        )),
        [
            "character varying(120)|bpchar|numeric(10,2)|integer|???|eggdb|unknown (OID=999)|UTF8|t|2|[0:1]={1,2}"
        ]
    );
    for (sql, state) in [
        ("SELECT 'nope'::regclass", "42P01"),
        ("SELECT 'shop.x'::regclass", "42P01"),
        ("SELECT '4294967296'::oid", "22003"),
        ("SELECT 'x'::oid", "22P02"),
        ("SELECT 'nope'::regtype", "42704"),
        ("SELECT 'nope'::regnamespace", "3F000"),
    ] {
        assert_eq!(fails(&mut session, sql).state().code(), state, "{sql}");
    }
}

#[test]
fn the_catalog_is_read_only_and_only_the_postgresql_dialect_sees_it() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    for sql in [
        "INSERT INTO pg_class (relname) VALUES ('x')",
        "DELETE FROM pg_catalog.pg_namespace",
        "UPDATE pg_attribute SET attname = 'x'",
    ] {
        assert_eq!(fails(&mut session, sql).state().code(), "42501", "{sql}");
    }
    last(&mut session, "CREATE TABLE public.pg_class (a integer)");
    last(&mut session, "INSERT INTO public.pg_class VALUES (7)");
    assert_eq!(
        rows_of(&last(&mut session, "SELECT a FROM public.pg_class")),
        ["7"]
    );
    assert_eq!(
        rows_of(&last(
            &mut session,
            "SELECT count(*) > 1 FROM pg_class WHERE relnamespace = 'pg_catalog'::regnamespace"
        )),
        ["t"]
    );
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    assert!(run(&mut mysql, "SELECT * FROM pg_namespace").is_err());
}

#[test]
fn a_statement_loads_the_catalog_once_and_builds_each_relation_once() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    last(
        &mut session,
        "CREATE TABLE parent (id integer PRIMARY KEY, code text UNIQUE)",
    );
    last(
        &mut session,
        "CREATE TABLE child (id serial PRIMARY KEY, parent_id integer REFERENCES parent (id), note text DEFAULT 'x')",
    );
    for query in [INDEX_QUERY, COLUMN_QUERY] {
        let loads = LOADS.with(std::cell::Cell::get);
        let builds = BUILDS.with(std::cell::Cell::get);
        last(&mut session, &for_table(query, "child"));
        assert_eq!(LOADS.with(std::cell::Cell::get) - loads, 1, "{query}");
        assert!(
            BUILDS.with(std::cell::Cell::get) - builds <= 6,
            "{query}: {} relations built",
            BUILDS.with(std::cell::Cell::get) - builds
        );
    }
    last(&mut session, "CREATE SEQUENCE s");
    let loads = LOADS.with(std::cell::Cell::get);
    last(&mut session, "INSERT INTO child (parent_id) VALUES (NULL)");
    last(
        &mut session,
        "SELECT nextval('s'::regclass), currval('public.s'::regclass)",
    );
    assert_eq!(LOADS.with(std::cell::Cell::get), loads);
}

#[test]
fn describing_one_table_among_a_thousand_takes_seconds_not_minutes() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    let mut script = String::new();
    for number in 0..1000 {
        script.push_str(&format!(
            "CREATE TABLE t{number} (id integer PRIMARY KEY, name text NOT NULL, amount numeric(12,2), \
             created timestamptz, note varchar(40) DEFAULT 'x');\n"
        ));
    }
    run(&mut session, &script).unwrap();
    let started = Instant::now();
    let lookup = last(
        &mut session,
        "SELECT c.oid, n.nspname, c.relname FROM pg_catalog.pg_class c \
         LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relname OPERATOR(pg_catalog.~) '^(t500)$' COLLATE pg_catalog.default \
         AND pg_catalog.pg_table_is_visible(c.oid) ORDER BY 2, 3",
    );
    assert_eq!(rows_of(&lookup).len(), 1);
    let columns = last(&mut session, &for_table(COLUMN_QUERY, "t500"));
    assert_eq!(rows_of(&columns).len(), 5, "{columns}");
    let indexes = last(&mut session, &for_table(INDEX_QUERY, "t500"));
    assert_eq!(rows_of(&indexes).len(), 1, "{indexes}");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "three describe queries over 1,000 tables took {elapsed:?}"
    );
}
