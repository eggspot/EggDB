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

const DESCRIBE_TABLE_QUERIES: [&str; 11] = [
    "SELECT c.relchecks, c.relkind, c.relhasindex, c.relhasrules, c.relhastriggers, c.relrowsecurity, c.relforcerowsecurity, false AS relhasoids, c.relispartition, '', c.reltablespace, CASE WHEN c.reloftype = 0 THEN '' ELSE c.reloftype::pg_catalog.regtype::pg_catalog.text END, c.relpersistence, c.relreplident, am.amname
FROM pg_catalog.pg_class c
 LEFT JOIN pg_catalog.pg_class tc ON (c.reltoastrelid = tc.oid)
LEFT JOIN pg_catalog.pg_am am ON (c.relam = am.oid)
WHERE c.oid = 'OID'",
    "SELECT a.attname,
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
WHERE a.attrelid = 'OID' AND a.attnum > 0 AND NOT a.attisdropped
ORDER BY a.attnum",
    "SELECT c2.relname, i.indisprimary, i.indisunique, i.indisclustered, i.indisvalid, pg_catalog.pg_get_indexdef(i.indexrelid, 0, true),
  pg_catalog.pg_get_constraintdef(con.oid, true), contype, condeferrable, condeferred, i.indisreplident, c2.reltablespace
FROM pg_catalog.pg_class c, pg_catalog.pg_class c2, pg_catalog.pg_index i
  LEFT JOIN pg_catalog.pg_constraint con ON (conrelid = i.indrelid AND conindid = i.indexrelid AND contype IN ('p','u','x'))
WHERE c.oid = 'OID' AND c.oid = i.indrelid AND i.indexrelid = c2.oid
ORDER BY i.indisprimary DESC, c2.relname",
    "SELECT true as sametable, conname,
  pg_catalog.pg_get_constraintdef(r.oid, true) as condef,
  conrelid::pg_catalog.regclass AS ontable
FROM pg_catalog.pg_constraint r
WHERE r.conrelid = 'OID' AND r.contype = 'f'
     AND conparentid = 0
ORDER BY conname",
    "SELECT conname, conrelid::pg_catalog.regclass AS ontable,
       pg_catalog.pg_get_constraintdef(oid, true) AS condef
  FROM pg_catalog.pg_constraint c
 WHERE confrelid IN (SELECT pg_catalog.pg_partition_ancestors('OID')
                     UNION ALL VALUES ('OID'::pg_catalog.regclass))
       AND contype = 'f' AND conparentid = 0
ORDER BY conname",
    "SELECT pol.polname, pol.polpermissive,
  CASE WHEN pol.polroles = '{0}' THEN NULL ELSE pg_catalog.array_to_string(array(select rolname from pg_catalog.pg_roles where oid = any (pol.polroles) order by 1),',') END,
  pg_catalog.pg_get_expr(pol.polqual, pol.polrelid),
  pg_catalog.pg_get_expr(pol.polwithcheck, pol.polrelid),
  CASE pol.polcmd
    WHEN 'r' THEN 'SELECT'
    WHEN 'a' THEN 'INSERT'
    WHEN 'w' THEN 'UPDATE'
    WHEN 'd' THEN 'DELETE'
    END AS cmd
FROM pg_catalog.pg_policy pol
WHERE pol.polrelid = 'OID' ORDER BY 1",
    "SELECT oid, stxrelid::pg_catalog.regclass, stxnamespace::pg_catalog.regnamespace::pg_catalog.text AS nsp, stxname,
pg_catalog.pg_get_statisticsobjdef_columns(oid) AS columns,
  'd' = any(stxkind) AS ndist_enabled,
  'f' = any(stxkind) AS deps_enabled,
  'm' = any(stxkind) AS mcv_enabled,
stxstattarget
FROM pg_catalog.pg_statistic_ext
WHERE stxrelid = 'OID'
ORDER BY nsp, stxname",
    "SELECT pubname
     , NULL
     , NULL
FROM pg_catalog.pg_publication p
     JOIN pg_catalog.pg_publication_namespace pn ON p.oid = pn.pnpubid
     JOIN pg_catalog.pg_class pc ON pc.relnamespace = pn.pnnspid
WHERE pc.oid ='OID' and pg_catalog.pg_relation_is_publishable('OID')
UNION
SELECT pubname
     , pg_get_expr(pr.prqual, c.oid)
     , (CASE WHEN pr.prattrs IS NOT NULL THEN
         (SELECT string_agg(attname, ', ')
           FROM pg_catalog.generate_series(0, pg_catalog.array_upper(pr.prattrs::pg_catalog.int2[], 1)) s,
                pg_catalog.pg_attribute
          WHERE attrelid = pr.prrelid AND attnum = prattrs[s])
        ELSE NULL END) FROM pg_catalog.pg_publication p
     JOIN pg_catalog.pg_publication_rel pr ON p.oid = pr.prpubid
     JOIN pg_catalog.pg_class c ON c.oid = pr.prrelid
WHERE pr.prrelid = 'OID'
UNION
SELECT pubname
     , NULL
     , NULL
FROM pg_catalog.pg_publication p
WHERE p.puballtables AND pg_catalog.pg_relation_is_publishable('OID')
ORDER BY 1",
    "SELECT t.tgname, pg_catalog.pg_get_triggerdef(t.oid, true), t.tgenabled, t.tgisinternal,
  CASE WHEN t.tgparentid != 0 THEN
    (SELECT u.tgrelid::pg_catalog.regclass
     FROM pg_catalog.pg_trigger AS u,
          pg_catalog.pg_partition_ancestors(t.tgrelid) WITH ORDINALITY AS a(relid, depth)
     WHERE u.tgname = t.tgname AND u.tgrelid = a.relid
           AND u.tgparentid = 0
     ORDER BY a.depth LIMIT 1)
  END AS parent
FROM pg_catalog.pg_trigger t
WHERE t.tgrelid = 'OID' AND (NOT t.tgisinternal OR (t.tgisinternal AND t.tgenabled = 'D'))
ORDER BY 1",
    "SELECT c.oid::pg_catalog.regclass
FROM pg_catalog.pg_class c, pg_catalog.pg_inherits i
WHERE c.oid = i.inhparent AND i.inhrelid = 'OID'
  AND c.relkind != 'p' AND c.relkind != 'I'
ORDER BY inhseqno",
    "SELECT c.oid::pg_catalog.regclass, c.relkind, inhdetachpending, pg_catalog.pg_get_expr(c.relpartbound, c.oid)
FROM pg_catalog.pg_class c, pg_catalog.pg_inherits i
WHERE c.oid = i.inhrelid AND i.inhparent = 'OID'
ORDER BY pg_catalog.pg_get_expr(c.relpartbound, c.oid) = 'DEFAULT', c.oid::pg_catalog.regclass::pg_catalog.text",
];

