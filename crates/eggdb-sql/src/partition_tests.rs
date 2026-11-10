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
        row_movement_disabled: false,
        interval: None,    };
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
        row_movement_disabled: false,
        interval: None,    };
    let bytes = crate::encoding::encode_partitioning(&plain);
    assert_eq!(bytes.first(), Some(&1));
    assert_eq!(
        crate::encoding::decode_partitioning("m", &bytes).ok(),
        Some(plain)
    );
}

#[test]
fn partition_keys_that_depend_on_the_session_settings_are_refused() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for key in [
        "RANGE ((t::date))",
        "RANGE ((date_trunc('day', t)))",
        "LIST ((to_char(t, 'YYYY')))",
        "RANGE ((extract(year FROM t)))",
        "RANGE ((date_part('year', t)))",
        "RANGE ((age(t)))",
        "LIST ((t::text))",
        "RANGE ((t + interval '1 day'))",
        "RANGE ((ts::timestamptz))",
        "RANGE ((s::date))",
        "LIST ((d::text))",
        "LIST ((concat(t)))",
        "LIST ((left(concat(t), 10)))",
        "LIST ((format('%s', t)))",
        "LIST ((to_jsonb(t)::text))",
        "LIST ((json_build_object('a', t)::text))",
        "RANGE ((make_timestamptz(2020, 1, 1, 0, 0, 0)))",
        "LIST ((s || t))",
        "LIST ((current_user || s))",
    ] {
        let error = failure(
            &mut session,
            &format!(
                "CREATE TABLE a (t timestamptz, ts timestamp, d date, s text) PARTITION BY {key}"
            ),
        );
        assert_eq!(error.state().code(), "42P17", "{key}: {error}");
        assert_eq!(
            error.message(),
            "functions in partition key expression must be marked IMMUTABLE",
            "{key}"
        );
    }
    for (name, key) in [
        ("b1", "RANGE ((date_trunc('day', ts)))"),
        ("b2", "RANGE ((ts::date))"),
        ("b3", "RANGE ((extract(year FROM d)))"),
        ("b4", "RANGE ((ts + interval '1 day'))"),
        ("b5", "LIST ((t AT TIME ZONE 'UTC'))"),
        ("b6", "LIST ((upper(s) || '-' || substr(s, 1, 2)))"),
        ("b7", "RANGE ((abs(extract(year FROM ts))))"),
    ] {
        answer(
            &mut session,
            &format!(
                "CREATE TABLE {name} (t timestamptz, ts timestamp, d date, s text) \
                 PARTITION BY {key}"
            ),
        );
    }
}

#[test]
fn an_unparenthesized_postgresql_key_expression_is_a_syntax_error() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let error = failure(
        &mut session,
        "CREATE TABLE a (x int, y int) PARTITION BY RANGE (x + y)",
    );
    assert_eq!(error.state().code(), "42601", "{error}");
    assert_eq!(error.message(), "syntax error at or near \"+\"");
    answer(
        &mut session,
        "CREATE TABLE b (x int, name text) PARTITION BY LIST (lower(name))",
    );
    let mut mysql = Session::with_dialect(&store, Dialect::MySql);
    answer(
        &mut mysql,
        "CREATE TABLE c (x int, y int) PARTITION BY HASH (x + y) PARTITIONS 2",
    );
}

