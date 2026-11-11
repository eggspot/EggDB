use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

fn rows(session: &mut Session<'_>, sql: &str) -> Vec<String> {
    let output = run(session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    let lines: Vec<&str> = output.lines().collect();
    match lines.as_slice() {
        [_, body @ .., _] => body.iter().map(|line| (*line).to_owned()).collect(),
        _ => panic!("{sql}: {output}"),
    }
}

fn one(session: &mut Session<'_>, sql: &str) -> String {
    let found = rows(session, sql);
    match found.as_slice() {
        [only] => only.clone(),
        _ => panic!("{sql}: {found:?}"),
    }
}

fn state(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error.state().code().to_owned(),
    }
}

#[test]
fn datetime_methods_read_iso_strings_and_templates() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let cases = [
        (r#""2017-03-10""#, "$.datetime()", r#""2017-03-10""#),
        (r#""2017-03-10""#, "$.datetime().type()", r#""date""#),
        (
            r#""12:34:56""#,
            "$.datetime().type()",
            r#""time without time zone""#,
        ),
        (r#""12:34:56+05:30""#, "$.time_tz()", r#""12:34:56+05:30""#),
        (
            r#""2017-03-10 12:34:56+3""#,
            "$.datetime()",
            r#""2017-03-10T12:34:56+03:00""#,
        ),
        (
            r#""2017-03-10 12:34:56+3""#,
            "$.datetime().type()",
            r#""timestamp with time zone""#,
        ),
        (
            r#""10-03-2017""#,
            r#"$.datetime("DD-MM-YYYY")"#,
            r#""2017-03-10""#,
        ),
        (
            r#""2017-03-10 12:34:56.789""#,
            "$.timestamp(1)",
            r#""2017-03-10T12:34:56.8""#,
        ),
        (r#""2017-03-10 12:34:56""#, "$.date()", r#""2017-03-10""#),
        (
            r#""2017-03-10""#,
            "$.datetime().string()",
            r#""2017-03-10""#,
        ),
    ];
    for (document, path, expected) in cases {
        assert_eq!(
            one(
                &mut session,
                &format!("SELECT jsonb_path_query('{document}', '{path}')")
            ),
            expected,
            "{path} on {document}"
        );
    }
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT jsonb_path_query('["2017-03-10", "2017-03-12"]', '$[*] ? (@.datetime() > "2017-03-11".datetime())')"#
        ),
        [r#""2017-03-12""#]
    );
}

#[test]
fn datetime_failures_are_soft_and_zone_comparisons_need_the_tz_functions() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        state(
            &mut session,
            r#"SELECT jsonb_path_query('"x"', '$.datetime()')"#
        ),
        "22031"
    );
    assert_eq!(
        state(&mut session, "SELECT jsonb_path_query('1', '$.datetime()')"),
        "22031"
    );
    assert_eq!(
        one(
            &mut session,
            r#"SELECT jsonb_path_query_first('"x"', '$.datetime()', '{}', true)"#
        ),
        "NULL"
    );
    assert_eq!(
        one(
            &mut session,
            r#"SELECT jsonb_path_exists('"x"', 'lax $ ? (@.datetime() > "2017-03-11".datetime())')"#
        ),
        "f"
    );
    let comparison = r#"'"2017-03-10"', '$.datetime() < "2017-03-11 00:00:00+00".datetime()'"#;
    assert_eq!(
        state(
            &mut session,
            &format!("SELECT jsonb_path_match({comparison})")
        ),
        "22031"
    );
    assert_eq!(
        one(
            &mut session,
            &format!("SELECT jsonb_path_match_tz({comparison})")
        ),
        "t"
    );
    assert_eq!(
        state(
            &mut session,
            r#"SELECT jsonb_path_query('"x"', '$.datetime(1)')"#
        ),
        "42601"
    );
}