fn add_copies_of_table(store: &MemoryStore, model: &str, copies: u32) {
    use crate::encoding::{TableDefinition, table_key};
    use crate::store::{KeyValueRead, KeyValueStore, KeyValueWrite};
    let reader = store.begin_read().unwrap();
    let bytes = reader.get(&table_key(model)).unwrap().unwrap();
    let definition = TableDefinition::decode(model, &bytes).unwrap();
    drop(reader);
    let mut writer = store.begin_write().unwrap();
    for number in 1..copies {
        let name = format!("t{number}");
        let mut copy = definition.clone();
        copy.id = definition.id + number * 16;
        copy.name.clone_from(&name);
        for (at, index) in copy.indexes.iter_mut().enumerate() {
            index.id = copy.id + 1 + at as u32;
            index.name = format!("{name}_u_key");
        }
        writer.put(&table_key(&name), &copy.encode()).unwrap();
    }
    writer.commit().unwrap();
}

#[test]
fn describing_one_table_among_ten_thousand_takes_seconds_not_minutes() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    last(
        &mut session,
        "CREATE TABLE t0 (id integer PRIMARY KEY, u integer UNIQUE, name text NOT NULL, \
         amount numeric(12,2), note varchar(40) DEFAULT 'x')",
    );
    add_copies_of_table(&store, "t0", 10_000);
    let started = Instant::now();
    let lookup = last(
        &mut session,
        "SELECT c.oid, n.nspname, c.relname FROM pg_catalog.pg_class c \
         LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relname OPERATOR(pg_catalog.~) '^(t5000)$' COLLATE pg_catalog.default \
         AND pg_catalog.pg_table_is_visible(c.oid) ORDER BY 2, 3",
    );
    let found = rows_of(&lookup);
    assert_eq!(found.len(), 1, "{lookup}");
    let oid = found[0].split('|').next().unwrap_or_default().to_owned();
    let mut shown = Vec::new();
    for query in DESCRIBE_TABLE_QUERIES {
        shown.push(last(&mut session, &query.replace("OID", &oid)));
    }
    let elapsed = started.elapsed();
    assert_eq!(rows_of(&shown[1]).len(), 5, "{}", shown[1]);
    assert_eq!(
        rows_of(&shown[2]),
        [
            "t5000_pkey|t|t|f|t|CREATE UNIQUE INDEX t5000_pkey ON t5000 USING btree (id)|PRIMARY KEY (id)|p|f|f|f|0",
            "t5000_u_key|f|t|f|t|CREATE UNIQUE INDEX t5000_u_key ON t5000 USING btree (u)|UNIQUE (u)|u|f|f|f|0"
        ]
    );
    assert!(
        elapsed < Duration::from_secs(120),
        "psql's queries for \\d of one table among 10,000 took {elapsed:?}"
    );
}

