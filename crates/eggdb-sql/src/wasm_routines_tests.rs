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
