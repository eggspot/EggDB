use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Session, SqlError};

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

fn rows(session: &mut Session<'_, MemoryStore>, sql: &str) -> Vec<String> {
    let rendered = last(session, sql);
    let lines: Vec<&str> = rendered.lines().collect();
    lines
        .get(1..lines.len().saturating_sub(1))
        .unwrap_or(&[])
        .iter()
        .map(|line| (*line).to_owned())
        .collect()
}

const PSQL_DF_QUERY: &str = "SELECT n.nspname as \"Schema\",
  p.proname as \"Name\",
  pg_catalog.pg_get_function_result(p.oid) as \"Result data type\",
  pg_catalog.pg_get_function_arguments(p.oid) as \"Argument data types\",
 CASE p.prokind
  WHEN 'a' THEN 'agg'
  WHEN 'w' THEN 'window'
  WHEN 'p' THEN 'proc'
  ELSE 'func'
 END as \"Type\"
FROM pg_catalog.pg_proc p
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
WHERE pg_catalog.pg_function_is_visible(p.oid)
      AND n.nspname <> 'pg_catalog'
      AND n.nspname <> 'information_schema'
ORDER BY 1, 2, 4;";

fn with_routines(session: &mut Session<'_, MemoryStore>) {
    for statement in [
        "CREATE FUNCTION add_one(a integer, b integer DEFAULT 1) RETURNS integer \
         LANGUAGE sql IMMUTABLE STRICT AS $$ SELECT a + b $$",
        "CREATE FUNCTION pairs(n integer) RETURNS TABLE(k integer, v text) LANGUAGE plpgsql AS $$ \
         BEGIN FOR i IN 1..n LOOP k := i; v := i::text; RETURN NEXT; END LOOP; END $$",
        "CREATE PROCEDURE bump(INOUT counter integer) LANGUAGE plpgsql AS $$ \
         BEGIN counter := counter + 1; END $$",
        "CREATE SCHEMA tools",
        "CREATE FUNCTION tools.hidden() RETURNS void LANGUAGE plpgsql AS $$ BEGIN END $$",
    ] {
        last(session, statement);
    }
}

#[test]
fn psql_df_lists_user_functions_and_procedures_as_postgresql_16_does() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    with_routines(&mut session);
    assert_eq!(
        rows(&mut session, PSQL_DF_QUERY),
        [
            "public|add_one|integer|a integer, b integer DEFAULT 1|func",
            "public|bump|NULL|INOUT counter integer|proc",
            "public|pairs|TABLE(k integer, v text)|n integer|func",
        ]
    );
    last(&mut session, "DROP FUNCTION add_one(integer, integer)");
    assert_eq!(
        rows(&mut session, PSQL_DF_QUERY),
        [
            "public|bump|NULL|INOUT counter integer|proc",
            "public|pairs|TABLE(k integer, v text)|n integer|func",
        ]
    );
}

#[test]
fn function_visibility_follows_the_search_path() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    with_routines(&mut session);
    let visibility = "SELECT p.proname, pg_function_is_visible(p.oid) FROM pg_proc p \
                      WHERE p.proname = 'hidden'";
    assert_eq!(rows(&mut session, visibility), ["hidden|f"]);
    assert_eq!(
        rows(
            &mut session,
            "SELECT 'tools.hidden'::regproc, 'add_one'::regproc::oid = \
             (SELECT oid FROM pg_proc WHERE proname = 'add_one')"
        ),
        ["tools.hidden|t"]
    );
    last(&mut session, "SET search_path = tools, public");
    assert_eq!(rows(&mut session, visibility), ["hidden|t"]);
}

