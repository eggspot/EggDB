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

#[test]
fn explain_analyze_keeps_the_scan_rows_of_a_statement_whose_correlated_subquery_reads_more_rows() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "CREATE TABLE outer_rows (a integer, b integer)",
        "CREATE TABLE inner_rows (c1 integer)",
        "INSERT INTO outer_rows VALUES (1, 30), (2, 20), (3, 10)",
        "INSERT INTO inner_rows VALUES (1), (1), (3), (3), (5), (6), (7)",
    ] {
        last(&mut session, sql);
    }
    for test in [
        "EXISTS (SELECT 1 FROM inner_rows WHERE inner_rows.c1 = outer_rows.a)",
        "a IN (SELECT c1 FROM inner_rows WHERE inner_rows.c1 = outer_rows.a)",
        "(SELECT max(c1) FROM inner_rows WHERE inner_rows.c1 = outer_rows.a) IS NOT NULL",
        "(SELECT count(*) FROM inner_rows WHERE inner_rows.c1 = outer_rows.a) > 0",
    ]
    .into_iter()
    .map(|test| {
        (
            format!("SELECT a FROM outer_rows WHERE {test} ORDER BY b"),
            2,
        )
    })
    .chain([
        (
            "SELECT a, (SELECT max(c1) FROM inner_rows WHERE inner_rows.c1 = outer_rows.a) \
             FROM outer_rows"
                .to_owned(),
            3,
        ),
        (
            "SELECT a, (SELECT count(*) FROM inner_rows WHERE inner_rows.c1 = outer_rows.a) \
             FROM outer_rows"
                .to_owned(),
            3,
        ),
    ]) {
        let (query, rows) = test;
        let lines = plan(&mut session, &format!("EXPLAIN ANALYZE {query}"));
        let scan = lines
            .iter()
            .find(|line| {
                line.trim_start()
                    .trim_start_matches("->  ")
                    .starts_with("Seq Scan on outer_rows")
            })
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert_eq!(actual(scan).map(|(rows, _)| rows), Some(rows), "{lines:?}");
    }
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    let text = last(
        &mut mysql,
        "EXPLAIN ANALYZE SELECT a FROM outer_rows WHERE EXISTS \
         (SELECT 1 FROM inner_rows WHERE inner_rows.c1 = outer_rows.a) ORDER BY b",
    );
    let filter = text
        .lines()
        .find(|line| line.trim_start().starts_with("-> Filter:"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(filter.ends_with(" rows=2 loops=1)"), "{text}");
    let text = last(
        &mut mysql,
        "EXPLAIN ANALYZE SELECT a, (SELECT max(c1) FROM inner_rows \
         WHERE inner_rows.c1 = outer_rows.a) FROM outer_rows",
    );
    let scan = text
        .lines()
        .find(|line| line.trim_start().starts_with("-> Table scan on outer_rows"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(scan.ends_with(" rows=3 loops=1)"), "{text}");
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
fn mysql_format_tree_draws_iterators_four_spaces_per_level_with_their_estimates() {
    let store = MemoryStore::default();
    let mut session = mysql_tables(&store);
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN FORMAT=TREE SELECT * FROM t1 WHERE a > 1"
        ),
        "EXPLAIN\n-> Filter: (t1.a > 1)  (cost=0.55 rows=1)\n    -> Table scan on t1  (cost=0.55 rows=3)\n\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN FORMAT = TREE SELECT * FROM t1 WHERE id = 2"
        ),
        "EXPLAIN\n-> Single-row index lookup on t1 using PRIMARY (id=2)  (cost=0.35 rows=1)\n\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN FORMAT=TREE SELECT a FROM t1 ORDER BY b DESC LIMIT 2"
        ),
        "EXPLAIN\n-> Limit: 2 row(s)  (cost=1.03 rows=2)\n    -> Sort: t1.b DESC, limit input to 2 row(s) per chunk  (cost=1.03 rows=2)\n        -> Table scan on t1  (cost=0.55 rows=3)\n\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN FORMAT=TREE SELECT * FROM t2 JOIN t1 ON t1.id = t2.c1"
        ),
        "EXPLAIN\n-> Nested loop inner join  (cost=1.6 rows=3)\n    -> Table scan on t2  (cost=0.55 rows=3)\n    -> Single-row index lookup on t1 using PRIMARY (id=t2.c1)  (cost=0.35 rows=1)\n\nSELECT 1"
    );
}

