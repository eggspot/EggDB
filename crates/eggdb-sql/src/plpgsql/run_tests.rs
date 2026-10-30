use super::{Call, Returned, TriggerCall, invoke, run_inline, take_raised_notices};
use crate::Session;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::memory_store::MemoryStore;
use crate::records::{RECORD_OID, record_value};
use crate::settings::Settings;
use crate::store::{KeyValueStore, KeyValueWrite};
use crate::types::Value;
use std::sync::Arc;

fn setup(store: &MemoryStore, statements: &[&str]) {
    let mut session = Session::new(store);
    for statement in statements {
        if let Err(error) = session.execute(statement, &mut |_| {}) {
            panic!("{statement}: {error}");
        }
    }
}

fn invoked(
    store: &MemoryStore,
    name: &str,
    arguments: Vec<Value>,
    trigger: Option<TriggerCall>,
) -> SqlResult<(Returned, Vec<String>)> {
    let _types = crate::user_types::enter_if_absent();
    let mut writer = store.begin_write()?;
    let mut tables = TableCache::default();
    let settings = Settings::default();
    let Some(named) = crate::routines::resolve(&writer, &mut tables, name)? else {
        return Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, name.to_owned()));
    };
    let definition = Arc::clone(&named.routines[0]);
    let call = Call {
        definition: &definition,
        arguments,
        trigger,
    };
    let returned = invoke(&mut writer, &mut tables, &settings, call);
    let notices = take_raised_notices();
    let returned = returned?;
    writer.commit()?;
    Ok((returned, notices))
}

fn call(store: &MemoryStore, name: &str, arguments: Vec<Value>) -> SqlResult<Returned> {
    invoked(store, name, arguments, None).map(|(returned, _)| returned)
}

fn value(store: &MemoryStore, name: &str, arguments: Vec<Value>) -> Value {
    match call(store, name, arguments) {
        Ok(Returned::Value(value)) => value,
        other => panic!("{name}: {other:?}"),
    }
}

fn failure(store: &MemoryStore, name: &str, arguments: Vec<Value>) -> SqlError {
    match call(store, name, arguments) {
        Ok(returned) => panic!("{name} returned {returned:?}"),
        Err(error) => error,
    }
}