#[test]
fn json_exists_answers_each_on_error_behavior() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let document = r#"'{"a": [1, 2, 3]}'::jsonb"#;
    let cases = [
        ("'lax $.a[5]' ERROR ON ERROR", "f"),
        ("'strict $.a[5]'", "f"),
        ("'strict $.a[5]' TRUE ON ERROR", "t"),
        ("'strict $.a[5]' UNKNOWN ON ERROR", "NULL"),
        ("'$.a[*] ? (@ > $x)' PASSING 2 AS x", "t"),
        ("'$.a[*] ? (@ > $x)' PASSING 3 AS x", "f"),
    ];
    for (rest, expected) in cases {
        assert_eq!(
            one(
                &mut session,
                &format!("SELECT JSON_EXISTS({document}, {rest})")
            ),
            expected,
            "{rest}"
        );
    }
    assert_eq!(
        state(
            &mut session,
            &format!("SELECT JSON_EXISTS({document}, 'strict $.a[5]' ERROR ON ERROR)")
        ),
        "22033"
    );
    assert_eq!(
        state(
            &mut session,
            &format!("SELECT JSON_EXISTS({document}, '$' NULL ON ERROR)")
        ),
        "42601"
    );
    assert_eq!(
        one(&mut session, "SELECT JSON_EXISTS(NULL::jsonb, '$')"),
        "NULL"
    );
    run(&mut session, "CREATE TABLE v (j jsonb)").expect("create");
    run(
        &mut session,
        r#"INSERT INTO v VALUES ('{"a": 1}'), ('{"b": 1}')"#,
    )
    .expect("insert");
    assert_eq!(
        one(
            &mut session,
            "SELECT count(*) FROM v WHERE JSON_EXISTS(j, '$.a')"
        ),
        "1"
    );
}

