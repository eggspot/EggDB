use crate::error::SqlState;
use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs.pop().unwrap_or_default())
}

fn answer(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(output) => output,
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn failure(session: &mut Session<'_>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error,
    }
}

fn with_measurements(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::new(store);
    answer(
        &mut session,
        "CREATE TABLE m (id integer NOT NULL, at date NOT NULL, v integer, PRIMARY KEY (id, at))
             PARTITION BY RANGE (at);
         CREATE TABLE m_2024 PARTITION OF m FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
         CREATE TABLE m_2025 PARTITION OF m FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
         INSERT INTO m VALUES (1, '2024-03-01', 10), (2, '2025-03-01', 20), (3, '2024-12-31', 30)",
    );
    session
}

#[test]
fn range_partitions_take_their_rows_and_the_parent_reads_them_all() {
    let store = MemoryStore::default();
    let mut session = with_measurements(&store);
    assert_eq!(
        answer(&mut session, "SELECT id FROM m_2024 ORDER BY id"),
        "id\n1\n3\nSELECT 2"
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM m_2025"),
        "id\n2\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT id, v FROM m ORDER BY id"),
        "id|v\n1|10\n2|20\n3|30\nSELECT 3"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT count(*) FROM m WHERE at >= '2025-01-01'"
        ),
        "count\n1\nSELECT 1"
    );
}

#[test]
fn a_row_without_a_partition_is_refused() {
    let store = MemoryStore::default();
    let mut session = with_measurements(&store);
    let error = failure(&mut session, "INSERT INTO m VALUES (9, '2030-01-01', 1)");
    assert_eq!(error.state().code(), "23514");
    assert!(
        error
            .message()
            .contains("no partition of relation \"m\" found for row"),
        "{error}"
    );
    let error = failure(
        &mut session,
        "INSERT INTO m_2024 VALUES (9, '2025-06-01', 1)",
    );
    assert!(
        error.message().contains("violates partition constraint"),
        "{error}"
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM m"),
        "count\n3\nSELECT 1"
    );
}

#[test]
fn a_where_on_the_key_reads_only_the_partitions_it_can_match() {
    let store = MemoryStore::default();
    let mut session = with_measurements(&store);
    let plan = answer(
        &mut session,
        "EXPLAIN SELECT * FROM m WHERE at = '2025-03-01'",
    );
    assert!(plan.contains("m_2025"), "{plan}");
    assert!(!plan.contains("m_2024"), "{plan}");
    let plan = answer(
        &mut session,
        "EXPLAIN SELECT * FROM m WHERE at BETWEEN '2024-02-01' AND '2025-02-01'",
    );
    assert!(plan.contains("m_2024") && plan.contains("m_2025"), "{plan}");
    let plan = answer(
        &mut session,
        "EXPLAIN SELECT * FROM m AS x WHERE x.at < '2024-06-01'",
    );
    assert!(
        plan.contains("m_2024") && !plan.contains("m_2025"),
        "{plan}"
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM m WHERE at = '2025-03-01'"),
        "id\n2\nSELECT 1"
    );
}

