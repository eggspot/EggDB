use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Session, SqlError};

fn session(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::new(store);
    session.identify("eggdb", "shapes");
    session
}

fn run(session: &mut Session<'_>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn last(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn rows(session: &mut Session<'_>, sql: &str) -> Vec<String> {
    let rendered = last(session, sql);
    let lines: Vec<&str> = rendered.lines().collect();
    lines
        .get(1..lines.len().saturating_sub(1))
        .unwrap_or(&[])
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
}

fn setup(session: &mut Session<'_>, statements: &[&str]) {
    for statement in statements {
        last(session, statement);
    }
}

const PSQL_DT_PLUS_QUERY: &str = "SELECT n.nspname as \"Schema\",
  c.relname as \"Name\",
  CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'i' THEN 'index' WHEN 'S' THEN 'sequence' WHEN 't' THEN 'TOAST table' WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' WHEN 'I' THEN 'partitioned index' END as \"Type\",
  pg_catalog.pg_get_userbyid(c.relowner) as \"Owner\",
  CASE c.relpersistence WHEN 'p' THEN 'permanent' WHEN 't' THEN 'temporary' WHEN 'u' THEN 'unlogged' END as \"Persistence\",
  am.amname as \"Access method\",
  pg_catalog.pg_size_pretty(pg_catalog.pg_table_size(c.oid)) as \"Size\",
  pg_catalog.obj_description(c.oid, 'pg_class') as \"Description\"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam
WHERE c.relkind IN ('r','p','')
      AND n.nspname <> 'pg_catalog'
      AND n.nspname !~ '^pg_toast'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1,2;";

const PSQL_D_PLUS_COLUMN_QUERY: &str = "SELECT a.attname,
  pg_catalog.format_type(a.atttypid, a.atttypmod),
  (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true)
   FROM pg_catalog.pg_attrdef d
   WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef),
  a.attnotnull,
  (SELECT c.collname FROM pg_catalog.pg_collation c, pg_catalog.pg_type t
   WHERE c.oid = a.attcollation AND t.oid = a.atttypid AND a.attcollation <> t.typcollation) AS attcollation,
  a.attidentity,
  a.attgenerated,
  a.attstorage,
  a.attcompression AS attcompression,
  CASE WHEN a.attstattarget=-1 THEN NULL ELSE a.attstattarget END AS attstattarget,
  pg_catalog.col_description(a.attrelid, a.attnum)
FROM pg_catalog.pg_attribute a
WHERE a.attrelid = 'items'::regclass AND a.attnum > 0 AND NOT a.attisdropped
ORDER BY a.attnum;";

const PSQL_D_PLUS_CHILDREN_QUERY: &str = "SELECT c.oid::pg_catalog.regclass, c.relkind, inhdetachpending, pg_catalog.pg_get_expr(c.relpartbound, c.oid)
FROM pg_catalog.pg_class c, pg_catalog.pg_inherits i
WHERE c.oid = i.inhrelid AND i.inhparent = 'items'::regclass
ORDER BY pg_catalog.pg_get_expr(c.relpartbound, c.oid) = 'DEFAULT', c.oid::pg_catalog.regclass::pg_catalog.text;";