#[test]
fn json_value_returns_one_scalar_with_on_empty_and_on_error() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let cases = [
        (r#"'{"a": "x"}'::jsonb, '$.a'"#, "x"),
        (r#"'{"a": "12"}'::jsonb, '$.a' RETURNING int"#, "12"),
        (r#"'{"a": true}'::jsonb, '$.a' RETURNING boolean"#, "t"),
        (r#"'{"a": null}'::jsonb, '$.a'"#, "NULL"),
        (r#"'{"a": [1]}'::jsonb, '$.a'"#, "NULL"),
        (r#"'[1, 2]'::jsonb, 'strict $[*]' DEFAULT 9 ON ERROR"#, "9"),
        (r#"'{}'::jsonb, '$.a'"#, "NULL"),
        (r#"'{}'::jsonb, '$.a' DEFAULT 'none' ON EMPTY"#, "none"),
        (r#"'{"a": "abc"}'::jsonb, '$.a' RETURNING int"#, "NULL"),
        (
            r#"'{"a": "abc"}'::jsonb, '$.a' RETURNING int DEFAULT -1 ON ERROR"#,
            "-1",
        ),
        (
            r#"'"2017-03-10"'::jsonb, '$.datetime()' RETURNING date"#,
            "2017-03-10",
        ),
        (
            r#"'{"a": 5}'::jsonb, '$.a + $b' PASSING 2 AS b RETURNING int"#,
            "7",
        ),
    ];
    for (arguments, expected) in cases {
        assert_eq!(
            one(&mut session, &format!("SELECT JSON_VALUE({arguments})")),
            expected,
            "{arguments}"
        );
    }
    assert_eq!(
        one(
            &mut session,
            r#"SELECT JSON_VALUE('{"a": "12"}'::jsonb, '$.a' RETURNING int) + 1"#
        ),
        "13"
    );
    let failures = [
        (r#"'[1, 2]'::jsonb, '$[*]' ERROR ON ERROR"#, "22034"),
        (r#"'{"a": [1]}'::jsonb, '$.a' ERROR ON ERROR"#, "2203F"),
        (r#"'{}'::jsonb, '$.a' ERROR ON EMPTY"#, "22035"),
        (
            r#"'{"a": "abc"}'::jsonb, '$.a' RETURNING int ERROR ON ERROR"#,
            "22P02",
        ),
        (r#"'{}'::jsonb, '$.a' EMPTY ON ERROR"#, "42601"),
        (r#"'{}'::jsonb, '$.a' NULL ON EMPTY NULL ON EMPTY"#, "42601"),
        (r#"'{'::jsonb, '$.a'"#, "22P02"),
    ];
    for (arguments, expected) in failures {
        assert_eq!(
            state(&mut session, &format!("SELECT JSON_VALUE({arguments})")),
            expected,
            "{arguments}"
        );
    }
}

#[test]
fn json_query_wraps_and_keeps_or_omits_quotes() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let cases = [
        (r#"'{"a": [1, 2]}'::jsonb, '$.a'"#, "[1, 2]"),
        (r#"'{"a": "x"}'::jsonb, '$.a'"#, r#""x""#),
        (
            r#"'{"a": "x"}'::jsonb, '$.a' RETURNING text OMIT QUOTES"#,
            "x",
        ),
        (
            r#"'{"a": "x"}'::jsonb, '$.a' RETURNING text KEEP QUOTES"#,
            r#""x""#,
        ),
        (r#"'[1, 2]'::jsonb, '$[*]'"#, "NULL"),
        (r#"'[1, 2]'::jsonb, '$[*]' WITH WRAPPER"#, "[1, 2]"),
        (
            r#"'[1, 2]'::jsonb, '$[0]' WITH UNCONDITIONAL ARRAY WRAPPER"#,
            "[1]",
        ),
        (
            r#"'[1, [2, 3], null]'::jsonb, 'lax $[*][$off]' PASSING 1 AS off WITH CONDITIONAL WRAPPER"#,
            "3",
        ),
        (r#"'{}'::jsonb, '$.a' WITH WRAPPER"#, "[]"),
        (r#"'{}'::jsonb, '$.a'"#, "NULL"),
        (r#"'{}'::jsonb, '$.a' EMPTY ARRAY ON EMPTY"#, "[]"),
        (r#"'{}'::jsonb, '$.a' EMPTY OBJECT ON EMPTY"#, "{}"),
        (r#"'{"a": null}'::jsonb, '$.a'"#, "null"),
        (r#"'{"a": 3}'::jsonb, '$.a' RETURNING int"#, "3"),
        (
            r#"'[1, 2]'::jsonb, '$[*]' DEFAULT '"many"' ON ERROR"#,
            r#""many""#,
        ),
    ];
    for (arguments, expected) in cases {
        assert_eq!(
            one(&mut session, &format!("SELECT JSON_QUERY({arguments})")),
            expected,
            "{arguments}"
        );
    }
    let failures = [
        (r#"'[1, 2]'::jsonb, '$[*]' ERROR ON ERROR"#, "22034"),
        (r#"'{}'::jsonb, '$.a' ERROR ON EMPTY"#, "22035"),
        (r#"'[1]'::jsonb, '$' WITH WRAPPER OMIT QUOTES"#, "42601"),
        (r#"'[1]'::jsonb, '$' TRUE ON ERROR"#, "42601"),
    ];
    for (arguments, expected) in failures {
        assert_eq!(
            state(&mut session, &format!("SELECT JSON_QUERY({arguments})")),
            expected,
            "{arguments}"
        );
    }
    assert!(
        run(
            &mut session,
            r#"SELECT JSON_QUERY('{"a": "[1, 2]"}'::jsonb, 'lax $.a' RETURNING int[] OMIT QUOTES ERROR ON ERROR)"#
        )
        .is_err()
    );
}

#[test]
fn sql_json_functions_name_their_columns_as_postgresql_does() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let output = run(
        &mut session,
        r#"SELECT json_value('{"a": 1}', '$.a'), json_query('{"a": 1}', '$'), json_exists('{"a": 1}', '$.a')"#,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        output.lines().next(),
        Some("json_value|json_query|json_exists")
    );
}

#[test]
fn json_table_builds_rows_from_columns() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT * FROM JSON_TABLE('[{"a": 1, "b": "x", "c": [1, 2]}, {"a": 2}]'::jsonb, '$[*]' COLUMNS (id FOR ORDINALITY, a int PATH '$.a', b text, d text DEFAULT 'none' ON EMPTY, has_b boolean EXISTS PATH '$.b', c jsonb, k text PATH '$.b' KEEP QUOTES)) AS jt"#
        ),
        [r#"1|1|x|none|t|[1, 2]|"x""#, "2|2|NULL|none|f|NULL|NULL"]
    );
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT jt.v FROM JSON_TABLE('[{"v": 1}, {"v": 5}, {"v": 9}]'::jsonb, '$[*] ? (@.v > $min)' PASSING 3 AS min COLUMNS (v int)) jt"#
        ),
        ["5", "9"]
    );
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT * FROM JSON_TABLE('{"v": [1, 2]}'::jsonb, '$' COLUMNS (w jsonb PATH '$.v[*]' WITH WRAPPER, x int PATH '$.v[*]', y int PATH '$.v[*]' DEFAULT 0 ON ERROR))"#
        ),
        ["[1, 2]|NULL|0"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT count(*) FROM JSON_TABLE(NULL::jsonb, '$' COLUMNS (a int))"
        ),
        ["0"]
    );
}

#[test]
fn json_table_nested_paths_join_children_and_union_siblings() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT * FROM JSON_TABLE('{"items": [{"k": "a", "v": [1, 2]}, {"k": "b", "v": []}]}'::jsonb, '$.items[*]' COLUMNS (k text PATH '$.k', NESTED PATH '$.v[*]' COLUMNS (n FOR ORDINALITY, v int PATH '$')))"#
        ),
        ["a|1|1", "a|2|2", "b|NULL|NULL"]
    );
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT * FROM JSON_TABLE('{"m": [1, 2], "b": [3]}'::jsonb, '$' COLUMNS (NESTED PATH '$.m[*]' COLUMNS (m int PATH '$'), NESTED '$.b[*]' AS books COLUMNS (b int PATH '$')))"#
        ),
        ["1|NULL", "2|NULL", "NULL|3"]
    );
}

#[test]
fn json_table_errors_follow_their_on_error_clauses() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT count(*) FROM JSON_TABLE('{}'::jsonb, 'strict $.x' COLUMNS (a int))"
        ),
        ["0"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT count(*) FROM JSON_TABLE('{}'::jsonb, 'strict $.x' COLUMNS (a int) EMPTY ARRAY ON ERROR)"
        ),
        ["0"]
    );
    assert_eq!(
        state(
            &mut session,
            "SELECT * FROM JSON_TABLE('{}'::jsonb, 'strict $.x' COLUMNS (a int) ERROR ON ERROR)"
        ),
        "2203A"
    );
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT * FROM JSON_TABLE('[{"a": "x"}]'::jsonb, '$[*]' COLUMNS (a int))"#
        ),
        ["NULL"]
    );
    let failures = [
        (
            r#"SELECT * FROM JSON_TABLE('[{"a": "x"}]'::jsonb, '$[*]' COLUMNS (a int ERROR ON ERROR))"#,
            "22P02",
        ),
        (
            r#"SELECT * FROM JSON_TABLE('[{}]'::jsonb, '$[*]' COLUMNS (a int ERROR ON EMPTY))"#,
            "22035",
        ),
        (
            "SELECT * FROM JSON_TABLE('[]'::jsonb, '$' COLUMNS (a int, a text))",
            "42712",
        ),
        (
            "SELECT * FROM JSON_TABLE('[]'::jsonb, '$' || '' COLUMNS (a int))",
            "42601",
        ),
        (
            "SELECT * FROM JSON_TABLE('[]'::jsonb, '$' COLUMNS (a int EXISTS NULL ON ERROR))",
            "42601",
        ),
        (
            "SELECT * FROM JSON_TABLE('[]'::jsonb, '$[' COLUMNS (a int))",
            "42601",
        ),
    ];
    for (sql, expected) in failures {
        assert_eq!(state(&mut session, sql), expected, "{sql}");
    }
}

#[test]
fn json_table_reads_each_row_of_a_table_before_it() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE docs (id int, js jsonb)").expect("create");
    run(
        &mut session,
        r#"INSERT INTO docs VALUES (1, '[1, 2]'), (2, '[3]'), (3, '[]')"#,
    )
    .expect("insert");
    assert_eq!(
        rows(
            &mut session,
            "SELECT d.id, jt.a FROM docs d, JSON_TABLE(d.js, '$[*]' COLUMNS (a int PATH '$')) jt ORDER BY d.id, jt.a"
        ),
        ["1|1", "1|2", "2|3"]
    );
}

#[test]
fn json_populate_record_fills_a_table_row_type_or_a_composite_type() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE t (a int, b text, c jsonb)").expect("create");
    run(&mut session, "CREATE TYPE pt AS (x int, y int)").expect("type");
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT * FROM json_populate_record(null::t, '{"a": 1, "b": "x", "d": 5}')"#
        ),
        ["1|x|NULL"]
    );
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT a, c FROM jsonb_populate_record(CAST(NULL AS t), '{"a": 2, "c": {"k": [1]}}')"#
        ),
        [r#"2|{"k": [1]}"#]
    );
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT a FROM jsonb_populate_recordset(NULL::t, '[{"a": 1}, {"a": 2}]') ORDER BY a"#
        ),
        ["1", "2"]
    );
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT * FROM json_populate_record(null::pt, '{"x": 3, "y": 4}')"#
        ),
        ["3|4"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM json_populate_record(null::pt, NULL)"
        ),
        ["NULL|NULL"]
    );
    run(
        &mut session,
        r#"INSERT INTO t SELECT * FROM json_populate_recordset(null::t, '[{"a": 7, "b": "y"}, {"a": 8}]')"#,
    )
    .expect("insert");
    assert_eq!(
        rows(&mut session, "SELECT a, b FROM t ORDER BY a"),
        ["7|y", "8|NULL"]
    );
    let failures = [
        (
            r#"SELECT * FROM json_populate_record(null::missing, '{}')"#,
            "42704",
        ),
        (
            r#"SELECT * FROM json_populate_record(null::t, '[1]')"#,
            "22023",
        ),
        (
            r#"SELECT * FROM json_populate_recordset(null::t, '{"a": 1}')"#,
            "22023",
        ),
        (
            r#"SELECT * FROM json_populate_record(null::int, '{}')"#,
            "42804",
        ),
        (
            r#"SELECT * FROM json_populate_record(null::t, '{"a": "x"}')"#,
            "22P02",
        ),
    ];
    for (sql, expected) in failures {
        assert_eq!(state(&mut session, sql), expected, "{sql}");
    }
}

#[test]
fn json_populate_record_takes_missing_fields_from_a_base_row() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TYPE pt AS (x int, y int, label text)").expect("type");
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT * FROM json_populate_record(ROW(1, 2, 'base')::pt, '{"y": 20}')"#
        ),
        ["1|20|base"]
    );
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT x, y FROM jsonb_populate_recordset(ROW(0, 9, 'b')::pt, '[{"x": 1}, {"x": 2, "y": null}]') ORDER BY x"#
        ),
        ["1|9", "2|NULL"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM json_populate_record(ROW(5, 6, 'kept')::pt, NULL)"
        ),
        ["5|6|kept"]
    );
}

