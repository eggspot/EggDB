use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session, SqlError};

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

fn failure(session: &mut Session<'_, MemoryStore>, sql: &str) -> SqlError {
    match run(session, sql) {
        Ok(output) => panic!("{sql} succeeded: {output}"),
        Err(error) => error,
    }
}

fn with_measurements(store: &MemoryStore) -> Session<'_, MemoryStore> {
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
    assert_eq!(answer(&mut session, "SELECT id FROM m_2025"), "id\n2\nSELECT 1");
    assert_eq!(
        answer(&mut session, "SELECT id, v FROM m ORDER BY id"),
        "id|v\n1|10\n2|20\n3|30\nSELECT 3"
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM m WHERE at >= '2025-01-01'"),
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
        error.message().contains("no partition of relation \"m\" found for row"),
        "{error}"
    );
    let error = failure(&mut session, "INSERT INTO m_2024 VALUES (9, '2025-06-01', 1)");
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
    let plan = answer(&mut session, "EXPLAIN SELECT * FROM m AS x WHERE x.at < '2024-06-01'");
    assert!(plan.contains("m_2024") && !plan.contains("m_2025"), "{plan}");
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
    let error = failure(&mut session, "UPDATE m SET at = '2024-01-05' WHERE id = 2");
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
    assert_eq!(answer(&mut session, "SELECT id FROM c_eu"), "id\n1\nSELECT 1");
    assert_eq!(
        answer(&mut session, "SELECT id FROM c_rest ORDER BY id"),
        "id\n3\n4\nSELECT 2"
    );
    let error = failure(&mut session, "CREATE TABLE c_x PARTITION OF c FOR VALUES IN ('us')");
    assert!(error.message().contains("would overlap"), "{error}");
    let error = failure(&mut session, "CREATE TABLE c_jp PARTITION OF c FOR VALUES IN ('jp')");
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
    assert!(failure(&mut session, "SELECT * FROM m_2024").message().contains("m_2024"));
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
    answer(
        &mut session,
        "INSERT INTO sales VALUES (1, 'E')",
    );
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
        "CREATE TABLE a (x int) PARTITION BY RANGE ((x + 1))",
        "CREATE TABLE a (x int) PARTITION BY HASH (x) PARTITIONS 0",
    ] {
        failure(&mut session, sql);
    }
    answer(&mut session, "CREATE TABLE r (x int) PARTITION BY RANGE (x)");
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
        answer(&mut session, &format!("INSERT INTO a VALUES ({})", value % 200));
    }
    answer(&mut session, "ANALYZE a");
    answer(&mut session, "ANALYZE");
    assert_eq!(
        answer(&mut session, "SELECT reltuples FROM pg_class WHERE relname = 'a'"),
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
