use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Session, SqlError};

fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

fn answer(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => output,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn refusal(session: &mut Session<'_>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error,
    }
}

const LOG: &str = "CREATE TABLE ddl_log (seq serial PRIMARY KEY, event text, tag text)";
const LOGGER: &str = "CREATE FUNCTION log_ddl() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO ddl_log (event, tag) VALUES (TG_EVENT, TG_TAG);
END
$$";

#[test]
fn start_and_end_triggers_see_the_event_and_the_tag() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, LOG);
    answer(&mut session, LOGGER);
    answer(
        &mut session,
        "CREATE EVENT TRIGGER a_start ON ddl_command_start EXECUTE FUNCTION log_ddl()",
    );
    answer(
        &mut session,
        "CREATE EVENT TRIGGER b_end ON ddl_command_end WHEN TAG IN ('create table') EXECUTE FUNCTION log_ddl()",
    );
    answer(&mut session, "CREATE TABLE t (a integer)");
    answer(&mut session, "CREATE INDEX t_a ON t (a)");
    assert_eq!(
        answer(&mut session, "SELECT event, tag FROM ddl_log ORDER BY seq"),
        answer(
            &mut session,
            "VALUES ('ddl_command_start', 'CREATE TABLE'), ('ddl_command_end', 'CREATE TABLE'), \
             ('ddl_command_start', 'CREATE INDEX')"
        )
        .replace("column1", "event")
        .replace("column2", "tag")
    );
}

#[test]
fn a_failing_end_trigger_undoes_the_command() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE FUNCTION no_tables() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'no new tables';
END
$$",
    );
    answer(
        &mut session,
        "CREATE EVENT TRIGGER guard ON ddl_command_end WHEN TAG IN ('CREATE TABLE') EXECUTE FUNCTION no_tables()",
    );
    let error = refusal(&mut session, "CREATE TABLE t (a integer)");
    assert!(error.to_string().contains("no new tables"));
    assert!(refusal(&mut session, "SELECT * FROM t").to_string().contains("does not exist"));
    answer(&mut session, "ALTER EVENT TRIGGER guard DISABLE");
    answer(&mut session, "CREATE TABLE t (a integer)");
}

#[test]
fn sql_drop_lists_the_dropped_objects() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, "CREATE TABLE dropped (kind text, identity text)");
    answer(
        &mut session,
        "CREATE FUNCTION note_drops() RETURNS event_trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO dropped SELECT object_type, object_identity FROM pg_event_trigger_dropped_objects();
END
$$",
    );
    answer(
        &mut session,
        "CREATE EVENT TRIGGER drops ON sql_drop EXECUTE FUNCTION note_drops()",
    );
    answer(&mut session, "CREATE TABLE gone (a integer)");
    answer(&mut session, "DROP TABLE gone");
    assert!(answer(&mut session, "SELECT kind, identity FROM dropped").contains("table | public.gone"));
}

#[test]
fn event_trigger_functions_are_refused_outside_triggers_and_definitions_are_checked() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, LOG);
    answer(&mut session, LOGGER);
    assert!(refusal(&mut session, "SELECT log_ddl()")
        .to_string()
        .contains("can only be called as triggers"));
    assert!(refusal(
        &mut session,
        "CREATE EVENT TRIGGER e ON ddl_command_middle EXECUTE FUNCTION log_ddl()"
    )
    .to_string()
    .contains("unrecognized event name"));
    assert!(refusal(
        &mut session,
        "CREATE EVENT TRIGGER e ON ddl_command_start WHEN TAG IN ('CREATE DATABASE') EXECUTE FUNCTION log_ddl()"
    )
    .to_string()
    .contains("not supported for CREATE DATABASE"));
    assert!(refusal(
        &mut session,
        "CREATE EVENT TRIGGER e ON ddl_command_start EXECUTE FUNCTION missing()"
    )
    .to_string()
    .contains("does not exist"));
    answer(
        &mut session,
        "CREATE EVENT TRIGGER e ON ddl_command_start EXECUTE FUNCTION log_ddl()",
    );
    assert!(refusal(
        &mut session,
        "CREATE EVENT TRIGGER e ON ddl_command_start EXECUTE FUNCTION log_ddl()"
    )
    .to_string()
    .contains("already exists"));
    assert!(answer(&mut session, "SELECT evtname, evtevent, evtenabled FROM pg_event_trigger")
        .contains("e | ddl_command_start | O"));
    answer(&mut session, "DROP EVENT TRIGGER e");
    assert!(answer(&mut session, "DROP EVENT TRIGGER IF EXISTS e").contains("DROP EVENT TRIGGER"));
}