#[test]
fn is_json_tests_shape_and_unique_keys() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let cases = [
        ("SELECT '{\"a\": 1}' IS JSON", "t"),
        ("SELECT 'abc' IS JSON", "f"),
        ("SELECT 'abc' IS NOT JSON", "t"),
        ("SELECT '[1, 2]' IS JSON ARRAY", "t"),
        ("SELECT '\"\\u0000\"' IS JSON", "t"),
        ("SELECT '{\"a\": \"\\u0000\"}' IS JSON OBJECT", "t"),
        ("SELECT '[1, 2]' IS JSON OBJECT", "f"),
        ("SELECT '\"x\"' IS JSON SCALAR", "t"),
        ("SELECT '{}' IS JSON SCALAR", "f"),
        ("SELECT '{}' IS JSON VALUE", "t"),
        ("SELECT '{\"a\": 1, \"a\": 2}' IS JSON", "t"),
        (
            "SELECT '{\"a\": 1, \"a\": 2}' IS JSON WITH UNIQUE KEYS",
            "f",
        ),
        (
            "SELECT '{\"a\": 1, \"a\": 2}' IS JSON OBJECT WITHOUT UNIQUE",
            "t",
        ),
        (
            "SELECT '[{\"k\": 1, \"k\": 2}]'::json IS JSON WITH UNIQUE",
            "f",
        ),
        (
            "SELECT '{\"a\": 1, \"a\": 2}'::jsonb IS JSON WITH UNIQUE KEYS",
            "t",
        ),
        ("SELECT NULL::text IS JSON", "NULL"),
        ("SELECT '\\x5b315d'::bytea IS JSON ARRAY", "t"),
    ];
    for (sql, expected) in cases {
        assert_eq!(one(&mut session, sql), expected, "{sql}");
    }
    assert_eq!(state(&mut session, "SELECT 1 IS JSON"), "42804");
    assert_eq!(
        run(&mut session, "SELECT '[]' IS JSON AS a, '1' IS JSON OBJECT")
            .expect("columns")
            .lines()
            .next(),
        Some("a|?column?")
    );
    run(
        &mut session,
        "CREATE TABLE docs (id int, body text CHECK (body IS JSON OBJECT))",
    )
    .expect("create");
    run(&mut session, "INSERT INTO docs VALUES (1, '{\"a\": 1}')").expect("insert");
    assert_eq!(
        state(&mut session, "INSERT INTO docs VALUES (2, '[1]')"),
        "23514"
    );
    assert_eq!(
        rows(&mut session, "SELECT id FROM docs WHERE body IS JSON"),
        ["1"]
    );
}