#[test]
fn updates_and_deletes_through_the_parent_reach_every_partition() {
    let store = MemoryStore::default();
    let mut session = with_measurements(&store);
    assert_eq!(answer(&mut session, "UPDATE m SET v = v + 1"), "UPDATE 3");
    assert_eq!(
        answer(&mut session, "DELETE FROM m WHERE at < '2025-01-01'"),
        "DELETE 2"
    );
    assert_eq!(
        answer(&mut session, "SELECT id, v FROM m"),
        "id|v\n2|21\nSELECT 1"
    );
    let error = failure(
        &mut session,
        "UPDATE m_2025 SET at = '2024-01-05' WHERE id = 2",
    );
    assert!(error.message().contains("partition constraint"), "{error}");
    answer(&mut session, "TRUNCATE m");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM m"),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn list_hash_and_default_partitions_route_rows() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE c (id integer, region text) PARTITION BY LIST (region);
         CREATE TABLE c_eu PARTITION OF c FOR VALUES IN ('de', 'fr');
         CREATE TABLE c_us PARTITION OF c FOR VALUES IN ('us');
         CREATE TABLE c_rest PARTITION OF c DEFAULT;
         INSERT INTO c VALUES (1, 'de'), (2, 'us'), (3, 'jp'), (4, NULL)",
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM c_eu"),
        "id\n1\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM c_rest ORDER BY id"),
        "id\n3\n4\nSELECT 2"
    );
    let error = failure(
        &mut session,
        "CREATE TABLE c_x PARTITION OF c FOR VALUES IN ('us')",
    );
    assert!(error.message().contains("would overlap"), "{error}");
    let error = failure(
        &mut session,
        "CREATE TABLE c_jp PARTITION OF c FOR VALUES IN ('jp')",
    );
    assert!(error.message().contains("violated by some row"), "{error}");
    answer(
        &mut session,
        "CREATE TABLE h (id integer, v text) PARTITION BY HASH (id);
         CREATE TABLE h0 PARTITION OF h FOR VALUES WITH (MODULUS 2, REMAINDER 0);
         CREATE TABLE h1 PARTITION OF h FOR VALUES WITH (MODULUS 2, REMAINDER 1)",
    );
    for id in 0..40 {
        answer(&mut session, &format!("INSERT INTO h VALUES ({id}, 'x')"));
    }
    let spread = answer(
        &mut session,
        "SELECT (SELECT count(*) FROM h0) > 0 AND (SELECT count(*) FROM h1) > 0 AS spread",
    );
    assert!(spread.contains('t'), "{spread}");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM h"),
        "count\n40\nSELECT 1"
    );
    let plan = answer(&mut session, "EXPLAIN SELECT * FROM h WHERE id = 7");
    assert!(plan.contains("h0") != plan.contains("h1"), "{plan}");
}

#[test]
fn attach_detach_and_drop_keep_the_tree_consistent() {
    let store = MemoryStore::default();
    let mut session = with_measurements(&store);
    answer(
        &mut session,
        "CREATE TABLE m_2026 (id integer NOT NULL, at date NOT NULL, v integer);
         INSERT INTO m_2026 VALUES (7, '2026-05-05', 70);
         ALTER TABLE m ATTACH PARTITION m_2026 FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')",
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM m"),
        "count\n4\nSELECT 1"
    );
    answer(&mut session, "ALTER TABLE m DETACH PARTITION m_2026");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM m"),
        "count\n3\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM m_2026"),
        "count\n1\nSELECT 1"
    );
    let error = failure(
        &mut session,
        "ALTER TABLE m ATTACH PARTITION m_2026 FOR VALUES FROM ('2024-06-01') TO ('2024-07-01')",
    );
    assert!(error.message().contains("would overlap"), "{error}");
    answer(&mut session, "DROP TABLE m");
    assert!(
        failure(&mut session, "SELECT * FROM m_2024")
            .message()
            .contains("m_2024")
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM m_2026"),
        "count\n1\nSELECT 1"
    );
}

#[test]
fn the_catalogs_describe_partitioned_tables_and_partitions() {
    let store = MemoryStore::default();
    let mut session = with_measurements(&store);
    assert_eq!(
        answer(
            &mut session,
            "SELECT relname, relkind, relispartition FROM pg_class
              WHERE relname IN ('m', 'm_2024') ORDER BY relname"
        ),
        "relname|relkind|relispartition\nm|p|f\nm_2024|r|t\nSELECT 2"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT pg_get_partkeydef('m'::regclass), pg_get_expr(c.relpartbound, c.oid)
               FROM pg_class c WHERE c.relname = 'm_2024'"
        ),
        "pg_get_partkeydef|pg_get_expr\n\
         RANGE (at)|FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')\nSELECT 1"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT count(*) FROM pg_inherits WHERE inhparent = 'm'::regclass"
        ),
        "count\n2\nSELECT 1"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT partstrat, partnatts FROM pg_partitioned_table"
        ),
        "partstrat|partnatts\nr|1\nSELECT 1"
    );
}

