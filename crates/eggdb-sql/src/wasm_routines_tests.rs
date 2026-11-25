use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Session, SqlError};

const ADD_MODULE: &str = "\\x0061736d010000000107016002\
7f7f017f030201000707010361646400000a09010700200020016a0b";

fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

#[test]
fn a_wasm_function_adds_its_arguments() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        &format!(
            "CREATE FUNCTION wadd(a integer, b integer) RETURNS integer LANGUAGE wasm AS '{ADD_MODULE}', 'add'"
        ),
    )
    .expect("created");
    let answer = run(&mut session, "SELECT wadd(40, 2) AS sum").expect("called");
    assert!(answer.contains("42"), "{answer}");
    let answer = run(&mut session, "SELECT wadd(NULL, 2) IS NULL AS missing").expect("called");
    assert!(answer.contains('t'), "{answer}");
}

#[test]
fn mismatched_declarations_and_missing_exports_are_refused() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let mismatch = run(
        &mut session,
        &format!("CREATE FUNCTION wbad(a bigint) RETURNS integer LANGUAGE wasm AS '{ADD_MODULE}', 'add'"),
    )
    .expect_err("types differ");
    assert!(mismatch.to_string().contains("does not match"), "{mismatch}");
    let missing = run(
        &mut session,
        &format!(
            "CREATE FUNCTION wmissing(a integer, b integer) RETURNS integer LANGUAGE wasm AS '{ADD_MODULE}', 'sub'"
        ),
    )
    .expect_err("no such export");
    assert!(missing.to_string().contains("exports no function"), "{missing}");
    let text = run(
        &mut session,
        "CREATE FUNCTION wtext(a text) RETURNS text LANGUAGE wasm AS '\\x00', 'f'",
    )
    .expect_err("text is not a wasm type");
    assert!(text.to_string().contains("not supported") || text.to_string().contains("malformed"), "{text}");
    let plain = run(
        &mut session,
        "CREATE FUNCTION wc(a integer) RETURNS integer LANGUAGE sql AS 'lib.so', 'symbol'",
    )
    .expect_err("object files");
    assert!(plain.to_string().contains("loadable object file"), "{plain}");
}

const ECHO_MODULE: &str = "\\x0061736d01000000010d0260017f017f60027f7f027f7f030302000105030100010716020b65676764625f616c6c6f630000046563686f00010a0e0205004180080b0600200020010b";

#[test]
fn text_and_bytea_pass_through_the_modules_memory() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        &format!("CREATE FUNCTION wecho(t text) RETURNS text LANGUAGE wasm AS '{ECHO_MODULE}', 'echo'"),
    )
    .expect("created");
    run(
        &mut session,
        &format!("CREATE FUNCTION wbytes(b bytea) RETURNS bytea LANGUAGE wasm AS '{ECHO_MODULE}', 'echo'"),
    )
    .expect("created");
    let answer = run(&mut session, "SELECT wecho('héllo') AS echoed").expect("called");
    assert!(answer.contains("héllo"), "{answer}");
    let answer = run(&mut session, "SELECT wbytes('\\x0102ff'::bytea) = '\\x0102ff'::bytea AS same").expect("called");
    assert!(answer.contains('t'), "{answer}");
    let refused = run(
        &mut session,
        &format!("CREATE FUNCTION wshort(t text) RETURNS integer LANGUAGE wasm AS '{ECHO_MODULE}', 'echo'"),
    )
    .expect_err("one result is not two");
    assert!(refused.to_string().contains("does not match"), "{refused}");
}

const NOW_MODULE: &str = "\\x0061736d010000000105016000017e\
0214010565676764620a6e6f775f6d6963726f730000\
0302010007070103\
6e6f7700010a06010400\
10000b";

const FOREIGN_IMPORT_MODULE: &str = "\\x0061736d010000000105016000017e\
020d0103656e76056c6f63616c0000\
0302010007070103\
6e6f7700010a06010400\
10000b";

#[test]
fn modules_import_the_host_functions_eggdb_provides_only() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        &format!("CREATE FUNCTION wnow() RETURNS bigint LANGUAGE wasm AS '{NOW_MODULE}', 'now'"),
    )
    .expect("created");
    let answer = run(
        &mut session,
        "SELECT wnow() = (extract(epoch FROM now()) * 1000000)::bigint AS same",
    )
    .expect("called");
    assert!(answer.contains('t'), "{answer}");
    let refused = run(
        &mut session,
        &format!("CREATE FUNCTION wlocal() RETURNS bigint LANGUAGE wasm AS '{FOREIGN_IMPORT_MODULE}', 'now'"),
    )
    .expect_err("env.local is not provided");
    assert!(refused.to_string().contains("does not provide"), "{refused}");
}