const PSQL_DF_PLUS_QUERY: &str = "SELECT n.nspname as \"Schema\",
  p.proname as \"Name\",
  pg_catalog.pg_get_function_result(p.oid) as \"Result data type\",
  pg_catalog.pg_get_function_arguments(p.oid) as \"Argument data types\",
 CASE p.prokind
  WHEN 'a' THEN 'agg'
  WHEN 'w' THEN 'window'
  WHEN 'p' THEN 'proc'
  ELSE 'func'
 END as \"Type\",
 CASE
  WHEN p.provolatile = 'i' THEN 'immutable'
  WHEN p.provolatile = 's' THEN 'stable'
  WHEN p.provolatile = 'v' THEN 'volatile'
 END as \"Volatility\",
 CASE
  WHEN p.proparallel = 'r' THEN 'restricted'
  WHEN p.proparallel = 's' THEN 'safe'
  WHEN p.proparallel = 'u' THEN 'unsafe'
 END as \"Parallel\",
 pg_catalog.pg_get_userbyid(p.proowner) as \"Owner\",
 CASE WHEN prosecdef THEN 'definer' ELSE 'invoker' END AS \"Security\",
 pg_catalog.array_to_string(p.proacl, E'\\n') AS \"Access privileges\",
 l.lanname as \"Language\",
 CASE WHEN l.lanname IN ('internal', 'c') THEN p.prosrc END as \"Internal name\",
 pg_catalog.obj_description(p.oid, 'pg_proc') as \"Description\"
FROM pg_catalog.pg_proc p
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
     LEFT JOIN pg_catalog.pg_language l ON l.oid = p.prolang
WHERE pg_catalog.pg_function_is_visible(p.oid)
      AND n.nspname <> 'pg_catalog'
      AND n.nspname <> 'information_schema'
ORDER BY 1, 2, 4;";

const PSQL_DT_TYPES_QUERY: &str = "SELECT n.nspname as \"Schema\",
  pg_catalog.format_type(t.oid, NULL) AS \"Name\",
  pg_catalog.obj_description(t.oid, 'pg_type') as \"Description\"
FROM pg_catalog.pg_type t
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
WHERE (t.typrelid = 0 OR (SELECT c.relkind = 'c' FROM pg_catalog.pg_class c WHERE c.oid = t.typrelid))
  AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_type el WHERE el.oid = t.typelem AND el.typarray = t.oid)
      AND n.nspname <> 'pg_catalog'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_type_is_visible(t.oid)
ORDER BY 1, 2;";

const PSQL_DT_TYPES_PLUS_QUERY: &str = "SELECT n.nspname as \"Schema\",
  pg_catalog.format_type(t.oid, NULL) AS \"Name\",
  t.typname AS \"Internal name\",
  CASE WHEN t.typrelid != 0
      THEN CAST('tuple' AS pg_catalog.text)
    WHEN t.typlen < 0
      THEN CAST('var' AS pg_catalog.text)
    ELSE CAST(t.typlen AS pg_catalog.text)
  END AS \"Size\",
  pg_catalog.array_to_string(
      ARRAY(
          SELECT e.enumlabel
          FROM pg_catalog.pg_enum e
          WHERE e.enumtypid = t.oid
          ORDER BY e.enumsortorder
      ),
      E'\\n'
  ) AS \"Elements\",
  pg_catalog.pg_get_userbyid(t.typowner) AS \"Owner\",
pg_catalog.array_to_string(t.typacl, E'\\n') AS \"Access privileges\",
    pg_catalog.obj_description(t.oid, 'pg_type') as \"Description\"
FROM pg_catalog.pg_type t
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
WHERE (t.typrelid = 0 OR (SELECT c.relkind = 'c' FROM pg_catalog.pg_class c WHERE c.oid = t.typrelid))
  AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_type el WHERE el.oid = t.typelem AND el.typarray = t.oid)
      AND n.nspname <> 'pg_catalog'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_type_is_visible(t.oid)
ORDER BY 1, 2;";

const PSQL_DD_DOMAINS_QUERY: &str = "SELECT n.nspname as \"Schema\",
       t.typname as \"Name\",
       pg_catalog.format_type(t.typbasetype, t.typtypmod) as \"Type\",
       (SELECT c.collname FROM pg_catalog.pg_collation c, pg_catalog.pg_type bt
        WHERE c.oid = t.typcollation AND bt.oid = t.typbasetype AND t.typcollation <> bt.typcollation) as \"Collation\",
       CASE WHEN t.typnotnull THEN 'not null' END as \"Nullable\",
       t.typdefault as \"Default\",
       pg_catalog.array_to_string(ARRAY(
         SELECT pg_catalog.pg_get_constraintdef(r.oid, true) FROM pg_catalog.pg_constraint r WHERE t.oid = r.contypid
       ), ' ') as \"Check\"