#[test]
fn mysql_and_oracle_name_their_partitions_inside_create_table() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    answer(
        &mut session,
        "CREATE TABLE o (id int NOT NULL, y int NOT NULL, PRIMARY KEY (id, y))
             PARTITION BY RANGE (y) (
               PARTITION p0 VALUES LESS THAN (2000),
               PARTITION p1 VALUES LESS THAN (2020),
               PARTITION p2 VALUES LESS THAN MAXVALUE
             );
         INSERT INTO o VALUES (1, 1999), (2, 2010), (3, 2030)",
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM o WHERE y >= 2000 ORDER BY id"),
        "id\n2\n3\nSELECT 2"
    );
    answer(
        &mut session,
        "CREATE TABLE k (id int NOT NULL, PRIMARY KEY (id)) PARTITION BY KEY (id) PARTITIONS 4;
         INSERT INTO k VALUES (1), (2), (3), (4), (5)",
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM k"),
        "count(*)\n5\nSELECT 1"
    );
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    answer(
        &mut session,
        "CREATE TABLE sales (id NUMBER, region VARCHAR2(10))
             PARTITION BY LIST (region) (
               PARTITION east VALUES ('E'),
               PARTITION west VALUES ('W'),
               PARTITION other VALUES (DEFAULT)
             )",
    );
    answer(&mut session, "INSERT INTO sales VALUES (1, 'E')");
    answer(&mut session, "INSERT INTO sales VALUES (2, 'N')");
    assert!(
        answer(&mut session, "SELECT COUNT(*) FROM sales").contains('2'),
        "both rows are read through the table"
    );
}

#[test]
fn hostile_partition_statements_are_refused_with_errors() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "CREATE TABLE a (x int) PARTITION BY RANGE (missing)",
        "CREATE TABLE a (x int, y int) PARTITION BY LIST (x, y)",
        "CREATE TABLE a (x int PRIMARY KEY, y int) PARTITION BY RANGE (y)",
        "CREATE TABLE b PARTITION OF nowhere FOR VALUES IN (1)",
        "CREATE TABLE a (x int) PARTITION BY RANGE ((1))",
        "CREATE TABLE a (x int) PARTITION BY RANGE ((x + random()::int))",
        "CREATE TABLE a (x int) PARTITION BY RANGE ((x + (SELECT 1)))",
        "CREATE TABLE a (x int) PARTITION BY RANGE ((sum(x)))",
        "CREATE TABLE a (x int PRIMARY KEY) PARTITION BY RANGE ((x + 1))",
        "CREATE TABLE a (x int) PARTITION BY RANGE ((y + 1))",
        "CREATE TABLE a (x int) PARTITION BY HASH (x) PARTITIONS 0",
    ] {
        failure(&mut session, sql);
    }
    answer(
        &mut session,
        "CREATE TABLE r (x int) PARTITION BY RANGE (x)",
    );
    for sql in [
        "CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (10) TO (5)",
        "CREATE TABLE r1 PARTITION OF r FOR VALUES FROM (NULL) TO (5)",
        "CREATE TABLE r1 PARTITION OF r FOR VALUES IN (1)",
        "CREATE TABLE r1 PARTITION OF r FOR VALUES WITH (MODULUS 2, REMAINDER 2)",
        "ALTER TABLE r ATTACH PARTITION r FOR VALUES FROM (1) TO (2)",
    ] {
        failure(&mut session, sql);
    }
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM r"),
        "count\n0\nSELECT 1"
    );
}