#[test]
fn renaming_or_dropping_a_key_column_keeps_the_table_readable() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE e (id int, name text) PARTITION BY LIST (lower(name));
         CREATE TABLE e_a PARTITION OF e FOR VALUES IN ('ann');
         INSERT INTO e VALUES (1, 'Ann');
         ALTER TABLE e RENAME COLUMN name TO nm",
    );
    assert_eq!(
        answer(&mut session, "SELECT id, nm FROM e"),
        "id|nm\n1|Ann\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT pg_get_partkeydef('e'::regclass)"),
        "pg_get_partkeydef\nLIST (lower(nm))\nSELECT 1"
    );
    answer(&mut session, "INSERT INTO e VALUES (2, 'ANN')");
    assert_eq!(
        answer(&mut session, "SELECT id, nm FROM e_a ORDER BY id"),
        "id|nm\n1|Ann\n2|ANN\nSELECT 2"
    );
    let error = failure(&mut session, "ALTER TABLE e_a RENAME COLUMN nm TO other");
    assert_eq!(error.state().code(), "42P16", "{error}");
    assert_eq!(error.message(), "cannot rename inherited column \"nm\"");
    let error = failure(&mut session, "ALTER TABLE e DROP COLUMN nm");
    assert_eq!(error.state().code(), "42P16", "{error}");
    assert_eq!(
        error.message(),
        "cannot drop column \"nm\" because it is part of the partition key of relation \"e\""
    );
    answer(
        &mut session,
        "CREATE TABLE f (a int, b int, c int) PARTITION BY RANGE ((a / b));
         CREATE TABLE f_low PARTITION OF f FOR VALUES FROM (0) TO (10)
             PARTITION BY RANGE (c);
         CREATE TABLE f_low_c PARTITION OF f_low FOR VALUES FROM (0) TO (100);
         INSERT INTO f VALUES (4, 2, 7)",
    );
    let error = failure(&mut session, "ALTER TABLE f DROP COLUMN b");
    assert_eq!(
        error.message(),
        "cannot drop column \"b\" because it is part of the partition key of relation \"f\""
    );
    let error = failure(&mut session, "ALTER TABLE f DROP COLUMN c");
    assert_eq!(
        error.message(),
        "cannot drop column \"c\" because it is part of the partition key of relation \"f_low\""
    );
    answer(&mut session, "ALTER TABLE f RENAME COLUMN c TO kept");
    answer(&mut session, "INSERT INTO f VALUES (6, 3, 8)");
    assert_eq!(
        answer(&mut session, "SELECT a, b, kept FROM f ORDER BY a"),
        "a|b|kept\n4|2|7\n6|3|8\nSELECT 2"
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM f_low_c"),
        "count\n2\nSELECT 1"
    );
}

#[test]
fn an_index_adopted_on_a_sub_partitioned_partition_reaches_its_partitions() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE s (id int, at date, v int) PARTITION BY RANGE (at);
         CREATE TABLE s_2024 PARTITION OF s FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')
             PARTITION BY RANGE (id);
         CREATE TABLE s_2024_low PARTITION OF s_2024 FOR VALUES FROM (0) TO (100);
         CREATE INDEX s_2024_v ON ONLY s_2024 (v);
         CREATE INDEX s_v ON s (v)",
    );
    for index in ["s_v", "s_2024_v"] {
        assert_eq!(
            answer(
                &mut session,
                &format!(
                    "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
                      WHERE c.relname = '{index}'"
                )
            ),
            "indisvalid\nt\nSELECT 1",
            "{index}"
        );
    }
}

fn pinned_version_1_records() -> Vec<(&'static str, Vec<u8>, crate::partitions::PartitionRecord)> {
    use crate::partitions::{
        BoundText, KeyPart, PartitionBound, PartitionKey, PartitionRecord, PartitionStrategy,
    };
    vec![
        (
            "m",
            vec![1, 1, 1, 1, 2, b'a', b't'],
            PartitionRecord {
                key: Some(PartitionKey {
                    strategy: PartitionStrategy::Range,
                    parts: vec![KeyPart::Column("at".to_owned())],
                }),
                parent: None,
                index_links: Vec::new(),
                row_movement_disabled: false,
                interval: None,            },
        ),
        (
            "m_x",
            vec![1, 3, 3, 1, 2, b'i', b'd', 1, b'm', 2, 3, 0, 1, b'x', 1, 2],
            PartitionRecord {
                key: Some(PartitionKey {
                    strategy: PartitionStrategy::Hash,
                    parts: vec![KeyPart::Column("id".to_owned())],
                }),
                parent: Some((
                    "m".to_owned(),
                    PartitionBound::List(vec![
                        BoundText::Value("x".to_owned()),
                        BoundText::Null,
                        BoundText::MinValue,
                    ]),
                )),
                index_links: Vec::new(),
                row_movement_disabled: false,
                interval: None,            },
        ),
        (
            "m_r",
            vec![1, 2, 1, b'm', 1, 1, 0, 1, b'1', 1, 3],
            PartitionRecord {
                key: None,
                parent: Some((
                    "m".to_owned(),
                    PartitionBound::Range {
                        from: vec![BoundText::Value("1".to_owned())],
                        to: vec![BoundText::MaxValue],
                    },
                )),
                index_links: Vec::new(),
                row_movement_disabled: false,
                interval: None,            },
        ),
        (
            "m_h",
            vec![1, 2, 1, b'm', 3, 4, 1],
            PartitionRecord {
                key: None,
                parent: Some((
                    "m".to_owned(),
                    PartitionBound::Hash {
                        modulus: 4,
                        remainder: 1,
                    },
                )),
                index_links: Vec::new(),
                row_movement_disabled: false,
                interval: None,            },
        ),
        (
            "m_d",
            vec![1, 2, 1, b'm', 4],
            PartitionRecord {
                key: None,
                parent: Some(("m".to_owned(), PartitionBound::Default)),
                index_links: Vec::new(),
                row_movement_disabled: false,
                interval: None,            },
        ),
    ]
}