FROM pg_catalog.pg_type t
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
WHERE t.typtype = 'd'
      AND n.nspname <> 'pg_catalog'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_type_is_visible(t.oid)
ORDER BY 1, 2;";

const PSQL_DP_QUERY: &str = "SELECT n.nspname as \"Schema\",
  c.relname as \"Name\",
  CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'S' THEN 'sequence' WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' END as \"Type\",
  pg_catalog.array_to_string(c.relacl, E'\\n') AS \"Access privileges\",
  pg_catalog.array_to_string(ARRAY(
    SELECT attname || E':\\n  ' || pg_catalog.array_to_string(attacl, E'\\n  ')
    FROM pg_catalog.pg_attribute a
    WHERE attrelid = c.oid AND NOT attisdropped AND attacl IS NOT NULL
  ), E'\\n') AS \"Column privileges\",
  pg_catalog.array_to_string(ARRAY(
    SELECT polname
    || CASE WHEN NOT polpermissive THEN
       E' (RESTRICTIVE)'
       ELSE '' END
    || CASE WHEN polcmd != '*' THEN
           E' (' || polcmd::pg_catalog.text || E'):'
       ELSE E':'
       END
    || CASE WHEN polqual IS NOT NULL THEN
           E'\\n  (u): ' || pg_catalog.pg_get_expr(polqual, polrelid)
       ELSE E''
       END
    || CASE WHEN polwithcheck IS NOT NULL THEN
           E'\\n  (c): ' || pg_catalog.pg_get_expr(polwithcheck, polrelid)
       ELSE E''
       END    || CASE WHEN polroles <> '{0}' THEN
           E'\\n  to: ' || pg_catalog.array_to_string(
               ARRAY(
                   SELECT rolname
                   FROM pg_catalog.pg_roles
                   WHERE oid = ANY (polroles)
                   ORDER BY 1
               ), E', ')
       ELSE E''
       END
    FROM pg_catalog.pg_policy pol
    WHERE polrelid = c.oid), E'\\n')
    AS \"Policies\"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r','v','m','S','f','p')
  AND n.nspname !~ '^pg_' AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1, 2;";

const PSQL_DX_QUERY: &str = "SELECT e.extname AS \"Name\", e.extversion AS \"Version\", n.nspname AS \"Schema\", c.description AS \"Description\"
FROM pg_catalog.pg_extension e LEFT JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace LEFT JOIN pg_catalog.pg_description c ON c.objoid = e.oid AND c.classoid = 'pg_catalog.pg_extension'::pg_catalog.regclass
ORDER BY 1;";