const SERIES_MODULE: &str = "\\x0061736d0100000001080160027f7f027f7f03020100070a0106736572696573\
00000a0e010c002001200048200141016a0b";

#[test]
fn a_set_returning_wasm_function_yields_rows_until_its_flag_says_stop() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        &format!(
            "CREATE FUNCTION wseries(n integer) RETURNS SETOF integer LANGUAGE wasm AS '{SERIES_MODULE}', 'series'"
        ),
    )
    .expect("created");
    assert_eq!(
        run(&mut session, "SELECT * FROM wseries(3) AS s(v)").expect("called"),
        "v\n1\n2\n3\nSELECT 3"
    );
    assert_eq!(
        run(&mut session, "SELECT count(*) AS n FROM wseries(0)").expect("called"),
        "n\n0\nSELECT 1"
    );
    assert!(run(
        &mut session,
        &format!("CREATE FUNCTION wrong(n integer) RETURNS SETOF bigint LANGUAGE wasm AS '{SERIES_MODULE}', 'series'"),
    )
    .is_err());
}

#[test]
fn a_roles_wasm_ceiling_and_the_sessions_settings_only_lower_the_limits() {
    let defaults = eggdb_wasm_vm::Limits::default();
    let mut settings = crate::settings::Settings::default();
    assert_eq!(super::limits_for(&settings), defaults);
    settings.wasm_ceiling = super::WasmCeiling::of_settings(&[
        (super::FUEL_SETTING.to_owned(), "5000".to_owned()),
        (super::MEMORY_PAGES_SETTING.to_owned(), "not a number".to_owned()),
        ("search_path".to_owned(), "public".to_owned()),
    ]);
    let limited = super::limits_for(&settings);
    assert_eq!(limited.fuel, 5000);
    assert_eq!(limited.max_memory_pages, defaults.max_memory_pages);
    settings.set(super::FUEL_SETTING, Some("900000000")).unwrap();
    settings.set(super::CALL_DEPTH_SETTING, Some("8")).unwrap();
    let limited = super::limits_for(&settings);
    assert_eq!(limited.fuel, 5000);
    assert_eq!(limited.max_call_depth, 8);
    settings.set(super::FUEL_SETTING, Some("100")).unwrap();
    assert_eq!(super::limits_for(&settings).fuel, 100);
}

#[test]
fn a_role_with_a_small_wasm_fuel_ceiling_traps_where_others_finish() {
    let store = MemoryStore::default();
    let mut admin = Session::new(&store);
    run(
        &mut admin,
        &format!("CREATE FUNCTION wseries(n integer) RETURNS SETOF integer LANGUAGE wasm AS '{SERIES_MODULE}', 'series'"),
    )
    .expect("created");
    run(&mut admin, "CREATE USER tight LOGIN").expect("role");
    run(&mut admin, "ALTER ROLE tight SET eggdb.wasm_fuel = 2").expect("ceiling");
    let mut tight = Session::new(&store);
    tight.identify_role("tight", "eggdb").expect("login");
    let error = run(&mut tight, "SELECT count(*) FROM wseries(2)").expect_err("trapped");
    assert_eq!(error.state().code(), "38000", "{error}");
    assert_eq!(run(&mut admin, "SELECT count(*) AS n FROM wseries(2)").expect("ran"), "n\n2\nSELECT 1");
}

const SQUARES_MODULE: &str = "\\x0061736d0100000001090160027f7f037f7f7e03020100070b0107737175617265730000\
0a1b0119002001200048200141016a200141016aac200141016aac7e0b";

#[test]
fn a_wasm_function_returning_a_table_yields_a_column_per_value() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        &format!(
            "CREATE FUNCTION wsquares(n integer) RETURNS TABLE (base integer, square bigint) LANGUAGE wasm AS '{SQUARES_MODULE}', 'squares'"
        ),
    )
    .expect("created");
    assert_eq!(
        run(&mut session, "SELECT base, square FROM wsquares(3) ORDER BY base").expect("called"),
        "base|square\n1|1\n2|4\n3|9\nSELECT 3"
    );
    assert!(run(
        &mut session,
        &format!("CREATE FUNCTION wrong_table(n integer) RETURNS TABLE (base integer, square integer) LANGUAGE wasm AS '{SQUARES_MODULE}', 'squares'"),
    )
    .is_err());
}