#[test]
fn json_json_scalar_and_json_serialize_construct_and_print_json() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let cases = [
        (
            "SELECT json('{\"a\":123, \"b\":[true,\"foo\"], \"a\":\"bar\"}')",
            "{\"a\":123, \"b\":[true,\"foo\"], \"a\":\"bar\"}",
        ),
        ("SELECT json('[1, 2]' WITH UNIQUE KEYS)", "[1, 2]"),
        ("SELECT json(NULL::text)", "NULL"),
        ("SELECT pg_typeof(json('1'))::text", "json"),
        ("SELECT json_scalar(123.45)", "123.45"),
        (
            "SELECT json_scalar('he said \"hi\"')",
            "\"he said \\\"hi\\\"\"",
        ),
        ("SELECT json_scalar(true)", "true"),
        ("SELECT json_scalar('2024-05-10'::date)", "\"2024-05-10\""),
        ("SELECT json_scalar(NULL::int)", "NULL"),
        ("SELECT json_serialize('{ \"a\" : 1 } ')", "{ \"a\" : 1 } "),
        (
            "SELECT json_serialize('{ \"a\" : 1 } ' RETURNING bytea)",
            "\\x7b20226122203a2031207d20",
        ),
        (
            "SELECT json_serialize('{\"b\": 1, \"a\": 2}'::jsonb RETURNING varchar)",
            "{\"a\": 2, \"b\": 1}",
        ),
        ("SELECT pg_typeof(json_serialize('1'))::text", "text"),
    ];
    for (sql, expected) in cases {
        assert_eq!(one(&mut session, sql), expected, "{sql}");
    }
    assert_eq!(
        run(
            &mut session,
            "SELECT json('1'), json_scalar(1), json_serialize('1')"
        )
        .expect("columns")
        .lines()
        .next(),
        Some("json|json_scalar|json_serialize")
    );
    let failures = [
        (
            "SELECT json('{\"a\": 1, \"a\": 2}' WITH UNIQUE KEYS)",
            "22030",
        ),
        ("SELECT json('{oops')", "22P02"),
        ("SELECT json(42)", "42804"),
        ("SELECT json_serialize('[1]' RETURNING integer)", "42804"),
        ("SELECT json_serialize('not json')", "22P02"),
    ];
    for (sql, expected) in failures {
        assert_eq!(state(&mut session, sql), expected, "{sql}");
    }
}