const PSQL_DD_QUERY: &str = "SELECT DISTINCT tt.nspname AS \"Schema\", tt.name AS \"Name\", tt.object AS \"Object\", d.description AS \"Description\"
FROM (
  SELECT pgc.oid as oid, pgc.tableoid AS tableoid,
  n.nspname as nspname,
  CAST(pgc.conname AS pg_catalog.text) as name,  CAST('table constraint' AS pg_catalog.text) as object
  FROM pg_catalog.pg_constraint pgc
    JOIN pg_catalog.pg_class c ON c.oid = pgc.conrelid
    LEFT JOIN pg_catalog.pg_namespace n     ON n.oid = c.relnamespace
WHERE n.nspname <> 'pg_catalog'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_table_is_visible(c.oid)
UNION ALL
  SELECT pgc.oid as oid, pgc.tableoid AS tableoid,
  n.nspname as nspname,
  CAST(pgc.conname AS pg_catalog.text) as name,  CAST('domain constraint' AS pg_catalog.text) as object
  FROM pg_catalog.pg_constraint pgc
    JOIN pg_catalog.pg_type t ON t.oid = pgc.contypid
    LEFT JOIN pg_catalog.pg_namespace n     ON n.oid = t.typnamespace
WHERE n.nspname <> 'pg_catalog'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_type_is_visible(t.oid)
UNION ALL
  SELECT o.oid as oid, o.tableoid as tableoid,
  n.nspname as nspname,
  CAST(o.opcname AS pg_catalog.text) as name,
  CAST('operator class' AS pg_catalog.text) as object
  FROM pg_catalog.pg_opclass o
    JOIN pg_catalog.pg_am am ON o.opcmethod = am.oid
    JOIN pg_catalog.pg_namespace n ON n.oid = o.opcnamespace
      AND n.nspname <> 'pg_catalog'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_opclass_is_visible(o.oid)
UNION ALL
  SELECT opf.oid as oid, opf.tableoid as tableoid,
  n.nspname as nspname,
  CAST(opf.opfname AS pg_catalog.text) AS name,
  CAST('operator family' AS pg_catalog.text) as object
  FROM pg_catalog.pg_opfamily opf
    JOIN pg_catalog.pg_am am ON opf.opfmethod = am.oid
    JOIN pg_catalog.pg_namespace n ON opf.opfnamespace = n.oid
      AND n.nspname <> 'pg_catalog'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_opfamily_is_visible(opf.oid)
UNION ALL
  SELECT r.oid as oid, r.tableoid as tableoid,
  n.nspname as nspname,
  CAST(r.rulename AS pg_catalog.text) as name,  CAST('rule' AS pg_catalog.text) as object
  FROM pg_catalog.pg_rewrite r
       JOIN pg_catalog.pg_class c ON c.oid = r.ev_class
       LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
  WHERE r.rulename != '_RETURN'
      AND n.nspname <> 'pg_catalog'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_table_is_visible(c.oid)
UNION ALL
  SELECT t.oid as oid, t.tableoid as tableoid,
  n.nspname as nspname,
  CAST(t.tgname AS pg_catalog.text) as name,  CAST('trigger' AS pg_catalog.text) as object
  FROM pg_catalog.pg_trigger t
       JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid
       LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname <> 'pg_catalog'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_table_is_visible(c.oid)
) AS tt
  JOIN pg_catalog.pg_description d ON (tt.oid = d.objoid AND tt.tableoid = d.classoid AND d.objsubid = 0)
ORDER BY 1, 2, 3;";

const PSQL_DB_QUERY: &str = "SELECT spcname AS \"Name\",
  pg_catalog.pg_get_userbyid(spcowner) AS \"Owner\",
  pg_catalog.pg_tablespace_location(oid) AS \"Location\"
FROM pg_catalog.pg_tablespace
ORDER BY 1;";

const PSQL_DB_PLUS_QUERY: &str = "SELECT spcname AS \"Name\",
  pg_catalog.pg_get_userbyid(spcowner) AS \"Owner\",
  pg_catalog.pg_tablespace_location(oid) AS \"Location\",
  pg_catalog.array_to_string(spcacl, E'\\n') AS \"Access privileges\",
  spcoptions AS \"Options\",
  pg_catalog.pg_size_pretty(pg_catalog.pg_tablespace_size(oid)) AS \"Size\",
  pg_catalog.shobj_description(oid, 'pg_tablespace') AS \"Description\"
FROM pg_catalog.pg_tablespace
ORDER BY 1;";

const PSQL_DC_PLUS_QUERY: &str = "SELECT pg_catalog.format_type(castsource, NULL) AS \"Source type\",
       pg_catalog.format_type(casttarget, NULL) AS \"Target type\",
       CASE WHEN c.castmethod = 'b' THEN '(binary coercible)'
            WHEN c.castmethod = 'i' THEN '(with inout)'
            ELSE p.proname
       END AS \"Function\",
       CASE WHEN c.castcontext = 'e' THEN 'no'
            WHEN c.castcontext = 'a' THEN 'in assignment'
            ELSE 'yes'
       END AS \"Implicit?\",
       d.description AS \"Description\"