#[test]
fn version_1_partition_records_keep_their_bytes() {
    for (table, bytes, record) in pinned_version_1_records() {
        assert_eq!(
            crate::encoding::decode_partitioning(table, &bytes).ok(),
            Some(record.clone()),
            "{table}"
        );
        assert_eq!(
            crate::encoding::encode_partitioning(&record),
            bytes,
            "{table}"
        );
    }
}

fn version_2_record() -> Vec<u8> {
    vec![
        2, 3, 2, 1, 1, 0, 11, b'l', b'o', b'w', b'e', b'r', b'(', b'n', b'a', b'm', b'e', b')', 6,
        b'p', b'e', b'o', b'p', b'l', b'e', 2, 1, 0, 3, b'a', b'n', b'n', 2, 2, 1, 5, 3,
    ]
}

#[test]
fn a_partition_record_from_a_newer_or_hostile_writer_is_refused_without_panicking() {
    use crate::partitions::{
        BoundText, IndexLink, KeyPart, PartitionBound, PartitionKey, PartitionRecord,
        PartitionStrategy,
    };
    let valid = version_2_record();
    assert_eq!(
        crate::encoding::decode_partitioning("people_a", &valid).ok(),
        Some(PartitionRecord {
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
            index_links: vec![
                IndexLink {
                    index: 2,
                    parent_index: 1,
                },
                IndexLink {
                    index: 5,
                    parent_index: 3,
                },
            ],
            row_movement_disabled: false,
            interval: None,        })
    );
    let mut newer = valid.clone();
    newer[0] = 3;
    let error = crate::encoding::decode_partitioning("people_a", &newer).unwrap_err();
    assert_eq!(error.state(), SqlState::FEATURE_NOT_SUPPORTED, "{error}");
    assert!(error.message().contains("format version 3"), "{error}");
    let damaged = |bytes: &[u8]| {
        let error = crate::encoding::decode_partitioning("people_a", bytes).unwrap_err();
        assert_eq!(error.state().code(), "XX001", "{bytes:?}: {error}");
    };
    let mut unknown_part = valid.clone();
    unknown_part[4] = 2;
    damaged(&unknown_part);
    let mut unknown_dialect = valid.clone();
    unknown_dialect[5] = 9;
    damaged(&unknown_dialect);
    let mut repeated_index = valid.clone();
    let last = repeated_index.len() - 2;
    repeated_index[last] = 2;
    damaged(&repeated_index);
    let mut repeated_parent_index = valid.clone();
    let last = repeated_parent_index.len() - 1;
    repeated_parent_index[last] = 1;
    damaged(&repeated_parent_index);
    let mut too_many_links = valid[..valid.len() - 5].to_vec();
    too_many_links.extend_from_slice(&[0x81, 0x80, 0x04]);
    damaged(&too_many_links);
    for at in 0..valid.len() {
        for byte in [0, 1, 2, 3, 4, 9, 0x7F, 0x80, 0xFF] {
            let mut mutated = valid.clone();
            mutated[at] = byte;
            let _ = crate::encoding::decode_partitioning("people_a", &mutated);
        }
    }
    let mut random = 0x5EED_0316_u64;
    for _ in 0..20_000 {
        let mut bytes = vec![2];
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        bytes.extend(random.to_le_bytes().iter().take((random % 9) as usize));
        let _ = crate::encoding::decode_partitioning("people_a", &bytes);
    }
}

#[test]
fn unique_indexes_of_a_file_from_before_index_links_are_linked_after_the_upgrade() {
    use crate::store::{KeyValueStore, KeyValueWrite};
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE u (id int, at date, v int) PARTITION BY RANGE (at);
         CREATE TABLE u_1 PARTITION OF u FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
         CREATE TABLE u_2 PARTITION OF u FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
         ALTER TABLE u ADD CONSTRAINT u_key UNIQUE (id, at);
         INSERT INTO u VALUES (1, '2024-05-01', 1)",
    );
    drop(session);
    let mut writer = store.begin_write().unwrap();
    let catalog = crate::partitions::load(&writer).unwrap();
    let partitions: Vec<(String, crate::partitions::PartitionRecord)> = catalog
        .every()
        .filter(|(_, record)| record.parent.is_some())
        .map(|(name, record)| (name.to_owned(), record.clone()))
        .collect();
    assert_eq!(partitions.len(), 2);
    for (name, mut record) in partitions {
        assert_eq!(record.index_links.len(), 1, "{name}");
        record.index_links.clear();
        let bytes = crate::encoding::encode_partitioning(&record);
        assert_eq!(bytes.first(), Some(&1), "{name}");
        writer
            .put(&crate::encoding::partition_key(&name), &bytes)
            .unwrap();
    }
    writer.commit().unwrap();
    let mut session = Session::new(&store);
    assert_eq!(
        answer(
            &mut session,
            "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
              WHERE c.relname = 'u_key'"
        ),
        "indisvalid\nt\nSELECT 1"
    );
    let error = failure(&mut session, "INSERT INTO u VALUES (1, '2024-05-01', 2)");
    assert_eq!(error.state().code(), "23505", "{error}");
    answer(&mut session, "ALTER TABLE u DROP CONSTRAINT u_key");
    assert_eq!(relation_names(&mut session, "u%key"), "relname\nSELECT 0");
    answer(&mut session, "INSERT INTO u VALUES (1, '2024-05-01', 2)");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM u_1"),
        "count\n2\nSELECT 1"
    );
}

