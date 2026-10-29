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

fn parsed(sql: &str) -> crate::ast::Expr {
    let tokens = crate::lexer::tokenize_in(sql, crate::dialect::Dialect::PostgreSql)
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    let mut parser =
        crate::parser::Parser::new(sql, &tokens).in_dialect(crate::dialect::Dialect::PostgreSql);
    let expr = parser
        .expression()
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    assert!(parser.at_end(), "{sql}");
    expr
}

#[test]
fn any_over_an_array_constructor_under_casts_is_parsed_once_into_an_in_list() {
    for sql in [
        "(level)::text = ANY ((ARRAY['debug'::character varying, 'info'::character varying])::text[])",
        "level = ANY (ARRAY['a', 'b'])",
        "level <> ALL (((ARRAY['a', 'b']))::varchar[]::text[])",
        "day = ANY (ARRAY['2024-01-01', '2024-01-02']::date[])",
    ] {
        assert!(
            matches!(parsed(sql).kind, crate::ast::ExprKind::InList { .. }),
            "{sql}"
        );
    }
    for sql in [
        "n = ANY (ARRAY[1, 2] || ARRAY[3])",
        "n = ANY ('{1,2}'::int[])",
        "n + 1 < ANY (ARRAY[1, 2])",
    ] {
        assert!(
            matches!(&parsed(sql).kind, crate::ast::ExprKind::Function { name, .. } if name == "array_any"),
            "{sql}"
        );
    }
}

#[test]
fn nested_any_over_arrays_fails_fast() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let mut nested = "true".to_owned();
    for _ in 0..16 {
        nested = format!("1 = ANY(ARRAY[{nested},{nested}] || ARRAY[true])");
    }
    let started = std::time::Instant::now();
    assert_eq!(
        state(&mut session, &format!("SELECT {nested} FROM")),
        "42601"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn concatenation_takes_the_common_element_type_statically_and_at_run_time() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let cases = [
        ("ARRAY[1,2] || 3.5", "{1,2,3.5}"),
        ("ARRAY[1,2] || 3.5::float8", "{1,2,3.5}"),
        ("ARRAY[2147483647] || 2147483648", "{2147483647,2147483648}"),
        ("ARRAY[1] || ARRAY[1::bigint]", "{1,1}"),
        ("ARRAY[1.5,2] || ARRAY[1]", "{1.5,2,1}"),
        ("3.5 || ARRAY[1,2]", "{3.5,1,2}"),
        ("ARRAY[1::smallint] || 70000", "{1,70000}"),
        ("ARRAY[1::real] || 1.5::numeric", "{1,1.5}"),
        ("NULL::int[] || 1", "{1}"),
        ("1 || NULL::int[]", "{1}"),
        ("'a'::text || NULL::text[]", "{a}"),
        ("NULL::int[] || NULL::int", "{NULL}"),
        ("NULL::int[] || 1.5", "{1.5}"),
        ("NULL::int[] || '{1,2}'", "{1,2}"),
        ("ARRAY[1] || NULL::numeric", "{1,NULL}"),
        ("NULL || ARRAY[1]", "{1}"),
    ];
    for (sql, expected) in cases {
        assert_eq!(
            run(&mut session, &format!("SELECT {sql} AS a")).unwrap(),
            format!("a\n{expected}\nSELECT 1"),
            "{sql}"
        );
        let derived = format!("SELECT a FROM (SELECT {sql} AS a) s");
        assert_eq!(
            run(&mut session, &derived).unwrap(),
            format!("a\n{expected}\nSELECT 1"),
            "{derived}"
        );
    }
    let typed = [
        ("ARRAY[1,2] || 3.5", ArrayElement::Numeric),
        ("ARRAY[1::real] || 1.5::numeric", ArrayElement::Real),
        ("ARRAY[1] || ARRAY[1::bigint]", ArrayElement::BigInt),
        ("NULL::int[] || 1.5", ArrayElement::Numeric),
        ("ARRAY['a'::varchar] || 'b'::text", ArrayElement::Varchar),
        ("ARRAY[1] || '{2}'", ArrayElement::Integer),
    ];
    for (sql, element) in typed {
        assert_eq!(
            crate::typing::static_type(&parsed(sql), crate::eval::Scope::empty()),
            Some(crate::types::SqlType::Array(element)),
            "{sql}"
        );
    }
    assert_eq!(
        run(
            &mut session,
            "SELECT NULL::int[] || NULL::int[] IS NULL AS a"
        )
        .unwrap(),
        "a\nt\nSELECT 1"
    );
    for sql in [
        "SELECT ARRAY[1] || true",
        "SELECT ARRAY[1] || ARRAY[true]",
        "SELECT ARRAY['a'] || 1",
    ] {
        assert_eq!(state(&mut session, sql), "42883", "{sql}");
    }
}

#[test]
fn subscripts_outside_the_integer_range_are_22003() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "SELECT (ARRAY[1,2,3])[2147483648]",
        "SELECT (ARRAY[1,2,3])[-2147483649]",
        "SELECT (ARRAY[1,2,3])[2147483647.6]",
    ] {
        assert_eq!(state(&mut session, sql), "22003", "{sql}");
    }
    assert_eq!(
        run(
            &mut session,
            "SELECT (ARRAY[1,2,3])[2147483647] IS NULL AS a, (ARRAY[1,2,3])[1.6] AS b"
        )
        .unwrap(),
        "a|b\nt|2\nSELECT 1"
    );
}

