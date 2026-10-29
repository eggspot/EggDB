use crate::arrays::{ArrayElement, ArrayValue, format_text, parse_text};
use crate::memory_store::MemoryStore;
use crate::tests::{fuzz, render};
use crate::types::Value;
use crate::{Session, SqlError};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

fn state(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error.state().code().to_owned(),
    }
}

const ELEMENTS: [ArrayElement; 10] = [
    ArrayElement::SmallInt,
    ArrayElement::Integer,
    ArrayElement::BigInt,
    ArrayElement::Real,
    ArrayElement::Double,
    ArrayElement::Numeric,
    ArrayElement::Boolean,
    ArrayElement::Text,
    ArrayElement::Varchar,
    ArrayElement::Bpchar,
];

#[test]
fn literals_read_back_as_they_print_for_every_element_type() {
    let cases = [
        (ArrayElement::SmallInt, "{1,-2,NULL}"),
        (ArrayElement::Integer, "{2147483647,-2147483648}"),
        (ArrayElement::BigInt, "{9223372036854775807}"),
        (ArrayElement::Real, "{1.5,NaN,-Infinity,1e+06}"),
        (ArrayElement::Double, "{0.30000000000000004,1e-05}"),
        (ArrayElement::Numeric, "{1.50,-0.001,NaN}"),
        (ArrayElement::Boolean, "{t,f,NULL}"),
        (
            ArrayElement::Text,
            "{a,\"b c\",\"\",\"NULL\",\"\\\"q\\\"\",\"back\\\\slash\",NULL}",
        ),
        (ArrayElement::Varchar, "{x,\"{y}\",\"a,b\"}"),
        (ArrayElement::Bpchar, "{\"a  \",b}"),
        (ArrayElement::Integer, "{}"),
    ];
    for (element, text) in cases {
        let array = parse_text(text, element).unwrap_or_else(|error| panic!("{text}: {error}"));
        assert_eq!(format_text(&array), text, "{element:?}");
    }
}

#[test]
fn malformed_literals_are_22p02_and_unsupported_forms_0a000() {
    let refused = |text: &str| {
        parse_text(text, ArrayElement::Integer)
            .expect_err(text)
            .state()
            .code()
            .to_owned()
    };
    for text in [
        "{1,2", "1,2", "{1,2}x", "{,}", "{1,}", "{\"1\"2}", "{1 2", "", "{\"1}", "{1,\\",
    ] {
        assert_eq!(refused(text), "22P02", "{text}");
    }
    for text in ["{{1,2},{3,4}}", "[1:2]={1,2}", "{1,{2}}"] {
        assert!(
            ["0A000", "22P02"].contains(&refused(text).as_str()),
            "{text}"
        );
    }
    assert_eq!(refused("{{1}}"), "0A000");
    assert_eq!(refused("[0:1]={1,2}"), "0A000");
    assert_eq!(refused("{a}"), "22P02");
    assert_eq!(refused("{99999999999}"), "22003");
}

#[test]
fn an_array_larger_than_the_value_limit_is_refused_while_it_is_read() {
    let text = format!("{{{}}}", vec!["123456"; 10_000].join(","));
    let _limit = crate::memory::enter_value_limit(4096);
    let error = parse_text(&text, ArrayElement::Integer).expect_err("limit");
    assert_eq!(error.state().code(), "54000");
}

struct Seeded(u64);