#[test]
fn a_column_inherited_from_the_parent_cannot_be_dropped_from_a_partition() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE e (id int, name text, extra int) PARTITION BY LIST (lower(name));
         CREATE TABLE e1 PARTITION OF e FOR VALUES IN ('a');
         CREATE TABLE g (k int, v int) PARTITION BY LIST (k);
         CREATE TABLE g1 PARTITION OF g FOR VALUES IN (1);
         INSERT INTO e VALUES (1, 'A', 5);
         INSERT INTO g VALUES (1, 2)",
    );
    for (statement, column) in [
        ("ALTER TABLE e1 DROP COLUMN name", "name"),
        ("ALTER TABLE e1 DROP COLUMN extra", "extra"),
        ("ALTER TABLE g1 DROP COLUMN k", "k"),
        ("ALTER TABLE g1 DROP COLUMN v", "v"),
    ] {
        let error = failure(&mut session, statement);
        assert_eq!(error.state().code(), "42P16", "{statement}: {error}");
        assert_eq!(
            error.message(),
            format!("cannot drop inherited column \"{column}\""),
            "{statement}"
        );
    }
    answer(&mut session, "INSERT INTO e VALUES (2, 'a', 6)");
    assert_eq!(
        answer(
            &mut session,
            "SELECT id, name, extra FROM e WHERE lower(name) = 'a' ORDER BY id"
        ),
        "id|name|extra\n1|A|5\n2|a|6\nSELECT 2"
    );
    answer(&mut session, "ALTER TABLE e DROP COLUMN extra");
    assert_eq!(
        answer(&mut session, "SELECT * FROM e1 ORDER BY id"),
        "id|name\n1|A\n2|a\nSELECT 2"
    );
    answer(&mut session, "INSERT INTO e VALUES (3, 'A')");
}

#[test]
fn mysql_names_the_partition_function_when_a_key_column_is_dropped() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    answer(
        &mut session,
        "CREATE TABLE r2 (id int, total int) PARTITION BY RANGE (total * 2) (
             PARTITION p0 VALUES LESS THAN (100),
             PARTITION p1 VALUES LESS THAN MAXVALUE)",
    );
    let error = failure(&mut session, "ALTER TABLE r2 DROP COLUMN total");
    assert_eq!(error.state().code(), "42S22", "{error}");
    assert_eq!(error.vendor_code(), Some(1054));
    assert_eq!(
        error.message(),
        "Unknown column 'total' in 'partition function'"
    );
}

#[test]
fn a_table_qualified_column_in_a_partition_key_survives_partitions_and_renames() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE pe (a int, b int) PARTITION BY LIST ((pe.a));
         CREATE TABLE pe1 PARTITION OF pe DEFAULT;
         INSERT INTO pe VALUES (1, 2);
         CREATE TABLE pr (a int, b int) PARTITION BY RANGE ((pr.a + 1));
         CREATE TABLE pr1 PARTITION OF pr FOR VALUES FROM (0) TO (100);
         INSERT INTO pr VALUES (5, 6);
         CREATE TABLE ps (a int) PARTITION BY HASH ((public.ps.a));
         CREATE TABLE ps0 PARTITION OF ps FOR VALUES WITH (MODULUS 1, REMAINDER 0);
         INSERT INTO ps VALUES (7)",
    );
    assert_eq!(
        answer(&mut session, "SELECT pg_get_partkeydef('pr'::regclass)"),
        "pg_get_partkeydef\nRANGE ((a + 1))\nSELECT 1"
    );
    answer(
        &mut session,
        "ALTER TABLE pe RENAME TO pf; ALTER TABLE pr RENAME TO pq",
    );
    assert_eq!(
        answer(&mut session, "SELECT a, b FROM pf"),
        "a|b\n1|2\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT a, b FROM pq WHERE a + 1 = 6"),
        "a|b\n5|6\nSELECT 1"
    );
    answer(
        &mut session,
        "INSERT INTO pf VALUES (3, 4); INSERT INTO pq VALUES (9, 9)",
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM ps"),
        "count\n1\nSELECT 1"
    );
    let error = failure(
        &mut session,
        "CREATE TABLE px (a int) PARTITION BY LIST ((other.a))",
    );
    assert_eq!(error.state().code(), "42P01", "{error}");
}