#[test]
fn stored_defaults_checks_and_views_read_the_catalog_when_they_need_it() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    last(
        &mut session,
        "CREATE TABLE customers (id integer PRIMARY KEY); CREATE SEQUENCE customers_id_seq",
    );
    last(
        &mut session,
        "CREATE TABLE d (v integer, t text DEFAULT 'customers'::regclass::text, \
         o text DEFAULT CAST('customers' AS pg_catalog.regclass)::text)",
    );
    last(&mut session, "INSERT INTO d (v) VALUES (1)");
    last(&mut session, "INSERT INTO d (v) VALUES (1 + 1)");
    assert_eq!(
        rows_of(&last(&mut session, "SELECT v, t, o FROM d ORDER BY v")),
        ["1|customers|customers", "2|customers|customers"]
    );
    last(
        &mut session,
        "CREATE TABLE named (n text CHECK (n::regclass::oid > 0))",
    );
    last(&mut session, "INSERT INTO named VALUES ('customers')");
    assert_eq!(
        fails(&mut session, "INSERT INTO named VALUES ('nosuch')")
            .state()
            .code(),
        "42P01"
    );
    last(
        &mut session,
        "CREATE VIEW shown AS SELECT 'customers'::regclass::text AS r",
    );
    assert_eq!(
        rows_of(&last(&mut session, "SELECT r FROM shown")),
        ["customers"]
    );
    last(
        &mut session,
        "CREATE VIEW names AS SELECT relname::text AS r FROM pg_class WHERE relname = 'd'",
    );
    assert_eq!(rows_of(&last(&mut session, "SELECT r FROM names")), ["d"]);
    let refused = fails(
        &mut session,
        "CREATE VIEW typed AS SELECT relname FROM pg_class",
    );
    assert_eq!(refused.state().code(), "0A000");
    assert!(refused.message().contains("type name"), "{refused}");
    last(
        &mut session,
        "CREATE TABLE serial_cast (id bigint DEFAULT nextval(CAST('customers_id_seq' AS regclass)), v integer)",
    );
    let loads = LOADS.with(std::cell::Cell::get);
    last(&mut session, "INSERT INTO serial_cast (v) VALUES (1)");
    last(&mut session, "INSERT INTO serial_cast (v) VALUES (2)");
    assert_eq!(LOADS.with(std::cell::Cell::get), loads);
    assert_eq!(
        rows_of(&last(
            &mut session,
            "SELECT nextval(CAST('customers_id_seq' AS regclass)), \
             nextval(CAST('customers_id_seq' AS pg_catalog.regclass)), \
             currval('customers_id_seq'::\"regclass\")"
        )),
        ["3|4|4"]
    );
    assert_eq!(LOADS.with(std::cell::Cell::get), loads);
    assert_eq!(
        rows_of(&last(
            &mut session,
            "SELECT id, v FROM serial_cast ORDER BY v"
        )),
        ["1|1", "2|2"]
    );
}

