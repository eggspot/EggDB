use super::{RoutineLanguage, parse_definition};
use crate::dialect::Dialect;
use crate::memory_store::MemoryStore;
use crate::session::Session;
use crate::tests::render;

fn session(store: &MemoryStore) -> Session<'_, MemoryStore> {
    Session::with_dialect(store, Dialect::MySql)
}

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    let mut out = Vec::new();
    session
        .execute(sql, &mut |result| out.push(render(&result)))
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    out.join("\n")
}

fn fails(session: &mut Session<'_, MemoryStore>, sql: &str) -> crate::SqlError {
    session.execute(sql, &mut |_| {}).expect_err(sql)
}

#[test]
fn a_function_with_locals_loops_and_conditions_returns_its_value() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    run(
        &mut session,
        "CREATE FUNCTION factorial(n INT) RETURNS BIGINT DETERMINISTIC
         BEGIN
           DECLARE result BIGINT DEFAULT 1;
           DECLARE i INT DEFAULT 2;
           IF n < 0 THEN
             SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'negative';
           END IF;
           counting: WHILE i <= n DO
             SET result = result * i;
             SET i = i + 1;
           END WHILE counting;
           RETURN result;
         END",
    );
    assert_eq!(run(&mut session, "SELECT factorial(5)"), run(&mut session, "SELECT CAST(120 AS SIGNED)"));
    let error = fails(&mut session, "SELECT factorial(-1)");
    assert_eq!(error.state().code(), "45000");
    assert!(error.message().contains("negative"), "{}", error.message());
    let definition = parse_definition(
        "CREATE FUNCTION twice(x INT) RETURNS INT RETURN x * 2",
        Dialect::MySql,
    )
    .unwrap();
    assert_eq!(definition.language, RoutineLanguage::MySql);
}

#[test]
fn a_procedure_writes_rows_reads_into_variables_and_fills_out_parameters() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    run(&mut session, "CREATE TABLE ledger (id INT PRIMARY KEY AUTO_INCREMENT, amount INT)");
    run(
        &mut session,
        "CREATE DEFINER=`root`@`localhost` PROCEDURE add_entries(IN how_many INT, IN each_amount INT, OUT total INT)
         MODIFIES SQL DATA
         BEGIN
           DECLARE done INT DEFAULT 0;
           REPEAT
             INSERT INTO ledger (amount) VALUES (each_amount);
             SET done = done + 1;
           UNTIL done >= how_many END REPEAT;
           SELECT SUM(l.amount) INTO total FROM ledger l;
         END",
    );
    run(&mut session, "CALL add_entries(3, 10, @sum)");
    assert_eq!(run(&mut session, "SELECT @sum"), run(&mut session, "SELECT 30"));
    assert_eq!(run(&mut session, "SELECT COUNT(*) FROM ledger"), run(&mut session, "SELECT 3"));
    run(&mut session, "DROP PROCEDURE add_entries");
    assert!(session.execute("CALL add_entries(1, 1, @x)", &mut |_| {}).is_err());
    run(&mut session, "DROP PROCEDURE IF EXISTS add_entries");
}

#[test]
fn leave_iterate_case_and_a_call_without_parentheses_work() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    run(&mut session, "CREATE TABLE seen (n INT)");
    run(
        &mut session,
        "CREATE PROCEDURE odd_numbers()
         BEGIN
           DECLARE i INT DEFAULT 0;
           numbers: LOOP
             SET i = i + 1;
             IF i > 7 THEN LEAVE numbers; END IF;
             CASE i % 2
               WHEN 0 THEN ITERATE numbers;
               ELSE INSERT INTO seen VALUES (i);
             END CASE;
           END LOOP numbers;
         END",
    );
    run(&mut session, "CALL odd_numbers");
    assert_eq!(run(&mut session, "SELECT SUM(n) FROM seen"), run(&mut session, "SELECT 16"));
}

#[test]
fn unsupported_and_malformed_bodies_are_errors() {
    let store = MemoryStore::default();
    let mut session = session(&store);
    for body in [
        "CREATE PROCEDURE p() BEGIN DECLARE CONTINUE HANDLER FOR NOT FOUND SET @x = 1; END",
        "CREATE PROCEDURE p() BEGIN DECLARE c CURSOR FOR SELECT 1; END",
        "CREATE PROCEDURE p() BEGIN LEAVE nowhere; END",
        "CREATE PROCEDURE p() BEGIN IF 1 THEN SELECT 1; END",
        "CREATE FUNCTION f() RETURNS INT BEGIN SET x = 1; DECLARE x INT; RETURN 1; END",
    ] {
        assert!(session.execute(body, &mut |_| {}).is_err(), "{body}");
    }
    let deep = format!("CREATE PROCEDURE p() {}SELECT 1;{}", "BEGIN ".repeat(200), " END;".repeat(200));
    assert!(session.execute(&deep, &mut |_| {}).is_err());
}