#[test]
fn mysql_estimates_follow_the_row_counts_analyze_table_records() {
    let store = MemoryStore::default();
    let mut session = mysql_tables(&store);
    for row in 4..=12 {
        last(
            &mut session,
            &format!("INSERT INTO t1 VALUES ({row}, {row}, {row})"),
        );
    }
    last(&mut session, "ANALYZE TABLE t1");
    last(&mut session, "INSERT INTO t1 VALUES (13, 13, 13)");
    assert_eq!(
        last(&mut session, "EXPLAIN FORMAT=TREE SELECT * FROM t1"),
        "EXPLAIN\n-> Table scan on t1  (cost=1.45 rows=12)\n\nSELECT 1"
    );
}

#[test]
fn mysql_tree_draws_aggregate_group_aggregate_and_stream_results() {
    let store = MemoryStore::default();
    let mut session = mysql_tables(&store);
    assert_eq!(
        last(&mut session, "EXPLAIN FORMAT=TREE SELECT count(*) FROM t1"),
        "EXPLAIN\n-> Aggregate: count(0)  (cost=0.85 rows=1)\n    -> Table scan on t1  (cost=0.55 rows=3)\n\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN FORMAT=TREE SELECT a, sum(b) FROM t1 GROUP BY a ORDER BY a"
        ),
        "EXPLAIN\n-> Sort: t1.a  (cost=0.95 rows=1)\n    -> Stream results  (cost=0.85 rows=1)\n        -> Group aggregate: sum(t1.b)  (cost=0.85 rows=1)\n            -> Table scan on t1  (cost=0.55 rows=3)\n\nSELECT 1"
    );
    let text = last(
        &mut session,
        "EXPLAIN FORMAT=TREE SELECT a FROM t1 GROUP BY a HAVING count(*) > 1",
    );
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[1].starts_with("-> Filter: (count("), "{text}");
    assert!(lines[1].ends_with("  (cost=0.85 rows=0.333)"), "{text}");
    assert_eq!(
        lines[2], "    -> Group aggregate: count(0)  (cost=0.85 rows=1)",
        "{text}"
    );
    assert_eq!(
        lines[3], "        -> Table scan on t1  (cost=0.55 rows=3)",
        "{text}"
    );
    let text = last(
        &mut session,
        "EXPLAIN FORMAT=TREE SELECT a FROM t1 GROUP BY a",
    );
    assert!(
        text.contains("-> Group (no aggregates)  (cost=0.85 rows=1)"),
        "{text}"
    );
    let text = last(
        &mut session,
        "EXPLAIN FORMAT=TREE SELECT * FROM t2 JOIN t1 ON t1.id = t2.c1 ORDER BY t2.c2",
    );
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[1].starts_with("-> Sort: t2.c2  (cost="), "{text}");
    assert!(lines[2].starts_with("    -> Stream results  (cost="), "{text}");
    assert!(
        lines[3].starts_with("        -> Nested loop inner join  (cost="),
        "{text}"
    );
    let text = last(
        &mut session,
        "EXPLAIN ANALYZE SELECT a, count(*) FROM t1 GROUP BY a",
    );
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[1].starts_with("-> Group aggregate: count(0)  (cost=0.85 rows=1) (actual time="),
        "{text}"
    );
    assert!(lines[1].ends_with(" rows=3 loops=1)"), "{text}");
    assert!(
        lines[2].starts_with("    -> Table scan on t1  (cost=0.55 rows=3) (actual time="),
        "{text}"
    );
    assert!(lines[2].ends_with(" rows=3 loops=1)"), "{text}");
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
        lines[1].starts_with(
            "-> Inner hash join (t1.a = t2.c2)  (cost=1.7 rows=0.9) (actual time="
        ),
        "{text}"
    );
    assert!(lines[1].ends_with(" rows=2 loops=1)"), "{text}");
    assert_eq!(
        lines[2], "    -> Table scan on t1  (cost=0.55 rows=3)",
        "{text}"
    );
    assert_eq!(lines[3], "    -> Hash", "{text}");
    assert_eq!(
        lines[4], "        -> Table scan on t2  (cost=0.55 rows=3)",
        "{text}"
    );
    let text = last(
        &mut session,
        "EXPLAIN ANALYZE SELECT a FROM t1 WHERE a > (SELECT min(c1) FROM t2)",
    );
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[1].starts_with("-> Filter: (t1.a > (select #2))  (cost=0.55 rows=1) (actual time="),
        "{text}"
    );
    assert_eq!(
        lines[2], "    -> Table scan on t1  (cost=0.55 rows=3)",
        "{text}"
    );
    assert_eq!(
        lines[3], "    -> Select #2 (subquery in condition; run only once)",
        "{text}"
    );
    assert!(
        lines[4].starts_with("        -> Aggregate: min(t2.c1)  (cost=0.85 rows=1) (actual time="),
        "{text}"
    );
    assert!(lines[4].ends_with(" rows=1 loops=1)"), "{text}");
    assert_eq!(
        lines[5], "            -> Table scan on t2  (cost=0.55 rows=3)",
        "{text}"
    );
}

