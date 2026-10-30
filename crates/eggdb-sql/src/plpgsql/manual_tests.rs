use crate::error::SqlState;
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Session, SqlError};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| {
        let mut shown = render(&result);
        for notice in &result.notices {
            shown.push_str("\nnotice: ");
            shown.push_str(notice);
        }
        outputs.push(shown);
    })?;
    Ok(outputs.pop().unwrap_or_default())
}

fn answer(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => output,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn refusal(session: &mut Session<'_, MemoryStore>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error,
    }
}

fn rows(session: &mut Session<'_, MemoryStore>, sql: &str) -> Vec<String> {
    let rendered = answer(session, sql);
    let lines: Vec<&str> = rendered.lines().collect();
    match lines.len() {
        0..=2 => Vec::new(),
        count => lines[1..count - 1]
            .iter()
            .map(|line| (*line).to_owned())
            .collect(),
    }
}

fn prepared<'s>(store: &'s MemoryStore, statements: &[&str]) -> Session<'s, MemoryStore> {
    let mut session = Session::new(store);
    for statement in statements {
        answer(&mut session, statement);
    }
    session
}

#[test]
fn the_manuals_sales_tax_and_out_parameter_functions_compute_their_results() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            "CREATE FUNCTION sales_tax(subtotal real) RETURNS real AS $$
             BEGIN
                 RETURN subtotal * 0.06;
             END;
             $$ LANGUAGE plpgsql",
            "CREATE FUNCTION sum_n_product(x int, y int, OUT sum int, OUT prod int) AS $$
             BEGIN
                 sum := x + y;
                 prod := x * y;
             END;
             $$ LANGUAGE plpgsql",
        ],
    );
    assert_eq!(rows(&mut session, "SELECT sales_tax(100)"), ["6"]);
    assert_eq!(rows(&mut session, "SELECT * FROM sum_n_product(2, 4)"), ["6|8"]);
}

#[test]
fn the_manuals_merge_loop_retries_after_a_unique_violation() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            "CREATE TABLE db (a INT PRIMARY KEY, b TEXT)",
            "CREATE FUNCTION merge_db(key INT, data TEXT) RETURNS VOID AS
             $$
             BEGIN
                 LOOP
                     UPDATE db SET b = data WHERE a = key;
                     IF found THEN
                         RETURN;
                     END IF;
                     BEGIN
                         INSERT INTO db(a,b) VALUES (key, data);
                         RETURN;
                     EXCEPTION WHEN unique_violation THEN
                         NULL;
                     END;
                 END LOOP;
             END;
             $$
             LANGUAGE plpgsql",
        ],
    );
    answer(&mut session, "SELECT merge_db(1, 'david')");
    answer(&mut session, "SELECT merge_db(1, 'dennis')");
    answer(&mut session, "SELECT merge_db(2, 'ann')");
    assert_eq!(
        rows(&mut session, "SELECT a, b FROM db ORDER BY a"),
        ["1|dennis", "2|ann"]
    );
}

#[test]
fn the_manuals_set_returning_functions_return_next_and_return_query() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            "CREATE TABLE foo (fooid INT, foosubid INT, fooname TEXT)",
            "INSERT INTO foo VALUES (1, 2, 'three')",
            "INSERT INTO foo VALUES (4, 5, 'six')",
            "CREATE OR REPLACE FUNCTION get_all_foo() RETURNS SETOF foo AS
             $BODY$
             DECLARE
                 r foo%rowtype;
             BEGIN
                 FOR r IN
                     SELECT * FROM foo WHERE fooid > 0
                 LOOP
                     RETURN NEXT r;
                 END LOOP;
                 RETURN;
             END;
             $BODY$
             LANGUAGE plpgsql",
            "CREATE TABLE flight (flightid integer, flightdate date)",
            "INSERT INTO flight VALUES (10, '2024-05-01'), (11, '2024-05-01'), (12, '2024-05-03')",
            "CREATE FUNCTION get_available_flightid(date) RETURNS SETOF integer AS
             $BODY$
             BEGIN
                 RETURN QUERY SELECT flightid
                                FROM flight
                               WHERE flightdate >= $1
                                 AND flightdate < ($1 + 1);
                 IF NOT FOUND THEN
                     RAISE EXCEPTION 'No flight at %.', $1;
                 END IF;
                 RETURN;
             END;
             $BODY$
             LANGUAGE plpgsql",
        ],
    );
    assert_eq!(
        rows(&mut session, "SELECT * FROM get_all_foo()"),
        ["1|2|three", "4|5|six"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM get_available_flightid('2024-05-01') AS f(id) ORDER BY id"
        ),
        ["10", "11"]
    );
    let error = refusal(
        &mut session,
        "SELECT * FROM get_available_flightid('2024-05-02')",
    );
    assert_eq!(error.state(), SqlState::RAISE_EXCEPTION);
    assert_eq!(error.message(), "No flight at 2024-05-02.");
}

#[test]
fn the_manuals_emp_stamp_trigger_checks_and_stamps_rows() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &[
            "CREATE TABLE emp (empname text, salary integer, last_date timestamp, last_user text)",
            "CREATE FUNCTION emp_stamp() RETURNS trigger AS $emp_stamp$
                 BEGIN
                     IF NEW.empname IS NULL THEN
                         RAISE EXCEPTION 'empname cannot be null';
                     END IF;
                     IF NEW.salary IS NULL THEN
                         RAISE EXCEPTION '% cannot have null salary', NEW.empname;
                     END IF;
                     IF NEW.salary < 0 THEN
                         RAISE EXCEPTION '% cannot have a negative salary', NEW.empname;
                     END IF;
                     NEW.last_date := current_timestamp;
                     NEW.last_user := current_user;
                     RETURN NEW;
                 END;
             $emp_stamp$ LANGUAGE plpgsql",
            "CREATE TRIGGER emp_stamp BEFORE INSERT OR UPDATE ON emp
                 FOR EACH ROW EXECUTE FUNCTION emp_stamp()",
        ],
    );
    answer(&mut session, "INSERT INTO emp (empname, salary) VALUES ('ann', 100)");
    assert_eq!(
        rows(
            &mut session,
            "SELECT empname, salary, last_date IS NOT NULL, last_user IS NOT NULL FROM emp"
        ),
        ["ann|100|t|t"]
    );
    let error = refusal(&mut session, "INSERT INTO emp VALUES ('bob', -1)");
    assert_eq!(error.state(), SqlState::RAISE_EXCEPTION);
    assert_eq!(error.message(), "bob cannot have a negative salary");
    let error = refusal(&mut session, "UPDATE emp SET salary = NULL");
    assert_eq!(error.message(), "ann cannot have null salary");
    assert_eq!(rows(&mut session, "SELECT count(*) FROM emp"), ["1"]);
}

#[test]
fn the_manuals_procedure_call_assigns_its_inout_argument_back() {
    let store = MemoryStore::default();
    let mut session = prepared(
        &store,
        &["CREATE PROCEDURE triple(INOUT x int)
           LANGUAGE plpgsql
           AS $$
           BEGIN
               x := x * 3;
           END;
           $$"],
    );
    let output = answer(
        &mut session,
        "DO $$
         DECLARE myvar int := 5;
         BEGIN
           CALL triple(myvar);
           RAISE NOTICE 'myvar = %', myvar;
         END;
         $$",
    );
    assert!(output.contains("myvar = 15"), "{output}");
    assert_eq!(rows(&mut session, "CALL triple(4)"), ["12"]);
}
