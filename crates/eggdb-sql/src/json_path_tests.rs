use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Session, SqlError};

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

const DOC: &str = r#"'{"a": [1, 2, 3, 4], "b": {"c": "x"}, "n": null}'::jsonb"#;

#[test]
fn jsonb_path_exists_and_match_answer_like_postgresql() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        one(
            &mut session,
            &format!("SELECT jsonb_path_exists({DOC}, '$.a[*] ? (@ > 3)')")
        ),
        "t"
    );
    assert_eq!(
        one(
            &mut session,
            &format!("SELECT jsonb_path_exists({DOC}, '$.missing')")
        ),
        "f"
    );
    assert_eq!(
        one(
            &mut session,
            &format!("SELECT jsonb_path_match({DOC}, 'exists($.b.c ? (@ starts with \"x\"))')")
        ),
        "t"
    );
    assert_eq!(
        one(
            &mut session,
            &format!(
                "SELECT jsonb_path_exists({DOC}, '$.a[*] ? (@ >= $min)', '{{\"min\": 4}}')"
            )
        ),
        "t"
    );
    assert_eq!(
        state(&mut session, &format!("SELECT jsonb_path_match({DOC}, '$.a')")),
        "22038"
    );
    assert_eq!(
        one(
            &mut session,
            &format!("SELECT jsonb_path_match({DOC}, '$.a', '{{}}', true) IS NULL")
        ),
        "t"
    );
}

#[test]
fn jsonb_path_query_returns_one_row_per_item() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        rows(
            &mut session,
            &format!("SELECT * FROM jsonb_path_query({DOC}, '$.a[*] ? (@ > 1)')")
        ),
        ["2", "3", "4"]
    );
    assert_eq!(
        rows(
            &mut session,
            &format!("SELECT jsonb_path_query({DOC}, '$.a[last]')")
        ),
        ["4"]
    );
    assert_eq!(
        rows(
            &mut session,
            &format!("SELECT jsonb_path_query({DOC}, 'strict $.zz', '{{}}', true)")
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        state(
            &mut session,
            &format!("SELECT * FROM jsonb_path_query({DOC}, 'strict $.zz')")
        ),
        "2203A"
    );
}

#[test]
fn jsonb_path_query_array_and_first_collect_items() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        one(
            &mut session,
            &format!("SELECT jsonb_path_query_array({DOC}, '$.a[*] ? (@ > 2)')")
        ),
        "[3, 4]"
    );
    assert_eq!(
        one(
            &mut session,
            &format!("SELECT jsonb_path_query_first({DOC}, '$.b.*')")
        ),
        "\"x\""
    );
    assert_eq!(
        one(
            &mut session,
            &format!("SELECT jsonb_path_query_first({DOC}, '$.zz') IS NULL")
        ),
        "t"
    );
    assert_eq!(
        one(
            &mut session,
            &format!("SELECT jsonb_path_query_array({DOC}, '$.a.size()')")
        ),
        "[4]"
    );
}

#[test]
fn path_operators_filter_rows_of_a_table() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE docs (id int, body jsonb)").unwrap();
    run(
        &mut session,
        r#"INSERT INTO docs VALUES (1, '{"tags": ["a", "b"], "price": 5}'), (2, '{"tags": ["c"], "price": 50}'), (3, '{"price": "x"}')"#,
    )
    .unwrap();
    assert_eq!(
        rows(
            &mut session,
            "SELECT id FROM docs WHERE body @? '$.tags[*] ? (@ == \"c\")' ORDER BY id"
        ),
        ["2"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT id FROM docs WHERE body @@ '$.price > 10' ORDER BY id"
        ),
        ["2"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT id, body @@ '$.price < 10' FROM docs ORDER BY id"
        ),
        ["1|t", "2|f", "3|NULL"]
    );
    assert_eq!(
        one(
            &mut session,
            "SELECT '{\"a\": 1}'::jsonb @? '$.a'::jsonpath"
        ),
        "t"
    );
}

#[test]
fn hostile_paths_fail_with_errors() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        state(&mut session, &format!("SELECT jsonb_path_exists({DOC}, '$.a[')")),
        "42601"
    );
    let deep = format!("{}$ {}", "(".repeat(5_000), ")".repeat(5_000));
    assert_eq!(
        state(
            &mut session,
            &format!("SELECT jsonb_path_exists({DOC}, '{deep}')")
        ),
        "54001"
    );
    assert_eq!(
        state(
            &mut session,
            &format!("SELECT jsonb_path_exists({DOC}, '$', '[1]')")
        ),
        "22023"
    );
    assert_eq!(
        state(
            &mut session,
            &format!("SELECT jsonb_path_query_array({DOC}, '$.a[*] * 10')")
        ),
        "22038"
    );
}

#[test]
fn date_trunc_of_an_interval_zeroes_the_smaller_fields() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        one(
            &mut session,
            "SELECT date_trunc('hour', interval '2 days 03:45:10')"
        ),
        "2 days 03:00:00"
    );
    assert_eq!(
        one(
            &mut session,
            "SELECT date_trunc('year', interval '3 years 5 months')"
        ),
        "3 years"
    );
    assert_eq!(
        state(
            &mut session,
            "SELECT date_trunc('week', interval '3 days')"
        ),
        "0A000"
    );
}

#[test]
fn ordered_aggregates_follow_their_order_by() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE t (k int, v text)").unwrap();
    run(
        &mut session,
        "INSERT INTO t VALUES (2, 'b'), (1, 'a'), (3, 'c')",
    )
    .unwrap();
    assert_eq!(
        one(
            &mut session,
            "SELECT string_agg(v, ',' ORDER BY k DESC) FROM t"
        ),
        "c,b,a"
    );
    assert_eq!(
        one(&mut session, "SELECT jsonb_agg(k ORDER BY v DESC) FROM t"),
        "[3, 2, 1]"
    );
    assert_eq!(
        one(
            &mut session,
            "SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY k) FROM t"
        ),
        "2"
    );
}

#[test]
fn json_to_record_reads_columns_from_a_definition_list() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT * FROM json_to_record('{"a": 1, "b": "x", "c": [1, 2]}') AS x(a int, b text, c jsonb, d text)"#
        ),
        ["1|x|[1, 2]|NULL"]
    );
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT a, b FROM jsonb_to_recordset('[{"a": 1, "b": true}, {"a": 2}]') AS (a bigint, b boolean) ORDER BY a"#
        ),
        ["1|t", "2|NULL"]
    );
    assert_eq!(
        rows(
            &mut session,
            r#"SELECT r.a + 1 FROM jsonb_to_record('{"a": 41}'::jsonb) AS r(a int)"#
        ),
        ["42"]
    );
    assert_eq!(
        state(
            &mut session,
            r#"SELECT * FROM json_to_record('[1]') AS x(a int)"#
        ),
        "22023"
    );
    assert_eq!(
        state(
            &mut session,
            r#"SELECT * FROM json_to_record('{"a": "x"}') AS x(a int)"#
        ),
        "22P02"
    );
    assert_eq!(
        state(&mut session, r#"SELECT * FROM json_to_record('{"a": 1}')"#),
        "42601"
    );
}
