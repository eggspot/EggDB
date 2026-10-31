use super::{
    TriggerRecords, anonymous_block_definition, compile, compile_package, compile_trigger,
    is_compilation_error,
};
use crate::error::SqlError;
use crate::memory_store::MemoryStore;
use crate::{Dialect, QueryResult, Session};

struct Scrambler(u64);

impl Scrambler {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound.max(1) as u64) as usize
    }

    fn joined(&mut self, pieces: &[&str], most: usize) -> String {
        let length = 1 + self.below(most);
        let mut text = String::new();
        for _ in 0..length {
            text.push_str(pieces[self.below(pieces.len())]);
            text.push(' ');
        }
        text
    }
}

const TOKENS: &[&str] = &[
    "DECLARE", "BEGIN", "END", "END;", "IF", "THEN", "ELSIF", "ELSE", "END IF", "LOOP",
    "END LOOP", "WHILE", "FOR", "IN", "REVERSE", "..", "CASE", "WHEN", "END CASE", "EXIT",
    "CONTINUE", "RETURN", "RAISE", "EXCEPTION", "OTHERS", "OR", "PRAGMA", "EXCEPTION_INIT",
    "AUTONOMOUS_TRANSACTION", "TYPE", "IS", "AS", "RECORD", "TABLE", "OF", "INDEX", "BY",
    "PLS_INTEGER", "VARCHAR2(10)", "NUMBER", "VARRAY(3)", "REF", "CURSOR", "SUBTYPE", "%TYPE",
    "%ROWTYPE", "%FOUND", "%NOTFOUND", "%ROWCOUNT", "%ISOPEN", "SQL", "PROCEDURE", "FUNCTION",
    "PACKAGE", "BODY", "OUT", "IN OUT", "NOCOPY", "DEFAULT", "CONSTANT", "NOT NULL", ":=", "=>",
    "OPEN", "FETCH", "CLOSE", "INTO", "BULK COLLECT", "LIMIT", "FORALL", "SAVE EXCEPTIONS",
    "EXECUTE IMMEDIATE", "USING", "GOTO", "<<", ">>", "lbl", "SELECT", "FROM dual", "COMMIT",
    "ROLLBACK", "NULL", "RAISE_APPLICATION_ERROR", "DBMS_OUTPUT.PUT_LINE", ":new", ":old",
    "x", "y", "t", "c", ";", ",", "(", ")", ".", "'s'", "-20001",
];

fn assert_compilation_error(text: &str, outcome: Result<(), SqlError>) {
    if let Err(error) = outcome {
        assert!(is_compilation_error(&error), "{text}: {error}");
        assert!(error.message().starts_with("line "), "{text}: {error}");
    }
}

#[test]
fn scrambled_units_compile_to_oracle_compilation_errors_and_never_panic() {
    let mut random = Scrambler(0x5EED_0316_ABCD_0001);
    let records = TriggerRecords::default();
    for _ in 0..20_000 {
        let tokens = random.joined(TOKENS, 24);
        let block = format!("DECLARE {tokens} BEGIN NULL; END;");
        let package = format!("PACKAGE BODY k IS {tokens} END k;");
        let subprogram = format!("PROCEDURE p IS {tokens}");
        for text in [&tokens, &block, &subprogram] {
            assert_compilation_error(
                text,
                compile(&anonymous_block_definition(text)).map(drop),
            );
        }
        assert_compilation_error(&package, compile_package(&package, None).map(drop));
        assert_compilation_error(&tokens, compile_trigger(&tokens, &records).map(drop));
    }
}

#[test]
fn oversized_units_are_refused_with_errors() {
    for text in [
        "BEGIN ".repeat(50_000),
        "(".repeat(50_000),
        format!(
            "BEGIN {}NULL;{} END;",
            "IF 1 = 1 THEN ".repeat(5_000),
            " END IF;".repeat(5_000)
        ),
        format!(
            "BEGIN {}NULL;{} END;",
            "LOOP ".repeat(5_000),
            " END LOOP;".repeat(5_000)
        ),
    ] {
        assert!(compile(&anonymous_block_definition(&text)).is_err(), "{}", &text[..40]);
    }
}

const DECLARATIONS: &str = "v NUMBER := 1;
  s VARCHAR2(100) := 'a';
  e EXCEPTION;
  PRAGMA EXCEPTION_INIT(e, -20001);
  TYPE nt IS TABLE OF NUMBER;
  c nt := nt();
  TYPE assoc IS TABLE OF NUMBER INDEX BY VARCHAR2(10);
  a assoc;
  TYPE va IS VARRAY(2) OF NUMBER;
  w va := va(1);
  CURSOR cur IS SELECT n FROM t ORDER BY n;
  PROCEDURE p (x IN OUT NUMBER) IS BEGIN x := x * 2; END p;
  FUNCTION f (x NUMBER) RETURN NUMBER IS BEGIN RETURN x / v; END f;";