#[test]
fn analyze_records_row_counts_and_approx_count_distinct_estimates() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(&mut session, "CREATE TABLE a (v integer)");
    for value in 0..500 {
        answer(
            &mut session,
            &format!("INSERT INTO a VALUES ({})", value % 200),
        );
    }
    answer(&mut session, "ANALYZE a");
    answer(&mut session, "ANALYZE");
    assert_eq!(
        answer(
            &mut session,
            "SELECT reltuples FROM pg_class WHERE relname = 'a'"
        ),
        "reltuples\n500\nSELECT 1"
    );
    let estimate = answer(&mut session, "SELECT approx_count_distinct(v) FROM a");
    let number: i64 = estimate.lines().nth(1).unwrap().parse().unwrap();
    assert!((190..=210).contains(&number), "{estimate}");
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    assert!(answer(&mut mysql, "ANALYZE TABLE a").contains("OK"));
    let mut oracle = Session::with_dialect(&store, Dialect::Oracle);
    answer(&mut oracle, "ANALYZE TABLE a COMPUTE STATISTICS");
}

#[test]
fn batched_aggregates_give_the_row_at_a_time_results() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE n (id integer PRIMARY KEY, v bigint, s smallint)",
    );
    let mut values = Vec::new();
    for id in 0..3000i64 {
        let v = match id % 7 {
            0 => None,
            1 => Some(i64::MAX / 4),
            2 => Some(-(id * 997)),
            _ => Some(id),
        };
        values.push(v);
        let shown = v.map_or("NULL".to_owned(), |v| v.to_string());
        answer(
            &mut session,
            &format!("INSERT INTO n VALUES ({id}, {shown}, {})", id % 100),
        );
    }
    let present: Vec<i128> = values.iter().flatten().map(|&v| i128::from(v)).collect();
    let expected = format!(
        "count|count|sum|min|max|min\n3000|{}|{}|{}|{}|0\nSELECT 1",
        present.len(),
        present.iter().sum::<i128>(),
        present.iter().min().unwrap(),
        present.iter().max().unwrap()
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT count(*), count(v), sum(v), min(v), max(v), min(s) FROM n"
        ),
        expected
    );
    assert_eq!(
        answer(&mut session, "SELECT sum(v), max(s) FROM n WHERE id < 0"),
        "sum|max\nNULL|NULL\nSELECT 1"
    );
}

#[test]
fn an_update_through_the_parent_moves_rows_to_the_partition_that_takes_them() {
    let store = MemoryStore::default();
    let mut session = with_measurements(&store);
    answer(&mut session, "CREATE INDEX m_v_idx ON m (v)");
    assert_eq!(
        answer(&mut session, "UPDATE m SET at = '2025-06-01' WHERE id = 1"),
        "UPDATE 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM m_2025 ORDER BY id"),
        "id\n1\n2\nSELECT 2"
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM m_2024"),
        "id\n3\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT id, at FROM m WHERE v = 10"),
        "id|at\n1|2025-06-01\nSELECT 1"
    );
    assert_eq!(
        answer(
            &mut session,
            "UPDATE m SET at = '2024-05-05', v = v + 1 WHERE id = 2 RETURNING id, at, v"
        ),
        "id|at|v\n2|2024-05-05|21\nUPDATE 1"
    );
    assert_eq!(answer(&mut session, "UPDATE m SET v = v + 100"), "UPDATE 3");
    let error = failure(&mut session, "UPDATE m SET at = '2030-01-01' WHERE id = 3");
    assert_eq!(error.state(), SqlState::CHECK_VIOLATION, "{error}");
    assert!(
        error
            .message()
            .contains("no partition of relation \"m\" found for row"),
        "{error}"
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM m_2024 ORDER BY id"),
        "id\n2\n3\nSELECT 2"
    );
    let error = failure(
        &mut session,
        "UPDATE m_2025 SET at = '2024-02-01' WHERE id = 1",
    );
    assert!(error.message().contains("partition constraint"), "{error}");
}