impl Seeded {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

#[test]
fn seeded_hostile_literals_never_panic_and_what_reads_prints_back_the_same() {
    const PIECES: [&str; 22] = [
        "{", "}", "\"", "\\", ",", " ", "\t", "a", "1", "-", ".", "e", "NULL", "null", "[", "]",
        ":", "=", "é", "\u{0}", "{}", "\"\"",
    ];
    let mut random = Seeded(0xA77A_75EE_D000_0037);
    for _ in 0..30_000 {
        let mut text = String::new();
        for _ in 0..random.below(14) {
            text.push_str(PIECES[random.below(PIECES.len() as u64) as usize]);
        }
        let element = ELEMENTS[random.below(ELEMENTS.len() as u64) as usize];
        if let Ok(array) = parse_text(&text, element) {
            let printed = format_text(&array);
            let again = parse_text(&printed, element)
                .unwrap_or_else(|error| panic!("{text} printed {printed}: {error}"));
            assert_eq!(again, array, "{text}");
        }
    }
}

#[test]
fn arrays_compare_element_by_element_with_nulls_last() {
    let array = |items: Vec<Value>| ArrayValue::new(ArrayElement::Integer, items);
    let ordering = crate::arrays::compare(
        &array(vec![Value::Integer(1), Value::Null]),
        &array(vec![Value::Integer(1), Value::Integer(9)]),
    );
    assert_eq!(ordering, std::cmp::Ordering::Greater);
    let ordering = crate::arrays::compare(
        &array(vec![Value::Integer(1)]),
        &array(vec![Value::Integer(1), Value::Integer(0)]),
    );
    assert_eq!(ordering, std::cmp::Ordering::Less);
}

#[test]
fn arrays_are_values_of_expressions_and_intermediate_results_but_not_of_tables() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        state(&mut session, "CREATE TABLE stored (a integer[])"),
        "0A000"
    );
    assert_eq!(
        state(&mut session, "CREATE TABLE stored (a text ARRAY)"),
        "0A000"
    );
    run(
        &mut session,
        "CREATE TABLE plain (id integer PRIMARY KEY, n integer, t text)",
    )
    .unwrap();
    assert_eq!(
        state(&mut session, "ALTER TABLE plain ADD COLUMN tags text[]"),
        "0A000"
    );
    assert_eq!(
        state(&mut session, "INSERT INTO plain VALUES (1, ARRAY[1], NULL)"),
        "42804"
    );
    run(
        &mut session,
        "INSERT INTO plain VALUES (1, 1, ARRAY['a','b c']::text)",
    )
    .unwrap();
    assert_eq!(
        run(&mut session, "SELECT t FROM plain").unwrap(),
        "t\n{a,\"b c\"}\nSELECT 1"
    );
    assert_eq!(
        state(
            &mut session,
            "CREATE VIEW arrays_view AS SELECT ARRAY[1, 2] AS a"
        ),
        "0A000"
    );
    assert_eq!(
        run(
            &mut session,
            "WITH w AS (SELECT ARRAY[n, n + 1] AS a FROM plain) SELECT a, a[2] FROM w"
        )
        .unwrap(),
        "a|a\n{1,2}|2\nSELECT 1"
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT a FROM (SELECT ARRAY['x', NULL] AS a UNION ALL SELECT ARRAY['y']) u ORDER BY a"
        )
        .unwrap(),
        "a\n{x,NULL}\n{y}\nSELECT 2"
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT DISTINCT a FROM (VALUES (ARRAY[1]), (ARRAY[1]), (ARRAY[2])) v(a) ORDER BY a"
        )
        .unwrap(),
        "a\n{1}\n{2}\nSELECT 2"
    );
    assert_eq!(state(&mut session, "SELECT '{{1}}'::int[]"), "0A000");
    assert_eq!(state(&mut session, "SELECT ARRAY[ARRAY[1]]"), "0A000");
    assert_eq!(state(&mut session, "SELECT (ARRAY[1, 2])[1:2]"), "0A000");
    assert_eq!(state(&mut session, "SELECT ARRAY[now()]"), "0A000");
    assert_eq!(state(&mut session, "SELECT '{1}'::date[]"), "0A000");
}

#[test]
fn hostile_statements_with_arrays_regular_expressions_and_values_never_panic() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        "CREATE TABLE f (id integer PRIMARY KEY, s text)",
    )
    .unwrap();
    run(&mut session, "INSERT INTO f VALUES (1, 'a'), (2, NULL)").unwrap();
    let fragments = [
        "SELECT",
        "ARRAY",
        "[",
        "]",
        "(",
        ")",
        "1",
        "'{1,2}'",
        "'{\"a\"}'",
        "::",
        "int[]",
        "text[]",
        "~",
        "~*",
        "!~",
        "'a.*'",
        "'('",
        "COLLATE",
        "\"C\"",
        "OPERATOR(pg_catalog.~)",
        "VALUES",
        ",",
        "FROM",
        "generate_series(1, 3)",
        "unnest(ARRAY[1])",
        "WITH ORDINALITY",
        "s",
        "f",
        "=",
        "ANY",
        "ALL",
        "||",
        "string_agg(s, ',')",
        "trim(",
        "quote_ident(s)",
        "WHERE",
        "AS",
        "x(a)",
        "NULL",
        "array_to_string(",
        "cardinality(",
        "UNION ALL",
    ];
    fuzz(&mut session, &fragments);
}