const STATEMENTS: &[&str] = &[
    "v := v + 1;",
    "v := 1 / 0;",
    "v := 0;",
    "v := NULL;",
    "s := s || 'x';",
    "s := RPAD('x', 500, 'y');",
    "s := NULL;",
    "SELECT n INTO v FROM t WHERE ROWNUM = 1;",
    "SELECT n INTO v FROM t;",
    "SELECT n INTO v FROM t WHERE n = -1;",
    "INSERT INTO t VALUES (v);",
    "DELETE FROM t WHERE n > 5;",
    "UPDATE t SET n = n + 1 WHERE n < 0;",
    "RAISE e;",
    "RAISE;",
    "RAISE NO_DATA_FOUND;",
    "RAISE_APPLICATION_ERROR(-20001, s);",
    "RAISE_APPLICATION_ERROR(v, s);",
    "c.EXTEND;",
    "c.EXTEND(2);",
    "c(v) := v;",
    "c.DELETE;",
    "c.TRIM;",
    "c := NULL;",
    "a(s) := v;",
    "a.DELETE(s);",
    "v := c.COUNT;",
    "v := c(1);",
    "v := a(s);",
    "w.EXTEND;",
    "w(2) := 5;",
    "OPEN cur;",
    "FETCH cur INTO v;",
    "CLOSE cur;",
    "IF cur%ISOPEN THEN CLOSE cur; END IF;",
    "IF v > 2 THEN v := 0; ELSE s := NULL; END IF;",
    "DBMS_OUTPUT.PUT_LINE(s);",
    "p(v);",
    "v := f(v);",
    "EXECUTE IMMEDIATE 'SELECT COUNT(*) FROM t' INTO v;",
    "EXECUTE IMMEDIATE s;",
    "COMMIT;",
    "ROLLBACK;",
    "NULL;",
    "BEGIN RAISE e; EXCEPTION WHEN OTHERS THEN s := SQLERRM; END;",
    "v := SQLCODE + SQL%ROWCOUNT;",
    "v := ;",
    "END;",
];

const HANDLERS: &[&str] = &[
    "",
    "EXCEPTION WHEN e THEN NULL; WHEN OTHERS THEN s := SQLERRM;",
    "EXCEPTION WHEN NO_DATA_FOUND OR VALUE_ERROR THEN RAISE;",
];

fn count_of(session: &mut Session<'_, MemoryStore>, text: &str) -> String {
    let mut found = None;
    session
        .execute(text, &mut |result: QueryResult| {
            if let Some(row) = result.rows.first() {
                found = row.first().and_then(|value| value.to_text());
            }
        })
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    found.unwrap_or_default()
}

#[test]
fn scrambled_blocks_run_to_an_end_or_an_error_and_leave_the_session_usable() {
    let store = MemoryStore::new();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    session
        .execute("CREATE TABLE t (n NUMBER)", &mut |_| {})
        .unwrap_or_else(|error| panic!("{error}"));
    session.enable_server_output(Some(2_000));
    let declared_only = format!("DECLARE\n  {DECLARATIONS}\nBEGIN\n  NULL;\nEND;");
    session
        .execute(&declared_only, &mut |_| {})
        .unwrap_or_else(|error| panic!("{error}"));
    let mut random = Scrambler(0x5EED_0316_0000_0002);
    for _ in 0..2_000 {
        let statements = random.joined(STATEMENTS, 8);
        let handler = HANDLERS[random.below(HANDLERS.len())];
        let block = format!("DECLARE\n  {DECLARATIONS}\nBEGIN\n  {statements}\n{handler}\nEND;");
        let _ = session.execute(&block, &mut |_| {});
        let _ = session.take_server_output();
    }
    session
        .execute("BEGIN DELETE FROM t; INSERT INTO t VALUES (42); COMMIT; END;", &mut |_| {})
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(count_of(&mut session, "SELECT COUNT(*) FROM t WHERE n = 42"), "1");
}

#[test]
fn runaway_recursion_and_oversized_output_end_with_errors() {
    let store = MemoryStore::new();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    for text in [
        "CREATE PROCEDURE again IS BEGIN again; END;",
        "CREATE FUNCTION deeper (n NUMBER) RETURN NUMBER IS BEGIN RETURN deeper(n + 1); END;",
        "CREATE PACKAGE broken AS v NUMBER; END broken;",
        "CREATE PACKAGE BODY broken AS BEGIN v := 1 / 0; END broken;",
    ] {
        session
            .execute(text, &mut |_| {})
            .unwrap_or_else(|error| panic!("{text}: {error}"));
    }
    session.enable_server_output(Some(2_000));
    for text in [
        "BEGIN again; END;",
        "SELECT deeper(1) FROM dual",
        "DECLARE x NUMBER; BEGIN x := deeper(1); END;",
        "BEGIN broken.v := 2; END;",
        "BEGIN broken.v := 2; END;",
        "BEGIN FOR i IN 1..100 LOOP DBMS_OUTPUT.PUT_LINE(RPAD('x', 100, 'x')); END LOOP; END;",
        "BEGIN DBMS_OUTPUT.PUT_LINE(RPAD('x', 40000, 'x')); END;",
        "DECLARE TYPE nt IS TABLE OF NUMBER; c nt := nt(); BEGIN c.EXTEND(5000000); END;",
        "BEGIN RAISE_APPLICATION_ERROR(-19999, 'below the range'); END;",
    ] {
        assert!(session.execute(text, &mut |_| {}).is_err(), "{text}");
        let _ = session.take_server_output();
    }
    assert_eq!(count_of(&mut session, "SELECT 1 FROM dual"), "1");
}