#[test]
fn rows_move_between_sub_partitions_and_in_mysql() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE t (k int, s text) PARTITION BY LIST (k);
         CREATE TABLE t1 PARTITION OF t FOR VALUES IN (1) PARTITION BY LIST (s);
         CREATE TABLE t1a PARTITION OF t1 FOR VALUES IN ('a');
         CREATE TABLE t1b PARTITION OF t1 FOR VALUES IN ('b');
         CREATE TABLE t2 PARTITION OF t FOR VALUES IN (2);
         INSERT INTO t VALUES (1, 'a'), (2, 'b')",
    );
    assert_eq!(
        answer(&mut session, "UPDATE t SET s = 'b' WHERE k = 1"),
        "UPDATE 1"
    );
    assert_eq!(answer(&mut session, "SELECT k FROM t1b"), "k\n1\nSELECT 1");
    let error = failure(&mut session, "UPDATE t1 SET k = 2");
    assert!(error.message().contains("partition constraint"), "{error}");
    assert_eq!(
        answer(&mut session, "UPDATE t SET k = 2 WHERE k = 1"),
        "UPDATE 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM t2"),
        "count\n2\nSELECT 1"
    );
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    answer(
        &mut session,
        "CREATE TABLE o (id int NOT NULL, y int NOT NULL, PRIMARY KEY (id, y))
             PARTITION BY RANGE (y) (
               PARTITION p0 VALUES LESS THAN (2000),
               PARTITION p1 VALUES LESS THAN (2020)
             );
         INSERT INTO o VALUES (1, 1999), (2, 2010)",
    );
    answer(&mut session, "UPDATE o SET y = 2015 WHERE id = 1");
    let mut reader = Session::new(&store);
    assert_eq!(
        answer(&mut reader, "SELECT id FROM \"o#P#p1\" ORDER BY id"),
        "id\n1\n2\nSELECT 2"
    );
    let error = failure(&mut session, "UPDATE o SET y = 2030 WHERE id = 1");
    assert!(
        error.message().contains("Table has no partition for value"),
        "{error}"
    );
}

#[test]
fn expressions_in_partition_keys_route_prune_and_move_rows() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE people (name text NOT NULL, born date) PARTITION BY LIST (lower(name));
         CREATE TABLE people_a PARTITION OF people FOR VALUES IN ('ann', 'amy');
         CREATE TABLE people_rest PARTITION OF people DEFAULT;
         INSERT INTO people VALUES ('Ann', '2000-01-01'), ('BOB', '1990-05-05')",
    );
    assert_eq!(
        answer(&mut session, "SELECT name FROM people_a"),
        "name\nAnn\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT pg_get_partkeydef('people'::regclass)"),
        "pg_get_partkeydef\nLIST (lower(name))\nSELECT 1"
    );
    let plan = answer(
        &mut session,
        "EXPLAIN SELECT * FROM people WHERE lower(name) = 'amy'",
    );
    assert!(
        plan.contains("people_a") && !plan.contains("people_rest"),
        "{plan}"
    );
    assert_eq!(
        answer(
            &mut session,
            "UPDATE people SET name = 'Amy' WHERE name = 'BOB'"
        ),
        "UPDATE 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT name FROM people_a ORDER BY name"),
        "name\nAmy\nAnn\nSELECT 2"
    );
    let error = failure(&mut session, "INSERT INTO people_a VALUES ('Zed', NULL)");
    assert!(error.message().contains("partition constraint"), "{error}");
    answer(
        &mut session,
        "CREATE TABLE ev (id int NOT NULL, note text) PARTITION BY RANGE ((id / 100));
         CREATE TABLE ev_0 PARTITION OF ev FOR VALUES FROM (0) TO (10);
         CREATE TABLE ev_1 PARTITION OF ev FOR VALUES FROM (10) TO (20);
         INSERT INTO ev VALUES (5, 'a'), (950, 'b'), (1500, 'c')",
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM ev_1"),
        "id\n1500\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT pg_get_partkeydef('ev'::regclass)"),
        "pg_get_partkeydef\nRANGE ((id / 100))\nSELECT 1"
    );
    let error = failure(&mut session, "INSERT INTO ev VALUES (2500, 'd')");
    assert_eq!(error.state(), SqlState::CHECK_VIOLATION, "{error}");
    assert!(
        error
            .detail()
            .unwrap_or_default()
            .contains("((id / 100)) = (25)"),
        "{error}"
    );
    answer(
        &mut session,
        "CREATE TABLE h (id int NOT NULL) PARTITION BY HASH ((id + 1));
         CREATE TABLE h0 PARTITION OF h FOR VALUES WITH (MODULUS 2, REMAINDER 0);
         CREATE TABLE h1 PARTITION OF h FOR VALUES WITH (MODULUS 2, REMAINDER 1);
         INSERT INTO h VALUES (1), (2), (3), (4), (5), (6), (7), (8), (9), (10)",
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT (SELECT count(*) FROM h0) + (SELECT count(*) FROM h1) AS total"
        ),
        "total\n10\nSELECT 1"
    );
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    answer(
        &mut session,
        "CREATE TABLE sales (id int NOT NULL, d date NOT NULL)
             PARTITION BY RANGE (EXTRACT(YEAR FROM d)) (
               PARTITION p_old VALUES LESS THAN (2000),
               PARTITION p_new VALUES LESS THAN MAXVALUE
             );
         INSERT INTO sales VALUES (1, '1999-12-31'), (2, '2024-06-01')",
    );
    assert_eq!(
        answer(
            &mut Session::new(&store),
            "SELECT id FROM \"sales#P#p_old\""
        ),
        "id\n1\nSELECT 1"
    );
}