FROM pg_catalog.pg_cast c LEFT JOIN pg_catalog.pg_proc p
     ON c.castfunc = p.oid
     LEFT JOIN pg_catalog.pg_type ts
     ON c.castsource = ts.oid
     LEFT JOIN pg_catalog.pg_namespace ns
     ON ns.oid = ts.typnamespace
     LEFT JOIN pg_catalog.pg_type tt
     ON c.casttarget = tt.oid
     LEFT JOIN pg_catalog.pg_namespace nt
     ON nt.oid = tt.typnamespace
     LEFT JOIN pg_catalog.pg_description d
     ON d.classoid = c.tableoid AND d.objoid = c.oid AND d.objsubid = 0
WHERE ( (true  AND pg_catalog.pg_type_is_visible(ts.oid)
) OR (true  AND pg_catalog.pg_type_is_visible(tt.oid)
) )
ORDER BY 1, 2;";

const PSQL_DY_QUERY: &str = "SELECT evtname as \"Name\", evtevent as \"Event\", pg_catalog.pg_get_userbyid(e.evtowner) as \"Owner\",
 case evtenabled when 'O' then 'enabled'  when 'R' then 'replica'  when 'A' then 'always'  when 'D' then 'disabled' end as \"Enabled\",
 COALESCE(pg_catalog.array_to_string(array(select x from pg_catalog.unnest(evttags) as t(x)), ', '), '') as \"Tags\"
FROM pg_catalog.pg_event_trigger e ORDER BY 1";

const PSQL_DRP_QUERY: &str = "SELECT pubname AS \"Name\",
  pg_catalog.pg_get_userbyid(pubowner) AS \"Owner\",
  puballtables AS \"All tables\",
  pubinsert AS \"Inserts\",
  pubupdate AS \"Updates\",
  pubdelete AS \"Deletes\",
  pubtruncate AS \"Truncates\",
  pubviaroot AS \"Via root\"
FROM pg_catalog.pg_publication
ORDER BY 1;";

const PSQL_DRS_QUERY: &str = "SELECT subname AS \"Name\"
,  pg_catalog.pg_get_userbyid(subowner) AS \"Owner\"
,  subenabled AS \"Enabled\"
,  subpublications AS \"Publication\"
FROM pg_catalog.pg_subscription
WHERE subdbid = (SELECT oid
                 FROM pg_catalog.pg_database
                 WHERE datname = pg_catalog.current_database())
ORDER BY 1;";

#[test]
fn psql_dt_plus_and_d_plus_show_owner_access_method_storage_and_comments() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    setup(
        &mut session,
        &[
            "CREATE TABLE items (id integer PRIMARY KEY, name text, price numeric(10,2))",
            "COMMENT ON TABLE items IS 'stock items'",
            "COMMENT ON COLUMN items.name IS 'shown name'",
        ],
    );
    assert_eq!(
        rows(&mut session, PSQL_DT_PLUS_QUERY),
        ["public|items|table|eggdb|permanent|heap|0 bytes|stock items"]
    );
    assert_eq!(
        rows(&mut session, PSQL_D_PLUS_COLUMN_QUERY),
        [
            "id|integer|NULL|t|NULL|||p||NULL|NULL",
            "name|text|NULL|f|NULL|||x||NULL|shown name",
            "price|numeric(10,2)|NULL|f|NULL|||m||NULL|NULL",
        ]
    );
    assert!(rows(&mut session, PSQL_D_PLUS_CHILDREN_QUERY).is_empty());
}