#[test]
fn reg_inputs_outputs_and_errors_follow_postgresql_16() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    last(
        &mut session,
        "CREATE TABLE customers (id integer PRIMARY KEY, note text DEFAULT 'x')",
    );
    assert_eq!(
        rows_of(&last(
            &mut session,
            "SELECT '0'::regclass, 0::regtype, '-'::regnamespace, 0::regrole, '-'::regproc, \
             '-'::regclass::oid, 'customers'::\"regclass\", regclass('customers'), \
             \"regclass\"('customers'), pg_catalog.regtype('int4')"
        )),
        ["-|-|-|-|-|0|customers|customers|customers|integer"]
    );
    assert_eq!(
        rows_of(&last(
            &mut session,
            "SELECT 'character varying(10)'::regtype, 'varchar(10)[]'::regtype, \
             'numeric(12,2)'::regtype, 'timestamp(3) with time zone'::regtype, \
             'time(2)'::regtype, 'float(10)'::regtype, 'float(40)'::regtype, \
             'interval year to month'::regtype, 'pg_catalog.varchar(10)'::regtype, \
             'char(2)'::regtype, 'INT4'::regtype, 'int4[]'::regtype"
        )),
        [
            "character varying|character varying[]|numeric|timestamp with time zone|time without time zone|real|double precision|interval|character varying|character|integer|integer[]"
        ]
    );
    assert_eq!(
        rows_of(&last(
            &mut session,
            "SELECT 'double precision[3]'::regtype, 'int4[3]'::regtype, 'int4 [ 3 ] [ ]'::regtype, \
             'varchar(10)[3]'::regtype, 'numeric(5,6)'::regtype, '02353'::regclass, \
             '00000000000000000000000000000002353'::regclass, '027'::regtype, '010'::oid, \
             '0x1F'::oid, '-2147483648'::oid, 'customers'::regclass::regclass"
        )),
        [
            "double precision[]|integer[]|integer[]|character varying[]|numeric|pg_class|pg_class|integer|8|31|2147483648|customers"
        ]
    );
    for (sql, state, message) in [
        (
            "SELECT 'a.b.c.d'::regclass",
            "42601",
            "improper relation name (too many dotted names): a.b.c.d",
        ),
        (
            "SELECT 'a.b.c'::regclass",
            "0A000",
            "cross-database references are not implemented: \"a.b.c\"",
        ),
        (
            "SELECT 'a.b.c.d'::regtype",
            "42601",
            "improper qualified name (too many dotted names): a.b.c.d",
        ),
        ("SELECT 'a.b'::regnamespace", "42602", "invalid name syntax"),
        (
            "SELECT '99999999999'::regclass",
            "22003",
            "value \"99999999999\" is out of range for type oid",
        ),
        (
            "SELECT 'int4(10)'::regtype",
            "42601",
            "type modifier is not allowed for type \"int4\"",
        ),
        (
            "SELECT 'nosuch(10)'::regtype",
            "42704",
            "type \"nosuch\" does not exist",
        ),
        (
            "SELECT 'varchar(x)'::regtype",
            "42601",
            "syntax error at or near \"x\"",
        ),
        (
            "SELECT nextval('customers'::regclass)",
            "42809",
            "\"customers\" is not a sequence",
        ),
        (
            "SELECT currval('customers')",
            "42809",
            "\"customers\" is not a sequence",
        ),
        (
            "CREATE TABLE wrong (id bigint DEFAULT nextval('customers'::regclass))",
            "42809",
            "\"customers\" is not a sequence",
        ),
        (
            "SELECT nextval('nosuch'::regclass)",
            "42P01",
            "relation \"nosuch\" does not exist",
        ),
        (
            "SELECT nextval('customers_pkey')",
            "42809",
            "\"customers_pkey\" is not a sequence",
        ),
        (
            "SELECT nextval('pg_class')",
            "42809",
            "\"pg_class\" is not a sequence",
        ),
        (
            "SELECT setval('pg_catalog.pg_class', 1)",
            "42809",
            "\"pg_class\" is not a sequence",
        ),
        (
            "SELECT 'pg_class'::regclass::regtype",
            "42846",
            "cannot cast type regclass to regtype",
        ),
        (
            "SELECT 'integer'::regtype::regclass",
            "42846",
            "cannot cast type regtype to regclass",
        ),
        (
            "SELECT 'numeric(1,2,3)'::regtype",
            "22023",
            "invalid NUMERIC type modifier",
        ),
        (
            "SELECT 'numeric(0)'::regtype",
            "22023",
            "NUMERIC precision 0 must be between 1 and 1000",
        ),
        (
            "SELECT 'numeric(5,1001)'::regtype",
            "22023",
            "NUMERIC scale 1001 must be between -1000 and 1000",
        ),
        (
            "SELECT 'numeric(99999999999)'::regtype",
            "22003",
            "value \"99999999999\" is out of range for type integer",
        ),
        (
            "SELECT 'varchar(0)'::regtype",
            "22023",
            "length for type varchar must be at least 1",
        ),
        (
            "SELECT 'character(0)'::regtype",
            "22023",
            "length for type char must be at least 1",
        ),
        (
            "SELECT 'varchar(10485761)'::regtype",
            "22023",
            "length for type varchar cannot exceed 10485760",
        ),
        (
            "SELECT 'varchar(1,2)'::regtype",
            "42601",
            "syntax error at or near \",\"",
        ),
        (
            "SELECT 'float(54)'::regtype",
            "22023",
            "precision for type float must be less than 54 bits",
        ),
        (
            "SELECT 'float(0)'::regtype",
            "22023",
            "precision for type float must be at least 1 bit",
        ),
        (
            "SELECT 'int4[]['::regtype",
            "42601",
            "syntax error at end of input",
        ),
        (
            "SELECT 'int4[]x'::regtype",
            "42601",
            "syntax error at or near \"x\"",
        ),
        (
            "SELECT 'int4[-1]'::regtype",
            "42601",
            "syntax error at or near \"-\"",
        ),
        (
            "SELECT ' 23 '::regtype",
            "42601",
            "syntax error at or near \"23\"",
        ),
        (
            "SELECT '23x'::regtype",
            "42601",
            "trailing junk after numeric literal at or near \"23x\"",
        ),
        (
            "SELECT '00000000000000000000001259'::regclass",
            "22P02",
            "invalid input syntax for type oid: \"00000000000000000000001259\"",
        ),
        (
            "SELECT ' 1259 '::regclass",
            "42P01",
            "relation \"1259\" does not exist",
        ),
        (
            "SELECT '-1'::regclass",
            "42P01",
            "relation \"-1\" does not exist",
        ),
        (
            "SELECT '08'::oid",
            "22P02",
            "invalid input syntax for type oid: \"08\"",
        ),
        (
            "SELECT '0x'::oid",
            "22P02",
            "invalid input syntax for type oid: \"0x\"",
        ),
        (
            "SELECT '040000000000'::oid",
            "22003",
            "value \"040000000000\" is out of range for type oid",
        ),
        (
            "SELECT pg_get_expr('abc', 1259)",
            "0A000",
            "cannot accept a value of type pg_node_tree",
        ),
        (
            "SELECT 'abc'::pg_node_tree",
            "0A000",
            "cannot accept a value of type pg_node_tree",
        ),
        (
            "SELECT pg_get_expr('abc'::text, 1259)",
            "42883",
            "function pg_get_expr(text, integer) does not exist",
        ),
        (
            "SELECT pg_get_expr(adbin::text, adrelid) FROM pg_attrdef",
            "42883",
            "function pg_get_expr(text, oid) does not exist",
        ),
    ] {
        let error = fails(&mut session, sql);
        assert_eq!(
            (error.state().code(), error.message()),
            (state, message),
            "{sql}"
        );
    }
    assert_eq!(
        rows_of(&last(
            &mut session,
            "SELECT pg_get_expr(NULL, 1259) IS NULL"
        )),
        ["t"]
    );
}

#[test]
fn changing_a_catalog_relation_is_refused_as_postgresql_refuses_it() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    for sql in [
        "DROP TABLE pg_class",
        "DROP TABLE IF EXISTS pg_catalog.pg_class",
        "ALTER TABLE pg_class ADD COLUMN x integer",
        "COMMENT ON TABLE pg_class IS 'x'",
        "COMMENT ON COLUMN pg_class.relname IS 'x'",
        "CREATE INDEX ON pg_class (relname)",
        "DROP VIEW pg_roles",
    ] {
        let error = fails(&mut session, sql);
        assert_eq!(error.state().code(), "42501", "{sql}");
        assert!(
            error.message().ends_with("is a system catalog"),
            "{sql}: {error}"
        );
    }
    last(&mut session, "CREATE TABLE public.pg_class (a integer)");
    last(&mut session, "DROP TABLE public.pg_class");
}
