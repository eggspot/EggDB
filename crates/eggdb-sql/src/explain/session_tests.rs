use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn last(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn postgresql_tables(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::new(store);
    for sql in [
        "CREATE TABLE tenk1 (unique1 integer PRIMARY KEY, ten integer, four integer, hundred integer)",
        "CREATE TABLE onek (unique1 integer PRIMARY KEY, ten integer, four integer)",
        "INSERT INTO tenk1 VALUES (1, 1, 1, 10), (2, 2, 2, 20), (3, 3, 1, 30)",
        "INSERT INTO onek VALUES (1, 5, 1), (2, 0, 2)",
    ] {
        last(&mut session, sql);
    }
    session
}

fn plan(session: &mut Session<'_>, sql: &str) -> Vec<String> {
    last(session, sql)
        .lines()
        .skip(1)
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_correlated_subquery_is_a_subplan_under_the_scan_that_filters_on_it() {
    let store = MemoryStore::default();
    let mut session = postgresql_tables(&store);
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT * FROM tenk1 t WHERE t.ten < ALL (SELECT o.ten FROM onek o WHERE o.four = t.four)"
        ),
        [
            "Seq Scan on tenk1 t",
            "  Filter: (SubPlan 1)",
            "  SubPlan 1",
            "    ->  Seq Scan on onek o",
            "EXPLAIN",
        ]
    );
}

#[test]
fn an_uncorrelated_scalar_subquery_is_an_initplan_returning_a_parameter() {
    let store = MemoryStore::default();
    let mut session = postgresql_tables(&store);
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT unique1 FROM tenk1 t WHERE t.ten < (SELECT min(o.ten) FROM onek o)"
        ),
        [
            "Seq Scan on tenk1 t",
            "  Filter: (ten < $0)",
            "  InitPlan 1 (returns $0)",
            "    ->  Seq Scan on onek o",
            "EXPLAIN",
        ]
    );
}

#[test]
fn an_uncorrelated_in_is_a_hashed_subplan_and_not_in_negates_it() {
    let store = MemoryStore::default();
    let mut session = postgresql_tables(&store);
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT unique1 FROM tenk1 WHERE hundred IN (SELECT ten FROM onek)"
        )[..3],
        [
            "Seq Scan on tenk1",
            "  Filter: (hashed SubPlan 1)",
            "  SubPlan 1",
        ]
    );
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT unique1 FROM tenk1 WHERE hundred NOT IN (SELECT ten FROM onek)"
        )[1],
        "  Filter: (NOT (hashed SubPlan 1))"
    );
}

#[test]
fn a_subquery_in_the_select_list_and_a_correlated_exists_are_subplans() {
    let store = MemoryStore::default();
    let mut session = postgresql_tables(&store);
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT t.unique1, (SELECT max(o.ten) FROM onek o WHERE o.four = t.four) AS most FROM tenk1 t"
        ),
        [
            "Seq Scan on tenk1 t",
            "  SubPlan 1",
            "    ->  Seq Scan on onek o",
            "EXPLAIN",
        ]
    );
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT unique1 FROM tenk1 t WHERE EXISTS (SELECT 1 FROM onek o WHERE o.four = t.four)"
        ),
        [
            "Seq Scan on tenk1 t",
            "  Filter: (SubPlan 1)",
            "  SubPlan 1",
            "    ->  Seq Scan on onek o",
            "EXPLAIN",
        ]
    );
}

#[test]
fn a_nested_subquery_is_numbered_first_and_drawn_under_the_subplan_that_uses_it() {
    let store = MemoryStore::default();
    let mut session = postgresql_tables(&store);
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT unique1 FROM tenk1 WHERE hundred IN (SELECT ten FROM onek WHERE four > (SELECT min(four) FROM onek))"
        ),
        [
            "Seq Scan on tenk1",
            "  Filter: (hashed SubPlan 2)",
            "  SubPlan 2",
            "    ->  Seq Scan on onek",
            "          Filter: (four > $0)",
            "          InitPlan 1 (returns $0)",
            "            ->  Seq Scan on onek",
            "EXPLAIN",
        ]
    );
}

#[test]
fn a_subplan_under_sort_and_limit_hangs_from_the_scan() {
    let store = MemoryStore::default();
    let mut session = postgresql_tables(&store);
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT unique1 FROM tenk1 t WHERE t.ten < (SELECT max(ten) FROM onek) ORDER BY hundred LIMIT 1"
        ),
        [
            "Limit",
            "  ->  Sort",
            "        ->  Seq Scan on tenk1 t",
            "              Filter: (ten < $0)",
            "              InitPlan 1 (returns $0)",
            "                ->  Seq Scan on onek",
            "EXPLAIN",
        ]
    );
}

fn actual(line: &str) -> Option<(u64, u64)> {
    let group = line.split_once(" (actual time=")?.1.strip_suffix(')')?;
    let (_, counts) = group.split_once(" rows=")?;
    let (rows, loops) = counts.split_once(" loops=")?;
    Some((rows.parse().ok()?, loops.parse().ok()?))
}