#[test]
fn psql_df_plus_shows_volatility_owner_security_and_language() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    setup(
        &mut session,
        &[
            "CREATE FUNCTION add_one(a integer) RETURNS integer LANGUAGE sql IMMUTABLE \
             AS $$ SELECT a + 1 $$",
            "CREATE FUNCTION audit_it(note text) RETURNS void LANGUAGE plpgsql SECURITY DEFINER \
             AS $$ BEGIN END $$",
        ],
    );
    assert_eq!(
        rows(&mut session, PSQL_DF_PLUS_QUERY),
        [
            "public|add_one|integer|a integer|func|immutable|unsafe|eggdb|invoker|NULL|sql|NULL|NULL",
            "public|audit_it|void|note text|func|volatile|unsafe|eggdb|definer|NULL|plpgsql|NULL|NULL",
        ]
    );
}

#[test]
fn psql_dt_lists_user_types_and_its_plus_form_their_size_and_labels() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    setup(
        &mut session,
        &[
            "CREATE TYPE mood AS ENUM ('sad', 'happy')",
            "CREATE DOMAIN posint AS integer NOT NULL DEFAULT 1 CHECK (VALUE > 0)",
            "CREATE TYPE pair AS (a integer, b text)",
        ],
    );
    assert_eq!(
        rows(&mut session, PSQL_DT_TYPES_QUERY),
        ["public|mood|NULL", "public|pair|NULL", "public|posint|NULL"]
    );
    let verbose = last(&mut session, PSQL_DT_TYPES_PLUS_QUERY);
    assert!(
        verbose.contains("public|mood|mood|4|sad\nhappy|eggdb|NULL|NULL"),
        "{verbose}"
    );
    assert!(
        verbose.contains("public|posint|posint|4||eggdb|NULL|NULL"),
        "{verbose}"
    );
}

#[test]
fn psql_dd_lists_domains_with_their_checks_from_pg_constraint() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    setup(
        &mut session,
        &[
            "CREATE DOMAIN posint AS integer NOT NULL DEFAULT 1 CHECK (VALUE > 0)",
            "ALTER DOMAIN posint ADD CONSTRAINT below_100 CHECK (VALUE < 100)",
            "CREATE DOMAIN code AS varchar(3)",
        ],
    );
    assert_eq!(
        rows(&mut session, PSQL_DD_DOMAINS_QUERY),
        [
            "public|code|character varying(3)|NULL|NULL|NULL|",
            "public|posint|integer|NULL|not null|1|CHECK (VALUE > 0) CHECK (VALUE < 100)",
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT conname, contype, conrelid, pg_catalog.pg_get_constraintdef(oid, true) \
             FROM pg_catalog.pg_constraint \
             WHERE contypid = (SELECT oid FROM pg_catalog.pg_type WHERE typname = 'posint') \
             ORDER BY conname"
        ),
        [
            "below_100|c|0|CHECK (VALUE < 100)",
            "posint_check|c|0|CHECK (VALUE > 0)",
        ]
    );
}

#[test]
fn psql_dp_shows_grants_as_postgresql_access_privileges() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    setup(
        &mut session,
        &[
            "CREATE TABLE accounts (id integer)",
            "CREATE TABLE notes (id integer)",
            "CREATE ROLE clerk",
            "GRANT SELECT, INSERT ON accounts TO clerk",
            "GRANT SELECT ON accounts TO PUBLIC",
        ],
    );
    let listed = last(&mut session, PSQL_DP_QUERY);
    assert!(
        listed.contains("public|accounts|table|eggdb=arwdDxt/eggdb\nclerk=ar/eggdb\n=r/eggdb||\n"),
        "{listed}"
    );
    assert!(listed.contains("public|notes|table|NULL||\n"), "{listed}");
    assert_eq!(
        rows(
            &mut session,
            "SELECT relname, relacl FROM pg_catalog.pg_class \
             WHERE relname IN ('accounts', 'notes') ORDER BY relname"
        ),
        [
            "accounts|{eggdb=arwdDxt/eggdb,clerk=ar/eggdb,=r/eggdb}",
            "notes|NULL",
        ]
    );
    last(&mut session, "REVOKE SELECT, INSERT ON accounts FROM clerk");
    assert_eq!(
        rows(
            &mut session,
            "SELECT relacl FROM pg_catalog.pg_class WHERE relname = 'accounts'"
        ),
        ["{eggdb=arwdDxt/eggdb,=r/eggdb}"]
    );
}

