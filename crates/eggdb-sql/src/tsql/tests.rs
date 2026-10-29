use super::errors::{formatted_message, line_of, raised_message, thrown};
use super::{CaughtError, Program, TryBlock, uses_batch_logic};
use crate::dialect::Dialect;
use crate::lexer::tokenize_in;
use crate::types::Value;

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

fn formatted(template: &str, arguments: &[Value]) -> String {
    formatted_message(template, arguments).unwrap_or_else(|error| panic!("{template}: {error}"))
}

#[test]
fn raiserror_substitutes_arguments_as_the_transact_sql_reference_documents() {
    assert_eq!(
        formatted(
            "The level for job_id:%d should be between %d and %d.",
            &[Value::Integer(3), Value::Integer(1), Value::Integer(10)]
        ),
        "The level for job_id:3 should be between 1 and 10."
    );
    assert_eq!(
        formatted("<<%*.*s>>", &[Value::Integer(7), Value::Integer(3), text("abcde")]),
        "<<    abc>>"
    );
    assert_eq!(formatted("<<%7.3s>>", &[text("abcde")]), "<<    abc>>");
    assert_eq!(formatted("<<%-7s>>", &[text("ab")]), "<<ab     >>");
    assert_eq!(formatted("%5d|%-5d|%05d", &[
        Value::Integer(42),
        Value::Integer(42),
        Value::Integer(42)
    ]), "   42|42   |00042");
    assert_eq!(formatted("%+d % d %i", &[
        Value::Integer(5),
        Value::Integer(5),
        Value::SmallInt(-5)
    ]), "+5  5 -5");
    assert_eq!(formatted("%x %X %#x %o %#o", &[
        Value::Integer(255),
        Value::Integer(255),
        Value::Integer(255),
        Value::Integer(8),
        Value::Integer(8)
    ]), "ff FF 0xff 10 010");
    assert_eq!(formatted("%u", &[Value::Integer(-1)]), "4294967295");
    assert_eq!(formatted("100%% done", &[]), "100% done");
    assert_eq!(formatted("%s and %d", &[]), "(null) and (null)");
    assert_eq!(formatted("%s", &[Value::Null]), "(null)");
    assert_eq!(formatted("%.3d", &[Value::Integer(7)]), "007");
    assert_eq!(formatted("ends with %", &[]), "ends with %");
    assert_eq!(formatted("%q stays", &[]), "%q stays");
    let mismatch = formatted_message("%d", &[text("x")]).expect_err("mismatch");
    assert_eq!(mismatch.vendor_code(), Some(2786));
    assert!(formatted_message("%s", &[Value::Integer(1)]).is_err());
}

#[test]
fn raiserror_checks_its_number_severity_and_state() {
    let message = raised_message(&text("x %s"), &Value::Integer(10), &Value::Integer(4), &[text(
        "y",
    )])
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        (message.number, message.severity, message.state, message.text.as_str()),
        (50_000, 10, 4, "x y")
    );
    let negative = raised_message(&text("m"), &Value::Integer(-3), &Value::Integer(-1), &[])
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!((negative.severity, negative.state), (0, 1));
    let cases = [
        (Value::Integer(50_000), Value::Integer(16), Value::Integer(1), 2732),
        (Value::Integer(12_999), Value::Integer(16), Value::Integer(1), 2732),
        (Value::Integer(60_000), Value::Integer(16), Value::Integer(1), 18054),
        (text("m"), Value::Integer(19), Value::Integer(1), 2754),
    ];
    for (message, severity, state, code) in cases {
        let error = raised_message(&message, &severity, &state, &[]).expect_err("refused");
        assert_eq!(error.vendor_code(), Some(code), "{message:?}");
    }
    assert!(raised_message(&text("m"), &Value::Integer(16), &Value::Integer(256), &[]).is_err());
    let long = "x".repeat(3000);
    let shortened = raised_message(&text(&long), &Value::Integer(1), &Value::Integer(1), &[])
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(shortened.text.chars().count(), 2047);
    assert!(shortened.text.ends_with("..."));
}

#[test]
fn throw_takes_numbers_from_50000_and_states_up_to_255() {
    let error = thrown(&Value::Integer(51_000), &text("custom"), &Value::Integer(7))
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(error.sqlserver_number(), Some(51_000));
    assert_eq!(error.sqlserver_state(), Some(7));
    assert_eq!(error.severity(), Some(16));
    assert_eq!(error.ends_sqlserver_batch(), Some(true));
    assert_eq!(error.to_string(), "Msg 51000, Level 16, State 7: custom");
    let wide = thrown(&Value::Integer(2_000_000_000), &text("wide"), &Value::Integer(0))
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(wide.sqlserver_number(), Some(2_000_000_000));
    assert_eq!(wide.vendor_code(), None);
    let low = thrown(&Value::Integer(49_999), &text("x"), &Value::Integer(1)).expect_err("low");
    assert_eq!(low.vendor_code(), Some(35100));
    assert!(thrown(&Value::Integer(50_000), &text("x"), &Value::Integer(300)).is_err());
    let caught = CaughtError::of(&error, 3);
    assert_eq!(caught.line, 3);
    assert_eq!(caught.rethrown().sqlserver_number(), Some(51_000));
}

#[test]
fn lines_count_from_one() {
    assert_eq!(line_of("a\nb\nc", 0), 1);
    assert_eq!(line_of("a\nb\nc", 2), 2);
    assert_eq!(line_of("a\nb\nc", 99), 3);
}

#[test]
fn batch_logic_is_found_by_its_words_and_variables() {
    let uses = |text: &str| {
        let tokens = tokenize_in(text, Dialect::SqlServer).unwrap_or_else(|error| panic!("{error}"));
        uses_batch_logic(&tokens)
    };
    for text in [
        "DECLARE @a int",
        "IF 1 = 1 SELECT 1",
        "WHILE 1 = 0 SELECT 1",
        "BEGIN SELECT 1 END",
        "BEGIN TRY SELECT 1 END TRY BEGIN CATCH END CATCH",
        "PRINT 'x'",
        "SELECT @x",
        "EXEC ('SELECT 1')",
        "EXEC sp_executesql N'SELECT 1'",
        "THROW 50000, 'x', 1",
    ] {
        assert!(uses(text), "{text}");
    }
    for text in [
        "SELECT 1",
        "BEGIN TRAN",
        "BEGIN TRANSACTION; COMMIT",
        "EXEC sp_tables",
        "SELECT @@TRANCOUNT",
    ] {
        assert!(!uses(text), "{text}");
    }
}

#[test]
fn the_innermost_try_and_catch_around_an_instruction_are_found() {
    let program = Program {
        instructions: Vec::new(),
        tries: vec![
            TryBlock {
                body: 0..10,
                handler: 11..20,
            },
            TryBlock {
                body: 2..5,
                handler: 6..8,
            },
        ],
        declarations: Vec::new(),
    };
    assert_eq!(program.try_around(3), Some(1));
    assert_eq!(program.try_around(7), Some(0));
    assert_eq!(program.try_around(10), None);
    assert_eq!(program.handler_around(7), Some(1));
    assert_eq!(program.handler_around(12), Some(0));
    assert_eq!(program.handler_around(1), None);
}