fn json_of(session: &mut Session<'_>, sql: &str) -> String {
    let text = last(session, sql);
    text.strip_prefix("EXPLAIN\n")
        .and_then(|rest| rest.strip_suffix("\nSELECT 1"))
        .unwrap_or_else(|| panic!("{sql}: {text}"))
        .to_owned()
}

#[test]
fn mysql_format_json_answers_mysql_8s_query_block() {
    let store = MemoryStore::default();
    let mut session = mysql_tables(&store);
    assert_eq!(
        json_of(
            &mut session,
            "EXPLAIN FORMAT=JSON SELECT * FROM t1 WHERE a > 1"
        ),
        r#"{
  "query_block": {
    "select_id": 1,
    "cost_info": {
      "query_cost": "0.55"
    },
    "table": {
      "table_name": "t1",
      "access_type": "ALL",
      "rows_examined_per_scan": 3,
      "rows_produced_per_join": 1,
      "filtered": "33.33",
      "cost_info": {
        "read_cost": "0.45",
        "eval_cost": "0.10",
        "prefix_cost": "0.55"
      },
      "attached_condition": "(t1.a > 1)"
    }
  }
}"#
    );
    assert_eq!(
        json_of(&mut session, "EXPLAIN FORMAT = JSON SELECT * FROM t1 WHERE id = 2"),
        r#"{
  "query_block": {
    "select_id": 1,
    "cost_info": {
      "query_cost": "0.35"
    },
    "table": {
      "table_name": "t1",
      "access_type": "const",
      "possible_keys": [
        "PRIMARY"
      ],
      "key": "PRIMARY",
      "used_key_parts": [
        "id"
      ],
      "ref": [
        "const"
      ],
      "rows_examined_per_scan": 1,
      "rows_produced_per_join": 1,
      "filtered": "100.00",
      "cost_info": {
        "read_cost": "0.25",
        "eval_cost": "0.10",
        "prefix_cost": "0.35"
      }
    }
  }
}"#
    );
    assert_eq!(
        json_of(&mut session, "EXPLAIN FORMAT=JSON SELECT 1"),
        "{\n  \"query_block\": {\n    \"select_id\": 1,\n    \"message\": \"No tables used\"\n  }\n}"
    );
}

#[test]
fn mysql_format_json_nests_joins_ordering_and_grouping() {
    let store = MemoryStore::default();
    let mut session = mysql_tables(&store);
    let json = json_of(
        &mut session,
        "EXPLAIN FORMAT=JSON SELECT t2.c2, count(*) FROM t2 JOIN t1 ON t1.id = t2.c1 GROUP BY t2.c2 ORDER BY t2.c2",
    );
    let ordering = json.find("\"ordering_operation\": {").unwrap_or(usize::MAX);
    let grouping = json.find("\"grouping_operation\": {").unwrap_or(usize::MAX);
    let nested = json.find("\"nested_loop\": [").unwrap_or(usize::MAX);
    assert!(ordering < grouping && grouping < nested && nested < json.len(), "{json}");
    assert!(json.contains("\"using_filesort\": true"), "{json}");
    assert!(json.contains("\"using_temporary_table\": true"), "{json}");
    assert!(json.contains("\"access_type\": \"eq_ref\""), "{json}");
    assert!(json.contains("\"prefix_cost\": \"1.60\""), "{json}");
    let json = json_of(
        &mut session,
        "EXPLAIN FORMAT=JSON SELECT * FROM t1 JOIN t2 ON t1.a = t2.c2",
    );
    assert!(
        json.contains("\"using_join_buffer\": \"hash join\""),
        "{json}"
    );
    assert!(
        json.contains("\"attached_condition\": \"(t1.a = t2.c2)\""),
        "{json}"
    );
}

