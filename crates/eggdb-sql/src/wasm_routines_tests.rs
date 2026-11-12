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