#[test]
fn acl_items_quote_role_names_as_postgresql_does() {
    let grant = |grantee: &str| {
        crate::roles::TableGrant {
            table: "t".to_owned(),
            grantee: grantee.to_owned(),
            privileges: 1,
        }
        .acl_item("eggdb")
    };
    assert_eq!(grant("clerk"), "clerk=r/eggdb");
    assert_eq!(grant("public"), "=r/eggdb");
    assert_eq!(grant("Mixed Case"), "\"Mixed Case\"=r/eggdb");
    assert_eq!(grant("say\"hi"), "\"say\"\"hi\"=r/eggdb");
    assert_eq!(
        crate::roles::owner_acl_item("app-owner"),
        "\"app-owner\"=arwdDxt/\"app-owner\""
    );
}

#[test]
fn psql_dx_lists_extensions_with_their_descriptions() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    assert_eq!(
        rows(&mut session, PSQL_DX_QUERY),
        [
            "citext|1.6|public|data type for case-insensitive character strings",
            "hstore|1.8|public|data type for storing sets of (key, value) pairs",
            "plpgsql|1.0|pg_catalog|PL/pgSQL procedural language",
        ]
    );
}

#[test]
fn psql_dd_finds_constraint_comments_through_tableoid() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    setup(
        &mut session,
        &[
            "CREATE TABLE stock (id integer, qty integer CONSTRAINT qty_positive CHECK (qty > 0))",
            "COMMENT ON CONSTRAINT qty_positive ON stock IS 'no negative stock'",
            "COMMENT ON TABLE stock IS 'not an object psql dd lists'",
        ],
    );
    assert_eq!(
        rows(&mut session, PSQL_DD_QUERY),
        ["public|qty_positive|table constraint|no negative stock"]
    );
}

#[test]
fn psql_db_dc_dy_drp_and_drs_answer_like_a_fresh_postgresql_16() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    assert_eq!(
        rows(&mut session, PSQL_DB_QUERY),
        ["pg_default|eggdb|", "pg_global|eggdb|"]
    );
    assert_eq!(
        rows(&mut session, PSQL_DB_PLUS_QUERY),
        [
            "pg_default|eggdb||NULL|NULL|0 bytes|NULL",
            "pg_global|eggdb||NULL|NULL|0 bytes|NULL",
        ]
    );
    assert!(rows(&mut session, PSQL_DC_PLUS_QUERY).is_empty());
    assert!(rows(&mut session, PSQL_DY_QUERY).is_empty());
    assert!(rows(&mut session, PSQL_DRP_QUERY).is_empty());
    assert!(rows(&mut session, PSQL_DRS_QUERY).is_empty());
}

#[test]
fn catalog_tables_answer_tableoid_without_showing_it_in_star() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT DISTINCT c.tableoid FROM pg_catalog.pg_class c"
        ),
        ["1259"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT tableoid::regclass FROM pg_catalog.pg_namespace LIMIT 1"
        ),
        ["pg_namespace"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT count(*) FROM pg_catalog.pg_extension WHERE tableoid = 'pg_catalog.pg_extension'::regclass"
        ),
        ["3"]
    );
    let star = last(
        &mut session,
        "SELECT * FROM pg_catalog.pg_tablespace WHERE false",
    );
    assert_eq!(
        star.lines().next(),
        Some("oid|spcname|spcowner|spcacl|spcoptions")
    );
    assert!(run(&mut session, "SELECT tableoid FROM pg_catalog.pg_roles").is_err());
}