#[test]
fn the_decimal_path_method_rounds_to_its_precision_and_scale() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let cases = [
        (
            "SELECT jsonb_path_query('1234.5678', '$.decimal(6, 2)')",
            "1234.57",
        ),
        (
            "SELECT jsonb_path_query('1234.5678', '$.decimal()')",
            "1234.5678",
        ),
        (
            "SELECT jsonb_path_query('\"12.345\"', '$.decimal(4, 1)')",
            "12.3",
        ),
        (
            "SELECT jsonb_path_query('1234.5678', '$.decimal(6)')",
            "1235",
        ),
        (
            "SELECT jsonb_path_query('1250', '$.decimal(4, -2)')",
            "1300",
        ),
        (
            "SELECT jsonb_path_query('[1.25, 2.5]', '$.decimal(3, 1)')",
            "1.3",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(
            rows(&mut session, sql).first().map(String::as_str),
            Some(expected),
            "{sql}"
        );
    }
    let failures = [
        (
            "SELECT jsonb_path_query('1234.5678', '$.decimal(4, 2)')",
            "2203G",
        ),
        ("SELECT jsonb_path_query('1', '$.decimal(0)')", "22023"),
        (
            "SELECT jsonb_path_query('1', '$.decimal(9999999999)')",
            "22023",
        ),
        (
            "SELECT jsonb_path_query('\"abc\"', '$.decimal(4)')",
            "2203G",
        ),
        ("SELECT jsonb_path_query('1', '$.decimal(1.5)')", "42601"),
    ];
    for (sql, expected) in failures {
        assert_eq!(state(&mut session, sql), expected, "{sql}");
    }
}

#[test]
fn oracle_shares_json_value_json_query_json_exists_and_json_table() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    assert_eq!(
        one(
            &mut session,
            r#"SELECT JSON_VALUE('{"a": {"b": 5}}', '$.a.b' RETURNING NUMBER) FROM DUAL"#
        ),
        "5"
    );
    assert_eq!(
        one(
            &mut session,
            r#"SELECT JSON_VALUE('{"a": "x"}', '$.b' DEFAULT 'none' ON EMPTY) FROM DUAL"#
        ),
        "none"
    );
    assert_eq!(
        one(
            &mut session,
            r#"SELECT JSON_QUERY('{"a": [1, 2]}', '$.a' WITH WRAPPER) FROM DUAL"#
        ),
        "[[1, 2]]"
    );
    assert_eq!(
        one(
            &mut session,
            r#"SELECT 1 FROM DUAL WHERE JSON_EXISTS('{"a": 1}', '$.a')"#
        ),
        "1"
    );
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT jt.x, jt.y FROM JSON_TABLE('[{"x": 1, "y": "p"}, {"x": 2}]', '$[*]' COLUMNS (x NUMBER PATH '$.x', y PATH '$.y')) jt"#
        ),
        ["1|p", "2|NULL"]
    );
}