fn index_of(store: &MemoryStore, table: &str, index: &str) -> u32 {
    use crate::store::KeyValueStore;
    let reader = store.begin_write().unwrap();
    let bytes = crate::store::KeyValueRead::get(&reader, &crate::encoding::table_key(table))
        .unwrap()
        .unwrap();
    crate::encoding::TableDefinition::decode(table, &bytes)
        .unwrap()
        .indexes
        .iter()
        .find(|found| found.name == index)
        .map(|found| found.id)
        .unwrap()
}

#[test]
fn a_dropped_column_leaves_no_index_link_for_a_later_index_to_inherit() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE p (a int, b int, c int) PARTITION BY RANGE (a);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10);
         CREATE INDEX pb ON p (b);
         CREATE INDEX pc ON p (c)",
    );
    let error = failure(&mut session, "ALTER TABLE p1 DROP COLUMN c");
    assert_eq!(error.state().code(), "42P16", "{error}");
    answer(&mut session, "ALTER TABLE p DROP COLUMN c");
    assert_eq!(answer(&mut session, "SELECT * FROM p1"), "a|b\nSELECT 0");
    answer(&mut session, "CREATE UNIQUE INDEX pa ON p (a)");
    assert_eq!(
        answer(
            &mut session,
            "SELECT c.relname FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
              JOIN pg_class parent ON parent.oid = i.inhparent
             WHERE parent.relname = 'pa'"
        ),
        "relname\np1_a_idx\nSELECT 1"
    );
    answer(&mut session, "INSERT INTO p VALUES (1, 1)");
    let error = failure(&mut session, "INSERT INTO p VALUES (1, 2)");
    assert_eq!(error.state().code(), "23505", "{error}");
    answer(&mut session, "DROP INDEX pa");
    assert_eq!(
        relation_names(&mut session, "p1_%"),
        relation_names(&mut session, "p1_b%")
    );
    answer(&mut session, "CREATE UNIQUE INDEX mine ON p1 (b)");
    answer(&mut session, "DROP INDEX pb");
    let error = failure(&mut session, "INSERT INTO p VALUES (2, 1)");
    assert_eq!(error.state().code(), "23505", "{error}");
}

#[test]
fn a_stale_index_link_from_an_older_writer_attaches_no_later_index() {
    use crate::store::{KeyValueStore, KeyValueWrite};
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE s (a int, b int) PARTITION BY RANGE (a);
         CREATE TABLE s1 PARTITION OF s FOR VALUES FROM (0) TO (10);
         CREATE INDEX sz ON ONLY s (b);
         CREATE INDEX s1_b ON s1 (b)",
    );
    drop(session);
    let parent_index = index_of(&store, "s", "sz");
    let stale = index_of(&store, "s1", "s1_b") + 1;
    let mut writer = store.begin_write().unwrap();
    let mut record = crate::partitions::stored(&writer, "s1").unwrap();
    record.index_links.push(crate::partitions::IndexLink {
        index: stale,
        parent_index,
    });
    writer
        .put(
            &crate::encoding::partition_key("s1"),
            &crate::encoding::encode_partitioning(&record),
        )
        .unwrap();
    writer.commit().unwrap();
    let mut session = Session::new(&store);
    assert_eq!(
        answer(
            &mut session,
            "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
              WHERE c.relname = 'sz'"
        ),
        "indisvalid\nf\nSELECT 1"
    );
    answer(&mut session, "CREATE UNIQUE INDEX mine ON s1 (a)");
    assert!(index_of(&store, "s1", "mine") > stale);
    answer(&mut session, "ALTER INDEX sz ATTACH PARTITION s1_b");
    answer(&mut session, "DROP INDEX sz");
    answer(&mut session, "INSERT INTO s VALUES (1, 1)");
    let error = failure(&mut session, "INSERT INTO s VALUES (1, 2)");
    assert_eq!(error.state().code(), "23505", "{error}");
}