fn relation_names(session: &mut Session<'_>, pattern: &str) -> String {
    answer(
        session,
        &format!("SELECT relname FROM pg_class WHERE relname LIKE '{pattern}' ORDER BY relname"),
    )
}

#[test]
fn an_index_on_the_parent_reaches_every_partition_now_and_later() {
    let store = MemoryStore::default();
    let mut session = with_measurements(&store);
    answer(&mut session, "CREATE INDEX m_v_idx ON m (v)");
    assert_eq!(
        relation_names(&mut session, "%v_idx"),
        "relname\nm_2024_v_idx\nm_2025_v_idx\nm_v_idx\nSELECT 3"
    );
    answer(
        &mut session,
        "CREATE TABLE m_2026 PARTITION OF m FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
         CREATE TABLE m_old (id integer NOT NULL, at date NOT NULL, v integer, PRIMARY KEY (id, at));
         CREATE INDEX m_old_v ON m_old (v);
         ALTER TABLE m ATTACH PARTITION m_old FOR VALUES FROM ('2000-01-01') TO ('2024-01-01')",
    );
    assert_eq!(
        relation_names(&mut session, "m%v%"),
        "relname\nm_2024_v_idx\nm_2025_v_idx\nm_2026_v_idx\nm_old_v\nm_v_idx\nSELECT 5"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT count(*) FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhparent
              WHERE c.relname = 'm_v_idx'"
        ),
        "count\n4\nSELECT 1"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT relkind FROM pg_class WHERE relname = 'm_v_idx'"
        ),
        "relkind\nI\nSELECT 1"
    );
    let error = failure(&mut session, "DROP INDEX m_2024_v_idx");
    assert_eq!(
        error.state(),
        SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
        "{error}"
    );
    assert!(
        error
            .message()
            .contains("cannot drop index m_2024_v_idx because index m_v_idx requires it"),
        "{error}"
    );
    answer(&mut session, "DROP INDEX m_v_idx");
    assert_eq!(relation_names(&mut session, "m%v%"), "relname\nSELECT 0");
    let error = failure(&mut session, "CREATE UNIQUE INDEX m_v_key ON m (v)");
    assert!(
        error
            .message()
            .contains("must include all partitioning columns"),
        "{error}"
    );
    answer(&mut session, "CREATE UNIQUE INDEX m_id_at ON m (id, at)");
    assert_eq!(
        relation_names(&mut session, "m%id_at%"),
        "relname\nm_2024_id_at_idx\nm_2025_id_at_idx\nm_2026_id_at_idx\nm_id_at\nm_old_id_at_idx\nSELECT 5"
    );
}