#[test]
fn a_long_concatenation_chain_with_null_operands_is_typed_once_per_node() {
    for dialect in [crate::Dialect::PostgreSql, crate::Dialect::Sqlite] {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, dialect);
        let columns: Vec<String> = (1..=40).map(|index| format!("c{index}")).collect();
        let definitions: Vec<String> = columns.iter().map(|name| format!("{name} text")).collect();
        run(
            &mut session,
            &format!(
                "CREATE TABLE wide (id integer PRIMARY KEY, {})",
                definitions.join(", ")
            ),
        )
        .unwrap();
        for id in 1..=20 {
            let values: Vec<String> = (1..=40)
                .map(|column| {
                    if id % 2 == 0 && column == 20 {
                        "NULL".to_owned()
                    } else {
                        format!("'{column}'")
                    }
                })
                .collect();
            run(
                &mut session,
                &format!("INSERT INTO wide VALUES ({id}, {})", values.join(", ")),
            )
            .unwrap();
        }
        let chain = columns.join(" || ");
        let started = std::time::Instant::now();
        let joined = run(
            &mut session,
            &format!("SELECT {chain} AS joined FROM wide WHERE id = 1"),
        )
        .unwrap();
        let counted = run(
            &mut session,
            &format!("SELECT count(*) AS n FROM wide WHERE ({chain}) IS NULL"),
        )
        .unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "{dialect:?}: {:?}",
            started.elapsed()
        );
        let expected: String = (1..=40).map(|column| column.to_string()).collect();
        assert_eq!(
            joined,
            format!("joined\n{expected}\nSELECT 1"),
            "{dialect:?}"
        );
        assert_eq!(counted, "n\n10\nSELECT 1", "{dialect:?}");
    }
}

#[test]
fn concatenation_results_carry_their_array_type_to_the_enclosing_concatenation() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let numbers: Vec<String> = (1..=40).map(|number| number.to_string()).collect();
    let started = std::time::Instant::now();
    assert_eq!(
        run(
            &mut session,
            &format!("SELECT NULL::int[] || {} AS a", numbers.join(" || "))
        )
        .unwrap(),
        format!("a\n{{{}}}\nSELECT 1", numbers.join(","))
    );
    assert!(started.elapsed() < std::time::Duration::from_millis(500));
    let cases = [
        ("(NULL::int[] || NULL::int[]) || 1", "{1}"),
        ("1 || (NULL::int[] || NULL::int[])", "{1}"),
        ("(NULL::text || NULL) || ARRAY['a']", "{NULL,a}"),
    ];
    for (sql, expected) in cases {
        assert_eq!(
            run(&mut session, &format!("SELECT {sql} AS a")).unwrap(),
            format!("a\n{expected}\nSELECT 1"),
            "{sql}"
        );
    }
    assert_eq!(
        state(&mut session, "SELECT ARRAY[1] || (NULL::text || 'a')"),
        "42883"
    );
}

#[test]
fn array_casts_apply_the_element_types_length_and_scale() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let cases = [
        ("ARRAY['abc']::varchar(1)[]", "{a}"),
        ("ARRAY[1,2]::numeric(5,2)[]", "{1.00,2.00}"),
        ("'{abc,de}'::char(2)[]", "{ab,de}"),
        ("'{1.234,NULL}'::numeric(5,2)[]", "{1.23,NULL}"),
        ("(ARRAY['abc']::varchar(2)[])[1]", "ab"),
        ("NULL::varchar(1)[] IS NULL", "t"),
    ];
    for (sql, expected) in cases {
        assert_eq!(
            run(&mut session, &format!("SELECT {sql} AS a")).unwrap(),
            format!("a\n{expected}\nSELECT 1"),
            "{sql}"
        );
    }
    assert_eq!(
        run(&mut session, "SELECT ARRAY['abc']::varchar(1)[]").unwrap(),
        "array\n{a}\nSELECT 1"
    );
    assert_eq!(
        state(&mut session, "SELECT ARRAY[1234.5]::numeric(3,1)[]"),
        "22003"
    );
    assert_eq!(
        state(&mut session, "SELECT array_element_cast(ARRAY[1], 1)"),
        "42883"
    );
}

#[test]
fn arrays_of_different_element_types_unify_in_union_and_values() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        run(
            &mut session,
            "SELECT ARRAY[1] AS a UNION SELECT ARRAY[1.5] ORDER BY 1"
        )
        .unwrap(),
        "a\n{1}\n{1.5}\nSELECT 2"
    );
    assert_eq!(
        run(&mut session, "VALUES (ARRAY[1]), (ARRAY[1.5])").unwrap(),
        "column1\n{1}\n{1.5}\nSELECT 2"
    );
    assert_eq!(
        run(
            &mut session,
            "SELECT ARRAY['a'::varchar] AS a UNION ALL SELECT ARRAY['b'::text]"
        )
        .unwrap(),
        "a\n{a}\n{b}\nSELECT 2"
    );
    assert_eq!(
        crate::set_operations::common_type(
            crate::types::SqlType::Array(ArrayElement::Integer),
            crate::types::SqlType::Array(ArrayElement::Boolean)
        ),
        None
    );
}

#[test]
fn any_with_a_parameter_on_the_left_keeps_the_general_form() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let sql = "SELECT $1 = ANY(ARRAY[1,2]) AS a, $1 <> ALL(ARRAY[3]) AS b";
    let description = session.describe(sql, &[None]).unwrap();
    assert_eq!(description.columns.map(|columns| columns.len()), Some(2));
    let parameter = crate::Parameter {
        declared_type: None,
        value: Some("1".to_owned()),
    };
    let mut outputs = Vec::new();
    session
        .execute_prepared(sql, &[parameter], &mut |result| {
            outputs.push(render(&result));
            true
        })
        .unwrap();
    assert_eq!(outputs, vec!["a|b\nt|t\nSELECT 1".to_owned()]);
}