#[test]
fn mysql_alter_table_indexes_reach_every_partition_of_a_year_partitioned_table() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    answer(
        &mut session,
        "CREATE TABLE sales350 (id int NOT NULL, d date NOT NULL)
             PARTITION BY RANGE (YEAR(d)) (
               PARTITION p_old VALUES LESS THAN (2000),
               PARTITION p_new VALUES LESS THAN MAXVALUE
             );
         INSERT INTO sales350 VALUES (1, '1999-12-31'), (2, '2024-06-01');
         ALTER TABLE sales350 ADD INDEX by_d (d)",
    );
    assert_eq!(
        answer(
            &mut Session::new(&store),
            "SELECT id FROM \"sales350#P#p_old\""
        ),
        "id\n1\nSELECT 1"
    );
    let keys = |table: &str| {
        answer(
            &mut Session::new(&store),
            &format!(
                "SELECT c.relname FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
                  JOIN pg_class t ON t.oid = i.indrelid WHERE t.relname = '{table}'"
            ),
        )
    };
    for table in ["sales350", "sales350#P#p_old", "sales350#P#p_new"] {
        assert!(keys(table).contains("\nby_d\n"), "{table}");
    }
    let error = failure(&mut session, "ALTER TABLE sales350 ADD KEY by_d (id)");
    assert_eq!(error.vendor_code(), Some(1061), "{error}");
    answer(
        &mut session,
        "ALTER TABLE sales350 RENAME INDEX by_d TO by_day",
    );
    for table in ["sales350", "sales350#P#p_old", "sales350#P#p_new"] {
        let shown = keys(table);
        assert!(shown.contains("\nby_day\n"), "{table}: {shown}");
        assert!(!shown.contains("\nby_d\n"), "{table}: {shown}");
    }
    assert_eq!(
        answer(
            &mut session,
            "SELECT id FROM sales350 WHERE d = '2024-06-01'"
        ),
        "id\n2\nSELECT 1"
    );
    answer(&mut session, "ALTER TABLE sales350 DROP INDEX by_day");
    for table in ["sales350", "sales350#P#p_old", "sales350#P#p_new"] {
        assert!(!keys(table).contains("by_day"), "{table}");
    }
    let error = failure(&mut session, "ALTER TABLE sales350 DROP INDEX by_day");
    assert_eq!(error.vendor_code(), Some(1091), "{error}");
}

#[test]
fn mysql_subpartitions_by_hash_split_every_partition() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    answer(
        &mut session,
        "CREATE TABLE ts (id int NOT NULL, purchased int NOT NULL)
             PARTITION BY RANGE (purchased) SUBPARTITION BY HASH (id) SUBPARTITIONS 2 (
               PARTITION p0 VALUES LESS THAN (1990),
               PARTITION p1 VALUES LESS THAN MAXVALUE
             )",
    );
    answer(
        &mut session,
        "INSERT INTO ts VALUES (1, 1980), (2, 1985), (3, 2000), (4, 2010)",
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM ts PARTITION (p0sp0, p0sp1)"),
        "count(*)\n2\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM ts PARTITION (p1)"),
        "count(*)\n2\nSELECT 1"
    );
    answer(
        &mut session,
        "CREATE TABLE named (id int NOT NULL, y int NOT NULL)
             PARTITION BY RANGE (y) SUBPARTITION BY KEY (id) (
               PARTITION a VALUES LESS THAN (10) (SUBPARTITION a0, SUBPARTITION a1),
               PARTITION b VALUES LESS THAN (20) (SUBPARTITION b0, SUBPARTITION b1)
             )",
    );
    answer(&mut session, "INSERT INTO named VALUES (1, 5), (2, 15)");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM named PARTITION (b0, b1)"),
        "count(*)\n1\nSELECT 1"
    );
    let error = failure(
        &mut session,
        "CREATE TABLE wrong (id int NOT NULL, y int NOT NULL)
             PARTITION BY RANGE (y) SUBPARTITION BY HASH (id) SUBPARTITIONS 3 (
               PARTITION a VALUES LESS THAN (10) (SUBPARTITION a0, SUBPARTITION a1)
             )",
    );
    assert_eq!(error.vendor_code(), Some(1485), "{error}");
}

