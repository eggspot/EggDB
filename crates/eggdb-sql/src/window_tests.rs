use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Session, SqlError};

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

fn answer(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => output,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn state(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error.state().code().to_owned(),
    }
}

fn with_sales(store: &MemoryStore) -> Session<'_, MemoryStore> {
    let mut session = Session::new(store);
    answer(
        &mut session,
        "CREATE TABLE s (id integer PRIMARY KEY, r text, a integer);
         INSERT INTO s VALUES (1, 'e', 10), (2, 'e', 20), (3, 'e', 20), (4, 'w', 5), (5, 'w', 15)",
    );
    session
}

#[test]
fn aggregates_over_windows_run_and_total_each_partition() {
    let store = MemoryStore::default();
    let mut session = with_sales(&store);
    assert_eq!(
        answer(
            &mut session,
            "SELECT id, sum(a) OVER (PARTITION BY r ORDER BY id) AS run, sum(a) OVER (PARTITION BY r) AS total FROM s ORDER BY id"
        ),
        "id|run|total\n1|10|50\n2|30|50\n3|50|50\n4|5|20\n5|20|20\nSELECT 5"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT r, sum(a) AS t, rank() OVER (ORDER BY sum(a) DESC) AS k FROM s GROUP BY r ORDER BY r"
        ),
        "r|t|k\ne|50|1\nw|20|2\nSELECT 2"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT id, string_agg(r, ',') OVER (ORDER BY id ROWS 1 PRECEDING) AS g FROM s ORDER BY id"
        ),
        "id|g\n1|e\n2|e,e\n3|e,e\n4|e,w\n5|w,w\nSELECT 5"
    );
}

#[test]
fn value_functions_read_neighbours_and_frame_ends() {
    let store = MemoryStore::default();
    let mut session = with_sales(&store);
    assert_eq!(
        answer(
            &mut session,
            "SELECT id, lag(a) OVER w AS prior_a, lead(a, 1, 0) OVER w AS next_a FROM s WINDOW w AS (PARTITION BY r ORDER BY id) ORDER BY id"
        ),
        "id|prior_a|next_a\n1|NULL|20\n2|10|20\n3|20|0\n4|NULL|15\n5|5|0\nSELECT 5"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT DISTINCT r, first_value(a) OVER w AS f, last_value(a) OVER w AS l, nth_value(id, 2) OVER w AS n FROM s WINDOW w AS (PARTITION BY r ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) ORDER BY r"
        ),
        "r|f|l|n\ne|10|20|2\nw|5|15|5\nSELECT 2"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT id, ntile(2) OVER (ORDER BY id) AS t, (cume_dist() OVER (ORDER BY id) * 5)::int AS c FROM s ORDER BY id"
        ),
        "id|t|c\n1|1|1\n2|1|2\n3|1|3\n4|2|4\n5|2|5\nSELECT 5"
    );
}

#[test]
fn window_calls_run_inside_expressions_and_order_by() {
    let store = MemoryStore::default();
    let mut session = with_sales(&store);
    assert_eq!(
        answer(
            &mut session,
            "SELECT id, a, rank() OVER (ORDER BY a DESC) * 10 AS r10 FROM s ORDER BY rank() OVER (ORDER BY a DESC), id"
        ),
        "id|a|r10\n2|20|10\n3|20|10\n5|15|30\n1|10|40\n4|5|50\nSELECT 5"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT id FROM (SELECT id, row_number() OVER (PARTITION BY r ORDER BY a DESC, id) AS n FROM s) t WHERE n = 1 ORDER BY id"
        ),
        "id\n2\n5\nSELECT 2"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT id, rank() OVER (w ORDER BY a) AS k FROM s WINDOW w AS (PARTITION BY r) ORDER BY id"
        ),
        "id|k\n1|1\n2|2\n3|2\n4|1\n5|2\nSELECT 5"
    );
}

#[test]
fn frames_count_rows_values_groups_and_exclusions() {
    let store = MemoryStore::default();
    let mut session = with_sales(&store);
    assert_eq!(
        answer(
            &mut session,
            "SELECT id, sum(a) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS s3 FROM s ORDER BY id"
        ),
        "id|s3\n1|30\n2|50\n3|45\n4|40\n5|20\nSELECT 5"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT id, count(*) OVER (ORDER BY a RANGE BETWEEN 5 PRECEDING AND CURRENT ROW) AS c FROM s ORDER BY id"
        ),
        "id|c\n1|2\n2|3\n3|3\n4|1\n5|2\nSELECT 5"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT id, count(*) OVER (ORDER BY a GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) AS c FROM s ORDER BY id"
        ),
        "id|c\n1|2\n2|3\n3|3\n4|1\n5|2\nSELECT 5"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT id, sum(a) OVER (PARTITION BY r ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE CURRENT ROW) AS others FROM s ORDER BY id"
        ),
        "id|others\n1|40\n2|30\n3|30\n4|15\n5|5\nSELECT 5"
    );
}

#[test]
fn filter_and_ordered_set_aggregates_compute_like_postgresql() {
    let store = MemoryStore::default();
    let mut session = with_sales(&store);
    assert_eq!(
        answer(
            &mut session,
            "SELECT count(*) FILTER (WHERE a > 10) AS big, sum(a) FILTER (WHERE r = 'e') AS e FROM s"
        ),
        "big|e\n3|50\nSELECT 1"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT id, count(*) FILTER (WHERE a >= 15) OVER (ORDER BY id) AS c FROM s ORDER BY id"
        ),
        "id|c\n1|0\n2|1\n3|2\n4|2\n5|3\nSELECT 5"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY a) AS m, mode() WITHIN GROUP (ORDER BY a) AS mo, percentile_cont(0.5) WITHIN GROUP (ORDER BY a) AS c FROM s"
        ),
        "m|mo|c\n15|20|15\nSELECT 1"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT r, percentile_cont(0.25) WITHIN GROUP (ORDER BY a) AS q FROM s GROUP BY r ORDER BY r"
        ),
        "r|q\ne|15\nw|7.5\nSELECT 2"
    );
}

#[test]
fn misplaced_or_malformed_windows_fail_like_postgresql() {
    let store = MemoryStore::default();
    let mut session = with_sales(&store);
    for (sql, expected) in [
        ("SELECT id FROM s WHERE row_number() OVER () > 1", "42P20"),
        ("SELECT sum(row_number() OVER ()) FROM s", "42803"),
        (
            "SELECT row_number() OVER (ROWS BETWEEN CURRENT ROW AND 1 PRECEDING) FROM s",
            "42P20",
        ),
        ("SELECT row_number() OVER w FROM s", "42704"),
        (
            "SELECT sum(a) OVER (RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM s",
            "42P20",
        ),
        ("SELECT ntile(0) OVER () FROM s", "22014"),
        (
            "SELECT sum(a) OVER (ORDER BY id ROWS -1 PRECEDING) FROM s",
            "22013",
        ),
        ("SELECT count(DISTINCT a) OVER () FROM s", "0A000"),
        ("SELECT abs(a) OVER () FROM s", "42809"),
        (
            "SELECT percentile_cont(2) WITHIN GROUP (ORDER BY a) FROM s",
            "22003",
        ),
        (
            "SELECT row_number() OVER (w ORDER BY id) FROM s WINDOW w AS (ORDER BY a)",
            "42P20",
        ),
        (
            "SELECT sum(a) OVER (ORDER BY id ROWS id PRECEDING) FROM s",
            "42P10",
        ),
    ] {
        assert_eq!(state(&mut session, sql), expected, "{sql}");
    }
}