#[test]
fn explain_analyze_counts_a_correlated_subplans_loops_and_an_initplans_single_run() {
    let store = MemoryStore::default();
    let mut session = postgresql_tables(&store);
    let lines = plan(
        &mut session,
        "EXPLAIN ANALYZE SELECT unique1 FROM tenk1 t WHERE t.ten < ALL (SELECT o.ten FROM onek o WHERE o.four = t.four)",
    );
    assert!(
        lines[0].starts_with("Seq Scan on tenk1 t (actual time="),
        "{lines:?}"
    );
    assert_eq!(lines[1], "  Filter: (SubPlan 1)", "{lines:?}");
    assert_eq!(lines[2], "  SubPlan 1", "{lines:?}");
    assert!(
        lines[3].starts_with("    ->  Seq Scan on onek o (actual time="),
        "{lines:?}"
    );
    assert_eq!(
        actual(&lines[3]).map(|(_, loops)| loops),
        Some(3),
        "{lines:?}"
    );
    let lines = plan(
        &mut session,
        "EXPLAIN ANALYZE SELECT unique1 FROM tenk1 t WHERE t.ten < (SELECT max(o.ten) FROM onek o)",
    );
    assert_eq!(
        actual(&lines[0]).map(|(rows, _)| rows),
        Some(3),
        "{lines:?}"
    );
    assert_eq!(lines[2], "  InitPlan 1 (returns $0)", "{lines:?}");
    assert_eq!(actual(&lines[3]), Some((1, 1)), "{lines:?}");
}

#[test]
fn explain_analyze_of_a_subplan_that_never_ran_says_never_executed() {
    let store = MemoryStore::default();
    let mut session = postgresql_tables(&store);
    let lines = plan(
        &mut session,
        "EXPLAIN ANALYZE SELECT unique1 FROM tenk1 t WHERE t.unique1 > 100 AND EXISTS (SELECT 1 FROM onek o WHERE o.four = t.four)",
    );
    let subplan = lines
        .iter()
        .position(|line| line == "  SubPlan 1")
        .unwrap_or_else(|| panic!("{lines:?}"));
    assert_eq!(
        lines.get(subplan + 1).map(String::as_str),
        Some("    ->  Seq Scan on onek o (never executed)"),
        "{lines:?}"
    );
}

fn mysql_tables(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::MySql);
    for sql in [
        "CREATE TABLE t1 (id INT PRIMARY KEY, a INT, b INT)",
        "CREATE TABLE t2 (c1 INT, c2 INT)",
        "INSERT INTO t1 VALUES (1, 1, 10), (2, 2, 20), (3, 3, 30)",
        "INSERT INTO t2 VALUES (1, 1), (2, 2), (5, 5)",
    ] {
        last(&mut session, sql);
    }
    session
}

#[test]
fn mysql_format_tree_draws_iterators_four_spaces_per_level() {
    let store = MemoryStore::default();
    let mut session = mysql_tables(&store);
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN FORMAT=TREE SELECT * FROM t1 WHERE a > 1"
        ),
        "EXPLAIN\n-> Filter: (t1.a > 1)\n    -> Table scan on t1\n\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN FORMAT = TREE SELECT * FROM t1 WHERE id = 2"
        ),
        "EXPLAIN\n-> Single-row index lookup on t1 using PRIMARY (id=2)\n\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN FORMAT=TREE SELECT a FROM t1 ORDER BY b DESC LIMIT 2"
        ),
        "EXPLAIN\n-> Limit: 2 row(s)\n    -> Sort: t1.b DESC, limit input to 2 row(s) per chunk\n        -> Table scan on t1\n\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN FORMAT=TREE SELECT * FROM t2 JOIN t1 ON t1.id = t2.c1"
        ),
        "EXPLAIN\n-> Nested loop inner join\n    -> Table scan on t2\n    -> Single-row index lookup on t1 using PRIMARY (id=t2.c1)\n\nSELECT 1"
    );
}

#[test]
fn mysql_explain_analyze_runs_the_select_and_prints_actual_time_rows_and_loops() {
    let store = MemoryStore::default();
    let mut session = mysql_tables(&store);
    let text = last(
        &mut session,
        "EXPLAIN ANALYZE SELECT * FROM t1 JOIN t2 ON (t1.a = t2.c2)",
    );
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "EXPLAIN", "{text}");
    assert!(
        lines[1].starts_with("-> Inner hash join (t1.a = t2.c2)  (actual time="),
        "{text}"
    );
    assert!(lines[1].ends_with(" rows=2 loops=1)"), "{text}");
    assert_eq!(lines[2], "    -> Table scan on t1", "{text}");
    assert_eq!(lines[3], "    -> Hash", "{text}");
    assert_eq!(lines[4], "        -> Table scan on t2", "{text}");
    let text = last(
        &mut session,
        "EXPLAIN ANALYZE SELECT a FROM t1 WHERE a > (SELECT min(c1) FROM t2)",
    );
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[1].starts_with("-> Filter: (t1.a > (select #2))  (actual time="),
        "{text}"
    );
    assert!(
        lines[2].starts_with("    -> Table scan on t1  (actual time="),
        "{text}"
    );
    assert_eq!(
        lines[3], "    -> Select #2 (subquery in condition; run only once)",
        "{text}"
    );
    assert!(
        lines[4].starts_with("        -> Table scan on t2  (actual time="),
        "{text}"
    );
    assert!(lines[4].ends_with(" rows=1 loops=1)"), "{text}");
}

#[test]
fn mysql_refuses_tree_explains_of_writes_and_json_format() {
    let store = MemoryStore::default();
    let mut session = mysql_tables(&store);
    for sql in [
        "EXPLAIN ANALYZE DELETE FROM t1 WHERE id = 1",
        "EXPLAIN FORMAT=JSON SELECT * FROM t1",
    ] {
        assert!(run(&mut session, sql).is_err(), "{sql}");
    }
    assert!(last(&mut session, "SELECT count(*) FROM t1").ends_with("\n3\nSELECT 1"));
    assert!(
        last(
            &mut session,
            "EXPLAIN FORMAT=TRADITIONAL SELECT * FROM t1 WHERE id = 2"
        )
        .contains("|const|PRIMARY|PRIMARY|")
    );
}