#[test]
fn oracle_composite_list_subpartitions_route_by_both_keys() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    answer(
        &mut session,
        "CREATE TABLE orders (id NUMBER, y NUMBER, region VARCHAR2(2))
             PARTITION BY RANGE (y) SUBPARTITION BY LIST (region) (
               PARTITION old VALUES LESS THAN (2000) (
                 SUBPARTITION old_e VALUES ('E'),
                 SUBPARTITION old_rest VALUES (DEFAULT)
               ),
               PARTITION new VALUES LESS THAN (MAXVALUE) (
                 SUBPARTITION new_e VALUES ('E'),
                 SUBPARTITION new_rest VALUES (DEFAULT)
               )
             )",
    );
    answer(
        &mut session,
        "INSERT INTO orders VALUES (1, 1990, 'E');
         INSERT INTO orders VALUES (2, 2010, 'W');
         INSERT INTO orders VALUES (3, 2020, 'E')",
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM orders SUBPARTITION (new_e)"),
        "ID\n3\nSELECT 1"
    );
}

#[test]
fn oracle_refuses_moving_a_row_until_row_movement_is_enabled() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    answer(
        &mut session,
        "CREATE TABLE r (id NUMBER, y NUMBER) PARTITION BY RANGE (y) (
             PARTITION p0 VALUES LESS THAN (10),
             PARTITION p1 VALUES LESS THAN (20)
         )",
    );
    answer(&mut session, "INSERT INTO r VALUES (1, 5)");
    answer(&mut session, "UPDATE r SET y = 6 WHERE id = 1");
    let error = failure(&mut session, "UPDATE r SET y = 15 WHERE id = 1");
    assert_eq!(error.vendor_code(), Some(14402), "{error}");
    assert_eq!(
        answer(&mut session, "SELECT y FROM r PARTITION (p0)"),
        "Y\n6\nSELECT 1"
    );
    answer(&mut session, "ALTER TABLE r ENABLE ROW MOVEMENT");
    answer(&mut session, "UPDATE r SET y = 15 WHERE id = 1");
    assert_eq!(
        answer(&mut session, "SELECT id FROM r PARTITION (p1)"),
        "ID\n1\nSELECT 1"
    );
    answer(&mut session, "ALTER TABLE r DISABLE ROW MOVEMENT");
    let error = failure(&mut session, "UPDATE r SET y = 5 WHERE id = 1");
    assert_eq!(error.vendor_code(), Some(14402), "{error}");
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    answer(
        &mut session,
        "CREATE TABLE m (id NUMBER, y NUMBER) PARTITION BY RANGE (y) (
             PARTITION p0 VALUES LESS THAN (10),
             PARTITION p1 VALUES LESS THAN (20)
         ) ENABLE ROW MOVEMENT",
    );
    answer(&mut session, "INSERT INTO m VALUES (1, 5)");
    answer(&mut session, "UPDATE m SET y = 15 WHERE id = 1");
}

#[test]
fn a_partition_record_keeps_disabled_row_movement_in_version_3() {
    use crate::partitions::{KeyPart, PartitionKey, PartitionRecord, PartitionStrategy};
    let record = PartitionRecord {
        key: Some(PartitionKey {
            strategy: PartitionStrategy::Range,
            parts: vec![KeyPart::Column("y".to_owned())],
        }),
        parent: None,
        index_links: Vec::new(),
        row_movement_disabled: true,
        interval: None,
    };
    let bytes = crate::encoding::encode_partitioning(&record);
    assert_eq!(bytes.first(), Some(&3));
    assert_eq!(
        crate::encoding::decode_partitioning("r", &bytes).ok(),
        Some(record.clone())
    );
    let enabled = PartitionRecord {
        row_movement_disabled: false,
        interval: None,        ..record
    };
    let bytes = crate::encoding::encode_partitioning(&enabled);
    assert_eq!(bytes.first(), Some(&1));
}

#[test]
fn oracle_interval_partitioning_creates_partitions_as_rows_arrive() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    answer(
        &mut session,
        "CREATE TABLE readings (id NUMBER, n NUMBER)
             PARTITION BY RANGE (n) INTERVAL (100) (
               PARTITION p0 VALUES LESS THAN (100)
             )",
    );
    answer(
        &mut session,
        "INSERT INTO readings VALUES (1, 50);
         INSERT INTO readings VALUES (2, 150);
         INSERT INTO readings VALUES (3, 199);
         INSERT INTO readings VALUES (4, 1250)",
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM readings PARTITION FOR (120) ORDER BY id"),
        "ID\n2\n3\nSELECT 2"
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM readings PARTITION FOR (1299)"),
        "ID\n4\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT COUNT(*) FROM readings PARTITION (SYS_P2)"),
        "COUNT(*)\n2\nSELECT 1"
    );
    let error = failure(
        &mut session,
        "ALTER TABLE readings ADD PARTITION p9 VALUES LESS THAN (99999)",
    );
    assert_eq!(error.vendor_code(), Some(14760), "{error}");
    let error = failure(
        &mut session,
        "CREATE TABLE capped (id NUMBER, n NUMBER)
             PARTITION BY RANGE (n) INTERVAL (10) (
               PARTITION top VALUES LESS THAN (MAXVALUE)
             )",
    );
    assert_eq!(error.vendor_code(), Some(14761), "{error}");
}