#[test]
fn an_index_made_on_only_the_parent_is_valid_once_every_partition_is_attached() {
    let store = MemoryStore::default();
    let mut session = with_measurements(&store);
    let validity = "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
                     WHERE c.relname = 'm_w'";
    answer(&mut session, "CREATE INDEX m_w ON ONLY m (v)");
    assert_eq!(
        relation_names(&mut session, "m%w"),
        "relname\nm_w\nSELECT 1"
    );
    assert_eq!(answer(&mut session, validity), "indisvalid\nf\nSELECT 1");
    answer(
        &mut session,
        "CREATE INDEX m_2024_w ON m_2024 (v);
         CREATE INDEX m_2025_id ON m_2025 (id);
         ALTER INDEX m_w ATTACH PARTITION m_2024_w;
         ALTER INDEX m_w ATTACH PARTITION m_2024_w",
    );
    assert_eq!(answer(&mut session, validity), "indisvalid\nf\nSELECT 1");
    let error = failure(&mut session, "ALTER INDEX m_w ATTACH PARTITION m_2025_id");
    assert_eq!(
        error.state(),
        SqlState::INVALID_OBJECT_DEFINITION,
        "{error}"
    );
    assert_eq!(
        error.detail().unwrap_or_default(),
        "The index definitions do not match."
    );
    answer(
        &mut session,
        "CREATE INDEX m_2025_w ON m_2025 (v);
         ALTER INDEX m_w ATTACH PARTITION m_2025_w",
    );
    assert_eq!(answer(&mut session, validity), "indisvalid\nt\nSELECT 1");
    let error = failure(
        &mut session,
        "ALTER INDEX m_2024_w ATTACH PARTITION m_2025_w",
    );
    assert_eq!(error.state(), SqlState::WRONG_OBJECT_TYPE, "{error}");
    answer(
        &mut session,
        "ALTER INDEX IF EXISTS nowhere ATTACH PARTITION m_2025_w",
    );
    let error = failure(&mut session, "DROP INDEX m_2025_w");
    assert_eq!(
        error.state(),
        SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
        "{error}"
    );
}

#[test]
fn partition_records_with_expressions_and_index_links_read_back() {
    use crate::partitions::{
        BoundText, IndexLink, KeyPart, PartitionBound, PartitionKey, PartitionRecord,
        PartitionStrategy,
    };
    let linked = PartitionRecord {
        key: Some(PartitionKey {
            strategy: PartitionStrategy::List,
            parts: vec![KeyPart::Expression {
                text: "lower(name)".to_owned(),
                dialect: Dialect::PostgreSql,
            }],
        }),
        parent: Some((
            "people".to_owned(),
            PartitionBound::List(vec![BoundText::Value("ann".to_owned())]),
        )),
        index_links: vec![IndexLink {
            index: 2,
            parent_index: 1,
        }],
    };
    let bytes = crate::encoding::encode_partitioning(&linked);
    assert_eq!(bytes.first(), Some(&2));
    assert_eq!(
        crate::encoding::decode_partitioning("people_a", &bytes).ok(),
        Some(linked)
    );
    for cut in 0..bytes.len() {
        assert!(crate::encoding::decode_partitioning("people_a", &bytes[..cut]).is_err());
    }
    let plain = PartitionRecord {
        key: Some(PartitionKey {
            strategy: PartitionStrategy::Range,
            parts: vec![KeyPart::Column("at".to_owned())],
        }),
        parent: None,
        index_links: Vec::new(),
    };
    let bytes = crate::encoding::encode_partitioning(&plain);
    assert_eq!(bytes.first(), Some(&1));
    assert_eq!(
        crate::encoding::decode_partitioning("m", &bytes).ok(),
        Some(plain)
    );
}