#[test]
fn mysql_explain_of_writes_marks_the_written_table_and_runs_nothing() {
    let store = MemoryStore::default();
    let mut session = mysql_tables(&store);
    assert_eq!(
        last(
            &mut session,
            "EXPLAIN FORMAT=TREE UPDATE t1 SET b = 0 WHERE id = 1"
        ),
        "EXPLAIN\n-> <not executable by iterator executor>\n\nSELECT 1"
    );
    let json = json_of(
        &mut session,
        "EXPLAIN FORMAT=JSON DELETE FROM t1 WHERE a > 1",
    );
    assert!(json.contains("\"delete\": true,\n      \"table_name\": \"t1\""), "{json}");
    assert!(!json.contains("cost_info"), "{json}");
    let text = last(
        &mut session,
        "EXPLAIN FORMAT=TREE UPDATE t1 JOIN t2 ON t1.id = t2.c1 SET t1.b = 0",
    );
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[1], "-> Update t1 (buffered)", "{text}");
    assert!(lines[2].starts_with("    -> "), "{text}");
    let json = json_of(
        &mut session,
        "EXPLAIN FORMAT=JSON DELETE t1, t2 FROM t1 JOIN t2 ON t1.id = t2.c1",
    );
    assert_eq!(json.matches("\"delete\": true").count(), 2, "{json}");
    assert!(json.contains("\"query_cost\""), "{json}");
    assert!(
        last(&mut session, "SELECT b FROM t1 ORDER BY id").ends_with("\n10\n20\n30\nSELECT 3")
    );
}

#[test]
fn mysql_explain_analyze_runs_a_multi_table_update_and_delete() {
    let store = MemoryStore::default();
    let mut session = mysql_tables(&store);
    let text = last(
        &mut session,
        "EXPLAIN ANALYZE UPDATE t1 JOIN t2 ON t1.id = t2.c1 SET t1.b = 0",
    );
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[1].starts_with("-> Update t1 (buffered)  (actual time="),
        "{text}"
    );
    assert!(lines[1].ends_with(" rows=0 loops=1)"), "{text}");
    assert!(lines[2].starts_with("    -> "), "{text}");
    assert!(
        last(&mut session, "SELECT b FROM t1 ORDER BY id").ends_with("\n0\n0\n30\nSELECT 3")
    );
    let text = last(
        &mut session,
        "EXPLAIN ANALYZE DELETE t1 FROM t1 JOIN t2 ON t1.id = t2.c1 WHERE t2.c2 = 2",
    );
    assert!(
        text.lines()
            .nth(1)
            .is_some_and(|line| line.starts_with("-> Delete from t1 (buffered)  (actual time=")),
        "{text}"
    );
    assert!(last(&mut session, "SELECT count(*) FROM t1").ends_with("\n2\nSELECT 1"));
}

#[test]
fn mysql_explain_and_explain_analyze_accept_table_statements() {
    let store = MemoryStore::default();
    let mut session = mysql_tables(&store);
    assert_eq!(
        last(&mut session, "EXPLAIN FORMAT=TREE TABLE t1 ORDER BY b LIMIT 2"),
        last(
            &mut session,
            "EXPLAIN FORMAT=TREE SELECT * FROM t1 ORDER BY b LIMIT 2"
        )
    );
    let text = last(&mut session, "EXPLAIN ANALYZE TABLE t1");
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[1].starts_with("-> Table scan on t1  (cost=0.55 rows=3) (actual time="),
        "{text}"
    );
    assert!(lines[1].ends_with(" rows=3 loops=1)"), "{text}");
    assert!(
        last(&mut session, "EXPLAIN TABLE t1").contains("|SIMPLE|t1|NULL|ALL|"),
        "EXPLAIN TABLE t1"
    );
    let json = json_of(&mut session, "EXPLAIN FORMAT=JSON TABLE t1");
    assert!(json.contains("\"table_name\": \"t1\""), "{json}");
}

#[test]
fn mysql_refuses_explain_analyze_of_single_table_writes_and_other_formats() {
    let store = MemoryStore::default();
    let mut session = mysql_tables(&store);
    for sql in [
        "EXPLAIN ANALYZE DELETE FROM t1 WHERE id = 1",
        "EXPLAIN ANALYZE UPDATE t1 SET b = 0",
    ] {
        match run(&mut session, sql) {
            Err(error) => {
                assert_eq!(error.vendor_code(), Some(1235), "{sql}: {error}");
                assert!(
                    error
                        .message()
                        .contains("doesn't yet support 'EXPLAIN ANALYZE for this statement'"),
                    "{sql}: {error}"
                );
            }
            Ok(outputs) => panic!("{sql}: {outputs:?}"),
        }
    }
    for sql in [
        "EXPLAIN ANALYZE FORMAT=JSON SELECT * FROM t1",
        "EXPLAIN ANALYZE FORMAT=TRADITIONAL SELECT * FROM t1",
        "EXPLAIN FORMAT=XML SELECT * FROM t1",
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

#[test]
fn postgresql_explains_a_table_statement_as_its_select() {
    let store = MemoryStore::default();
    let mut session = postgresql_tables(&store);
    assert_eq!(plan(&mut session, "EXPLAIN TABLE onek"), ["Seq Scan on onek", "EXPLAIN"]);
}