#[test]
fn oracle_interval_partitioning_by_month() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    answer(
        &mut session,
        "CREATE TABLE sales (id NUMBER, sold DATE)
             PARTITION BY RANGE (sold) INTERVAL (NUMTOYMINTERVAL(1, 'MONTH')) (
               PARTITION before_2024 VALUES LESS THAN (DATE '2024-01-01')
             )",
    );
    answer(
        &mut session,
        "INSERT INTO sales VALUES (1, DATE '2024-03-15');
         INSERT INTO sales VALUES (2, DATE '2024-03-31')",
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT COUNT(*) FROM sales PARTITION FOR (DATE '2024-03-01')"
        ),
        "COUNT(*)\n2\nSELECT 1"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT COUNT(*) FROM sales PARTITION FOR (DATE '2024-02-29')"
        ),
        "COUNT(*)\n0\nSELECT 1"
    )
}

#[test]
fn a_row_named_into_a_subpartition_is_admitted_through_its_partition() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    answer(
        &mut session,
        "CREATE TABLE orders (id NUMBER, y NUMBER, region VARCHAR2(2))
             PARTITION BY RANGE (y) SUBPARTITION BY LIST (region) (
               PARTITION old VALUES LESS THAN (2000) (
                 SUBPARTITION old_e VALUES ('E'),
                 SUBPARTITION old_rest VALUES (DEFAULT)
               ),
               PARTITION new VALUES LESS THAN (MAXVALUE) (
                 SUBPARTITION new_e VALUES ('E'),
                 SUBPARTITION new_rest VALUES (DEFAULT)
               )
             )",
    );
    answer(
        &mut session,
        "INSERT INTO orders SUBPARTITION (new_e) VALUES (1, 2030, 'E')",
    );
    let error = failure(
        &mut session,
        "INSERT INTO orders SUBPARTITION (new_e) VALUES (2, 2030, 'W')",
    );
    assert_eq!(error.vendor_code(), Some(14401), "{error}");
    let error = failure(
        &mut session,
        "INSERT INTO orders SUBPARTITION (new_e) VALUES (3, 1990, 'E')",
    );
    assert_eq!(error.vendor_code(), Some(14401), "{error}");
    assert_eq!(
        answer(&mut session, "SELECT id FROM orders SUBPARTITION (new_e)"),
        "ID\n1\nSELECT 1"
    );
}

#[test]
fn a_row_written_straight_into_a_partition_takes_the_parents_next_id() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE m (id int GENERATED BY DEFAULT AS IDENTITY, at date NOT NULL,
             PRIMARY KEY (id, at)) PARTITION BY RANGE (at);
         CREATE TABLE m_2025 PARTITION OF m FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
         INSERT INTO m (at) VALUES ('2025-02-01')",
    );
    answer(&mut session, "INSERT INTO m_2025 (at) VALUES ('2025-03-01')");
    answer(&mut session, "INSERT INTO m (at) VALUES ('2025-04-01')");
    assert_eq!(
        answer(&mut session, "SELECT id FROM m ORDER BY id"),
        "id\n1\n2\n3\nSELECT 3"
    );
}

#[test]
fn the_partition_tree_functions_and_relispartition_cover_tables_and_indexes() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE p (a int, b int) PARTITION BY RANGE (a);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (a);
         CREATE TABLE p1a PARTITION OF p1 FOR VALUES FROM (0) TO (5);
         CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (10) TO (20);
         CREATE UNIQUE INDEX pa ON p (a)",
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT relid, parentrelid, isleaf, level FROM pg_partition_tree('p')"
        ),
        "relid|parentrelid|isleaf|level\np||f|0\np1|p|f|1\np2|p|t|1\np1a|p1|t|2\nSELECT 4"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT relid FROM pg_partition_ancestors('p1a')"
        ),
        "relid\np1a\np1\np\nSELECT 3"
    );
    assert_eq!(
        answer(&mut session, "SELECT pg_partition_root('p1a')"),
        "pg_partition_root\np\nSELECT 1"
    );
    assert_eq!(
        answer(
            &mut session,
            "SELECT count(*) FROM pg_class WHERE relkind = 'i' AND relispartition"
        ),
        "count\n3\nSELECT 1"
    );
}