fn inline(store: &MemoryStore, code: &str) -> SqlResult<Vec<String>> {
    let _types = crate::user_types::enter_if_absent();
    let mut writer = store.begin_write()?;
    let mut tables = TableCache::default();
    let settings = Settings::default();
    let outcome = run_inline(&mut writer, &mut tables, &settings, code);
    let notices = take_raised_notices();
    outcome?;
    writer.commit()?;
    Ok(notices)
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

fn row(fields: Vec<Value>, names: &[&str]) -> Value {
    record_value(
        RECORD_OID,
        fields,
        names.iter().map(|name| (*name).to_owned()).collect(),
    )
}

const TABLE: &str = "CREATE TABLE t (id integer PRIMARY KEY, note text)";
const COUNTED: &str =
    "CREATE FUNCTION counted() RETURNS bigint LANGUAGE sql AS 'SELECT count(*) FROM t'";

#[test]
fn loops_conditions_and_variables_compute_a_result() {
    let store = MemoryStore::default();
    setup(
        &store,
        &["CREATE FUNCTION add_odd(n integer) RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
  total integer := 0;
BEGIN
  FOR i IN 1..n LOOP
    CONTINUE WHEN i % 2 = 0;
    total := total + i;
  END LOOP;
  WHILE total > 100 LOOP
    total := total - 100;
  END LOOP;
  RETURN total;
END $$"],
    );
    assert_eq!(value(&store, "add_odd", vec![Value::Integer(5)]), Value::Integer(9));
    assert_eq!(value(&store, "add_odd", vec![Value::Integer(0)]), Value::Integer(0));
    assert_eq!(value(&store, "add_odd", vec![Value::Integer(21)]), Value::Integer(21));
}

#[test]
fn an_exception_handler_undoes_the_blocks_writes_and_keeps_its_variables() {
    let store = MemoryStore::default();
    setup(
        &store,
        &[TABLE, COUNTED, "CREATE FUNCTION guarded() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  x integer := 1;
BEGIN
  INSERT INTO t VALUES (1, 'kept');
  BEGIN
    INSERT INTO t VALUES (2, 'undone');
    x := 2;
    PERFORM 1 / 0;
  EXCEPTION WHEN division_by_zero THEN
    RETURN 'caught ' || SQLSTATE || ' x=' || x;
  END;
  RETURN 'not reached';
END $$"],
    );
    assert_eq!(value(&store, "guarded", Vec::new()), text("caught 22012 x=2"));
    assert_eq!(value(&store, "counted", Vec::new()), Value::BigInt(1));
}

#[test]
fn an_uncaught_error_leaves_the_handler_and_reports_its_line() {
    let store = MemoryStore::default();
    setup(
        &store,
        &["CREATE FUNCTION noisy(a integer) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  RAISE NOTICE 'value is %', a;
  IF a < 0 THEN
    RAISE EXCEPTION 'negative: %', a USING ERRCODE = 'invalid_parameter_value', HINT = 'use a positive number';
  END IF;
END $$"],
    );
    let (returned, notices) =
        invoked(&store, "noisy", vec![Value::Integer(1)], None).expect("noisy(1)");
    assert_eq!(returned, Returned::Value(Value::Null));
    assert_eq!(notices, vec!["NOTICE:  value is 1".to_owned()]);
    let error = failure(&store, "noisy", vec![Value::Integer(-1)]);
    assert_eq!(error.state(), SqlState::INVALID_PARAMETER_VALUE);
    assert_eq!(error.message(), "negative: -1");
    assert_eq!(error.hint(), Some("use a positive number"));
    let context = error.context().unwrap_or_default();
    assert!(context.contains("PL/pgSQL function noisy(integer) line 5 at RAISE"), "{context}");
}

#[test]
fn select_into_strict_finds_exactly_one_row() {
    let store = MemoryStore::default();
    setup(
        &store,
        &[TABLE, "INSERT INTO t VALUES (1, 'one'), (2, 'two')", "CREATE FUNCTION lookup(wanted integer) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  found_note text;
BEGIN
  SELECT note INTO STRICT found_note FROM t WHERE id = wanted;
  RETURN found_note;
END $$", "CREATE FUNCTION any_note() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  found_note text;
BEGIN
  SELECT note INTO STRICT found_note FROM t;
  RETURN found_note;
END $$", "CREATE FUNCTION loose(wanted integer) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE
  found_note text;
BEGIN
  SELECT note INTO found_note FROM t WHERE id = wanted;
  RETURN FOUND;
END $$"],
    );
    assert_eq!(value(&store, "lookup", vec![Value::Integer(1)]), text("one"));
    let missing = failure(&store, "lookup", vec![Value::Integer(99)]);
    assert_eq!(missing.state(), SqlState::NO_DATA_FOUND);
    assert_eq!(missing.message(), "query returned no rows");
    assert_eq!(failure(&store, "any_note", Vec::new()).state(), SqlState::TOO_MANY_ROWS);
    assert_eq!(value(&store, "loose", vec![Value::Integer(2)]), Value::Boolean(true));
    assert_eq!(value(&store, "loose", vec![Value::Integer(9)]), Value::Boolean(false));
}

#[test]
fn set_returning_functions_return_next_rows_and_query_rows() {
    let store = MemoryStore::default();
    setup(
        &store,
        &[TABLE, "INSERT INTO t VALUES (2, 'b'), (1, 'a')", "CREATE FUNCTION squares(n integer) RETURNS TABLE(x integer, y integer) LANGUAGE plpgsql AS $$
BEGIN
  FOR i IN 1..n LOOP
    x := i;
    y := i * i;
    RETURN NEXT;
  END LOOP;
END $$", "CREATE FUNCTION notes() RETURNS SETOF text LANGUAGE plpgsql AS $$
BEGIN
  RETURN QUERY SELECT note FROM t ORDER BY id;
  RETURN NEXT 'z';
END $$", "CREATE FUNCTION sql_notes() RETURNS SETOF text LANGUAGE sql AS 'SELECT note FROM t ORDER BY id DESC'"],
    );
    assert_eq!(
        call(&store, "squares", vec![Value::Integer(3)]),
        Ok(Returned::Set(vec![
            row(vec![Value::Integer(1), Value::Integer(1)], &["x", "y"]),
            row(vec![Value::Integer(2), Value::Integer(4)], &["x", "y"]),
            row(vec![Value::Integer(3), Value::Integer(9)], &["x", "y"]),
        ]))
    );
    assert_eq!(
        call(&store, "notes", Vec::new()),
        Ok(Returned::Set(vec![text("a"), text("b"), text("z")]))
    );
    assert_eq!(
        call(&store, "sql_notes", Vec::new()),
        Ok(Returned::Set(vec![text("b"), text("a")]))
    );
}

#[test]
fn cursors_open_fetch_and_close() {
    let store = MemoryStore::default();
    setup(
        &store,
        &[TABLE, "INSERT INTO t VALUES (3, 'c'), (1, 'a'), (2, 'b')", "CREATE FUNCTION walk() RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  c CURSOR FOR SELECT id FROM t ORDER BY id;
  v integer;
  seen text := '';
BEGIN
  OPEN c;
  LOOP
    FETCH c INTO v;
    EXIT WHEN NOT FOUND;
    seen := seen || v || ',';
  END LOOP;
  CLOSE c;
  RETURN seen;
END $$", "CREATE FUNCTION walk_from(low integer) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
  c CURSOR (lowest integer) FOR SELECT id, note FROM t WHERE id >= lowest ORDER BY id;
  seen text := '';
BEGIN
  FOR r IN c(low) LOOP
    seen := seen || r.note;
  END LOOP;
  RETURN seen;
END $$"],
    );
    assert_eq!(value(&store, "walk", Vec::new()), text("1,2,3,"));
    assert_eq!(value(&store, "walk_from", vec![Value::Integer(2)]), text("bc"));
}

#[test]
fn dynamic_sql_runs_with_using_values_and_counts_rows() {
    let store = MemoryStore::default();
    setup(
        &store,
        &[TABLE, "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')", "CREATE FUNCTION dynamic_count(minimum integer) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
  counted bigint;
  changed bigint;
BEGIN
  EXECUTE 'SELECT count(*) FROM t WHERE id >= $1' INTO counted USING minimum;
  UPDATE t SET note = 'seen' WHERE id >= minimum;
  GET DIAGNOSTICS changed = ROW_COUNT;
  RETURN counted * 100 + changed;
END $$"],
    );
    assert_eq!(value(&store, "dynamic_count", vec![Value::Integer(2)]), Value::BigInt(202));
}

#[test]
fn sql_functions_read_their_parameters_by_name_and_number() {
    let store = MemoryStore::default();
    setup(
        &store,
        &[
            "CREATE FUNCTION plus(a integer, b integer) RETURNS integer LANGUAGE sql AS 'SELECT $1 + b'",
            "CREATE FUNCTION twice(a integer) RETURNS integer LANGUAGE sql RETURN a * 2",
            "CREATE FUNCTION greeting(who text DEFAULT 'world') RETURNS text LANGUAGE sql AS $$ SELECT 'hello ' || who $$",
        ],
    );
    assert_eq!(
        value(&store, "plus", vec![Value::Integer(2), Value::Integer(3)]),
        Value::Integer(5)
    );
    assert_eq!(value(&store, "twice", vec![Value::Integer(21)]), Value::Integer(42));
    assert_eq!(value(&store, "greeting", Vec::new()), text("hello world"));
}

#[test]
fn do_blocks_write_and_raise_notices() {
    let store = MemoryStore::default();
    setup(&store, &[TABLE, COUNTED]);
    let notices = inline(
        &store,
        "DECLARE n integer := 2; BEGIN INSERT INTO t VALUES (n, 'from do'); RAISE NOTICE 'done %', n; RAISE WARNING 'careful'; END",
    )
    .expect("DO");
    assert_eq!(
        notices,
        vec!["NOTICE:  done 2".to_owned(), "WARNING:  careful".to_owned()]
    );
    assert_eq!(value(&store, "counted", Vec::new()), Value::BigInt(1));
}

#[test]
fn records_case_and_constants_fail_like_postgresql() {
    let store = MemoryStore::default();
    setup(
        &store,
        &["CREATE FUNCTION unassigned() RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
  r record;
BEGIN
  RETURN r.id;
END $$", "CREATE FUNCTION pick(n integer) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
  CASE n
    WHEN 1, 2 THEN RETURN 'small';
    WHEN 3 THEN RETURN 'three';
  END CASE;
END $$", "CREATE FUNCTION no_return() RETURNS integer LANGUAGE plpgsql AS $$
BEGIN
  NULL;
END $$"],
    );
    let unassigned = failure(&store, "unassigned", Vec::new());
    assert_eq!(unassigned.state(), SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE);
    assert_eq!(unassigned.message(), "record \"r\" is not assigned yet");
    assert_eq!(value(&store, "pick", vec![Value::Integer(2)]), text("small"));
    assert_eq!(value(&store, "pick", vec![Value::Integer(3)]), text("three"));
    assert_eq!(failure(&store, "pick", vec![Value::Integer(4)]).state(), SqlState::CASE_NOT_FOUND);
    assert_eq!(
        failure(&store, "no_return", Vec::new()).state(),
        SqlState::FUNCTION_EXECUTED_NO_RETURN_STATEMENT
    );
}

#[test]
fn trigger_functions_change_new_and_read_trigger_variables() {
    let store = MemoryStore::default();
    setup(
        &store,
        &["CREATE FUNCTION stamp() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  NEW.note := upper(NEW.note) || ' by ' || TG_OP || ' ' || TG_NARGS || ' ' || TG_ARGV[0];
  RETURN NEW;
END $$"],
    );
    assert_eq!(failure(&store, "stamp", Vec::new()).state(), SqlState::FEATURE_NOT_SUPPORTED);
    let trigger = TriggerCall {
        new: Some(row(vec![Value::Integer(1), text("x")], &["id", "note"])),
        old: None,
        name: "stamp_rows".to_owned(),
        when: "BEFORE",
        level: "ROW",
        operation: "INSERT",
        relation_oid: 16_384,
        table_name: "t".to_owned(),
        table_schema: "public".to_owned(),
        arguments: vec!["tag".to_owned()],
    };
    let (returned, _) = invoked(&store, "stamp", Vec::new(), Some(trigger)).expect("stamp");
    assert_eq!(
        returned,
        Returned::Value(row(vec![Value::Integer(1), text("X by INSERT 1 tag")], &["id", "note"]))
    );
}

#[test]
fn hostile_bodies_fail_with_errors_instead_of_crashing() {
    let store = MemoryStore::default();
    let cases = [
        ("BEGIN EXECUTE NULL; END", SqlState::NULL_VALUE_NOT_ALLOWED),
        (
            "DECLARE a integer[] := ARRAY[1]; BEGIN a[2000000000] := 1; END",
            SqlState::PROGRAM_LIMIT_EXCEEDED,
        ),
        ("BEGIN PERFORM 1 / 0; END", SqlState::DIVISION_BY_ZERO),
        (
            "BEGIN FOR i IN 1..3 BY 0 LOOP NULL; END LOOP; END",
            SqlState::INVALID_PARAMETER_VALUE,
        ),
        ("DECLARE x integer NOT NULL := 1; BEGIN x := NULL; END", SqlState::NULL_VALUE_NOT_ALLOWED),
        ("DECLARE x CONSTANT integer := 1; BEGIN SELECT 2 INTO x; END", SqlState::ERROR_IN_ASSIGNMENT),
        ("BEGIN SELECT 1; END", SqlState::SYNTAX_ERROR),
        ("BEGIN RAISE; END", SqlState::STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER),
        ("BEGIN ASSERT 1 = 2, 'broken'; END", SqlState::ASSERT_FAILURE),
        ("DECLARE c refcursor; BEGIN CLOSE c; END", SqlState::INVALID_CURSOR_NAME),
    ];
    for (code, state) in cases {
        match inline(&store, code) {
            Ok(notices) => panic!("{code} ran: {notices:?}"),
            Err(error) => assert_eq!(error.state(), state, "{code}: {error}"),
        }
    }
    for garbage in ["", "BEGIN", "BEGIN END", "x := ;", "BEGIN LOOP END; END", "\u{0}"] {
        let _ = inline(&store, garbage);
    }
}