#[test]
fn pg_get_functiondef_writes_a_create_or_replace_statement() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    with_routines(&mut session);
    assert_eq!(
        last(
            &mut session,
            "SELECT pg_get_functiondef(oid) FROM pg_proc WHERE proname = 'add_one'"
        ),
        "pg_get_functiondef\n\
         CREATE OR REPLACE FUNCTION public.add_one(a integer, b integer DEFAULT 1)\n \
         RETURNS integer\n \
         LANGUAGE sql\n \
         IMMUTABLE\n \
         STRICT\n\
         AS $function$ SELECT a + b $function$\n\n\
         SELECT 1"
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT pg_get_function_identity_arguments(oid), pg_get_function_result(oid) \
             FROM pg_proc WHERE proname IN ('add_one', 'bump') ORDER BY proname"
        ),
        ["a integer, b integer|integer", "INOUT counter integer|NULL"]
    );
}

#[test]
fn pg_proc_and_pg_language_describe_user_routines() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    with_routines(&mut session);
    assert_eq!(
        rows(
            &mut session,
            "SELECT proname, prokind, proretset, provolatile, proisstrict, pronargs, \
             pronargdefaults, proargmodes, proargnames, prorettype \
             FROM pg_proc WHERE proname IN ('add_one', 'pairs') ORDER BY proname"
        ),
        [
            "add_one|f|f|i|t|2|1|NULL|{a,b}|23",
            "pairs|f|t|v|f|1|0|{i,t,t}|{n,k,v}|2249",
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT lanname, lanispl, lanpltrusted FROM pg_language ORDER BY oid"
        ),
        ["internal|f|f", "c|f|f", "sql|f|t", "plpgsql|t|t"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT p.proname, l.lanname FROM pg_proc p JOIN pg_language l ON l.oid = p.prolang \
             WHERE p.proname IN ('add_one', 'pairs', 'bump') ORDER BY 1"
        ),
        ["add_one|sql", "bump|plpgsql", "pairs|plpgsql"]
    );
}

#[test]
fn information_schema_lists_routines_and_their_parameters() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    with_routines(&mut session);
    assert_eq!(
        rows(
            &mut session,
            "SELECT routine_name, routine_type, data_type, type_udt_name, routine_body, \
             external_language, is_deterministic, security_type \
             FROM information_schema.routines WHERE routine_schema = 'public' \
             ORDER BY routine_name"
        ),
        [
            "add_one|FUNCTION|integer|int4|SQL|SQL|YES|INVOKER",
            "bump|PROCEDURE|NULL|NULL|EXTERNAL|PLPGSQL|NO|INVOKER",
            "pairs|FUNCTION|record|record|EXTERNAL|PLPGSQL|NO|INVOKER",
        ]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT r.routine_name, p.ordinal_position, p.parameter_mode, p.parameter_name, \
             p.data_type, p.parameter_default \
             FROM information_schema.parameters p \
             JOIN information_schema.routines r ON r.specific_name = p.specific_name \
             WHERE r.routine_name IN ('add_one', 'pairs') ORDER BY 1, 2"
        ),
        [
            "add_one|1|IN|a|integer|NULL",
            "add_one|2|IN|b|integer|1",
            "pairs|1|IN|n|integer|NULL",
            "pairs|2|OUT|k|integer|NULL",
            "pairs|3|OUT|v|text|NULL",
        ]
    );
}

#[test]
fn triggers_name_their_function_and_routines_know_their_oid() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    for statement in [
        "CREATE TABLE notes (id integer PRIMARY KEY, body text)",
        "CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
        "CREATE TRIGGER notes_touch BEFORE INSERT ON notes FOR EACH ROW EXECUTE FUNCTION touch()",
        "CREATE FUNCTION my_oid() RETURNS oid LANGUAGE plpgsql AS $$ \
         DECLARE found oid; BEGIN GET DIAGNOSTICS found = PG_ROUTINE_OID; RETURN found; END $$",
    ] {
        last(&mut session, statement);
    }
    assert_eq!(
        rows(
            &mut session,
            "SELECT t.tgname, p.proname FROM pg_trigger t JOIN pg_proc p ON p.oid = t.tgfoid"
        ),
        ["notes_touch|touch"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT my_oid() = (SELECT oid FROM pg_proc WHERE proname = 'my_oid')"
        ),
        ["t"]
    );
}
