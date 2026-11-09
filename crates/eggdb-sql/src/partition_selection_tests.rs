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

fn listed(session: &mut Session<'_>, sql: &str) -> Vec<String> {
    let output = answer(session, sql);
    let lines: Vec<&str> = output.lines().collect();
    match lines.len() {
        0..=2 => Vec::new(),
        count => lines[1..count - 1]
            .iter()
            .map(|line| (*line).to_owned())
            .collect(),
    }
}

fn mysql_years(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::MySql);
    answer(
        &mut session,
        "CREATE TABLE o (id int NOT NULL, y int NOT NULL, PRIMARY KEY (id, y))
             PARTITION BY RANGE (y) (
               PARTITION p0 VALUES LESS THAN (2000),
               PARTITION p1 VALUES LESS THAN (2020),
               PARTITION p2 VALUES LESS THAN MAXVALUE
             );
         INSERT INTO o VALUES (1, 1999), (2, 2010), (3, 2030), (4, 1990)",
    );
    session
}

#[test]
fn mysql_select_reads_only_the_named_partitions() {
    let store = MemoryStore::default();
    let mut session = mysql_years(&store);
    assert_eq!(
        listed(&mut session, "SELECT id FROM o PARTITION (p0) ORDER BY id"),
        ["1", "4"]
    );
    assert_eq!(
        listed(
            &mut session,
            "SELECT x.id FROM o PARTITION (p0, p2) AS x WHERE x.id > 1 ORDER BY x.id"
        ),
        ["3", "4"]
    );
    assert_eq!(
        listed(&mut session, "SELECT id FROM o PARTITION (P1, p1)"),
        ["2"]
    );
    assert_eq!(
        listed(
            &mut session,
            "SELECT count(*) FROM o PARTITION (p0) WHERE y > 2000"
        ),
        ["0"]
    );
    assert_eq!(
        listed(
            &mut session,
            "SELECT a.id, b.id FROM o PARTITION (p1) a JOIN o PARTITION (p2) b ON b.y > a.y"
        ),
        ["2|3"]
    );
}

#[test]
fn mysql_writes_through_named_partitions_refuse_rows_outside_them() {
    let store = MemoryStore::default();
    let mut session = mysql_years(&store);
    answer(&mut session, "INSERT INTO o PARTITION (p1) VALUES (5, 2015)");
    let error = failure(&mut session, "INSERT INTO o PARTITION (p1) VALUES (6, 1980)");
    assert_eq!(error.vendor_code(), Some(1748), "{error}");
    assert_eq!(
        error.message(),
        "Found a row not matching the given partition set"
    );
    let error = failure(
        &mut session,
        "INSERT INTO o PARTITION (p1) VALUES (7, 2011), (8, 1500)",
    );
    assert_eq!(error.vendor_code(), Some(1748), "{error}");
    answer(
        &mut session,
        "INSERT INTO o PARTITION (p0, p1) (id, y) VALUES (7, 1500), (8, 2001)",
    );
    answer(&mut session, "REPLACE INTO o PARTITION (p1) VALUES (8, 2001)");
    assert_eq!(
        listed(&mut session, "SELECT id FROM o ORDER BY id"),
        ["1", "2", "3", "4", "5", "7", "8"]
    );
    answer(&mut session, "UPDATE o PARTITION (p0) SET id = id + 100");
    assert_eq!(
        listed(&mut session, "SELECT id FROM o ORDER BY id"),
        ["2", "3", "5", "8", "101", "104", "107"]
    );
    let error = failure(
        &mut session,
        "UPDATE o PARTITION (p0) SET y = 2005 WHERE id = 101",
    );
    assert_eq!(error.vendor_code(), Some(1748), "{error}");
    answer(
        &mut session,
        "UPDATE o PARTITION (p0, p1) SET y = 2005 WHERE id = 101",
    );
    assert_eq!(
        listed(&mut session, "SELECT id FROM o PARTITION (p1) ORDER BY id"),
        ["2", "5", "8", "101"]
    );
    answer(&mut session, "DELETE FROM o PARTITION (p2)");
    answer(&mut session, "DELETE FROM o AS x PARTITION (p1) WHERE x.id = 2");
    assert_eq!(
        listed(&mut session, "SELECT id FROM o ORDER BY id"),
        ["5", "8", "101", "104", "107"]
    );
}

#[test]
fn mysql_answers_unknown_partitions_and_unpartitioned_tables_with_its_errors() {
    let store = MemoryStore::default();
    let mut session = mysql_years(&store);
    for sql in [
        "SELECT * FROM o PARTITION (p9)",
        "INSERT INTO o PARTITION (p9) VALUES (9, 1999)",
        "UPDATE o PARTITION (p0, p9) SET id = 1",
        "DELETE FROM o PARTITION (p9)",
    ] {
        let error = failure(&mut session, sql);
        assert_eq!(error.vendor_code(), Some(1735), "{sql}: {error}");
        assert_eq!(error.message(), "Unknown partition 'p9' in table 'o'");
    }
    answer(
        &mut session,
        "CREATE TABLE plain (id int PRIMARY KEY); CREATE VIEW plain_view AS SELECT id FROM plain",
    );
    for sql in [
        "SELECT * FROM plain PARTITION (p0)",
        "SELECT * FROM plain_view PARTITION (p0)",
        "INSERT INTO plain PARTITION (p0) VALUES (1)",
        "INSERT INTO plain PARTITION (p0) (id) VALUES (1 + 1)",
        "UPDATE plain PARTITION (p0) SET id = 2",
        "DELETE FROM plain PARTITION (p0)",
    ] {
        let error = failure(&mut session, sql);
        assert_eq!(error.vendor_code(), Some(1747), "{sql}: {error}");
        assert_eq!(
            error.message(),
            "PARTITION () clause on non partitioned table"
        );
    }
    assert_eq!(listed(&mut session, "SELECT count(*) FROM plain"), ["0"]);
}

#[test]
fn oracle_partition_extension_clauses_name_one_partition_or_the_one_holding_a_key() {
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
    answer(&mut session, "INSERT INTO sales VALUES (2, 'W')");
    answer(&mut session, "INSERT INTO sales VALUES (3, 'N')");
    assert_eq!(
        listed(&mut session, "SELECT id FROM sales PARTITION (east)"),
        ["1"]
    );
    assert_eq!(
        listed(&mut session, "SELECT s.id FROM sales PARTITION FOR ('W') s"),
        ["2"]
    );
    assert_eq!(
        listed(&mut session, "SELECT id FROM sales PARTITION FOR ('Z')"),
        ["3"]
    );
    let error = failure(
        &mut session,
        "INSERT INTO sales PARTITION (east) VALUES (4, 'W')",
    );
    assert_eq!(error.vendor_code(), Some(14401), "{error}");
    answer(
        &mut session,
        "INSERT INTO sales PARTITION (east) (id, region) VALUES (4, 'E')",
    );
    answer(&mut session, "UPDATE sales PARTITION (west) SET id = 20");
    answer(&mut session, "DELETE FROM sales PARTITION FOR ('N')");
    assert_eq!(
        listed(&mut session, "SELECT id FROM sales ORDER BY id"),
        ["1", "4", "20"]
    );
    let error = failure(&mut session, "SELECT * FROM sales PARTITION (nowhere)");
    assert_eq!(error.vendor_code(), Some(2149), "{error}");
    answer(
        &mut session,
        "CREATE TABLE readings (id NUMBER, d NUMBER)
             PARTITION BY RANGE (d) (PARTITION low VALUES LESS THAN (10))",
    );
    let error = failure(&mut session, "SELECT * FROM readings PARTITION FOR (50)");
    assert_eq!(error.vendor_code(), Some(14400), "{error}");
    answer(&mut session, "CREATE TABLE flat (id NUMBER)");
    let error = failure(&mut session, "SELECT * FROM flat PARTITION (p)");
    assert_eq!(error.vendor_code(), Some(14501), "{error}");
}

#[test]
fn postgresql_reads_partition_after_a_table_as_its_alias() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE t (a int, b int) PARTITION BY LIST (a);
         CREATE TABLE p1 PARTITION OF t FOR VALUES IN (1);
         INSERT INTO t VALUES (1, 2)",
    );
    assert_eq!(
        answer(&mut session, "SELECT partition.b FROM t partition"),
        "b\n2\nSELECT 1"
    );
    let error = failure(&mut session, "SELECT * FROM t partition (p1)");
    assert_eq!(error.state().code(), "0A000", "{error}");
    assert!(error.message().contains("column aliases"), "{error}");
}

#[test]
fn mysql_auto_increment_in_a_hash_partitioned_table_numbers_rows_in_statement_order() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    answer(
        &mut session,
        "CREATE TABLE h (id int NOT NULL AUTO_INCREMENT, created_at date NOT NULL, v int,
             PRIMARY KEY (id)) PARTITION BY HASH (id) PARTITIONS 4",
    );
    answer(
        &mut session,
        "INSERT INTO h (created_at, v) VALUES ('2024-01-01', 1), ('2024-02-01', 2), ('2024-03-01', 3)",
    );
    assert_eq!(listed(&mut session, "SELECT LAST_INSERT_ID()"), ["1"]);
    answer(
        &mut session,
        "INSERT INTO h (id, created_at, v) VALUES (10, '2024-04-01', 4)",
    );
    answer(
        &mut session,
        "INSERT INTO h (created_at, v) VALUES ('2024-05-01', 5), ('2024-06-01', 6)",
    );
    assert_eq!(listed(&mut session, "SELECT LAST_INSERT_ID()"), ["11"]);
    answer(
        &mut session,
        "INSERT INTO h VALUES (0, '2024-07-01', 7), (NULL, '2024-08-01', 8)",
    );
    assert_eq!(listed(&mut session, "SELECT LAST_INSERT_ID()"), ["13"]);
    assert_eq!(
        listed(&mut session, "SELECT id, v FROM h ORDER BY id"),
        ["1|1", "2|2", "3|3", "10|4", "11|5", "12|6", "13|7", "14|8"]
    );
    let filled = ["p0", "p1", "p2", "p3"]
        .iter()
        .filter(|partition| {
            listed(
                &mut session,
                &format!("SELECT count(*) FROM h PARTITION ({partition})"),
            ) != ["0"]
        })
        .count();
    assert!(filled > 1, "the ids spread over the hash partitions");
}

#[test]
fn mysql_auto_increment_takes_the_furthest_id_of_every_range_partition() {
    for primary_key in [
        "PRIMARY KEY (id, created_at)",
        "PRIMARY KEY (created_at, id), KEY id_key (id)",
    ] {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, Dialect::MySql);
        answer(
            &mut session,
            &format!(
                "CREATE TABLE r (id int NOT NULL AUTO_INCREMENT, created_at date NOT NULL,
                     {primary_key})
                     PARTITION BY RANGE COLUMNS (created_at) (
                       PARTITION p2024 VALUES LESS THAN ('2025-01-01'),
                       PARTITION p2025 VALUES LESS THAN MAXVALUE
                     )"
            ),
        );
        answer(
            &mut session,
            "INSERT INTO r (created_at) VALUES ('2025-03-01'), ('2024-03-01'), ('2025-06-01')",
        );
        answer(&mut session, "INSERT INTO r (created_at) VALUES ('2024-05-01')");
        assert_eq!(
            listed(&mut session, "SELECT id FROM r PARTITION (p2024) ORDER BY id"),
            ["2", "4"],
            "{primary_key}"
        );
        assert_eq!(
            listed(&mut session, "SELECT id FROM r PARTITION (p2025) ORDER BY id"),
            ["1", "3"],
            "{primary_key}"
        );
        assert_eq!(listed(&mut session, "SELECT LAST_INSERT_ID()"), ["4"]);
        answer(
            &mut session,
            "UPDATE r SET created_at = '2024-12-31' WHERE id = 3",
        );
        answer(&mut session, "INSERT INTO r (created_at) VALUES ('2025-02-02')");
        assert_eq!(
            listed(&mut session, "SELECT id FROM r PARTITION (p2024) ORDER BY id"),
            ["2", "3", "4"],
            "a moved row keeps its id: {primary_key}"
        );
        assert_eq!(
            listed(&mut session, "SELECT max(id) FROM r"),
            ["5"],
            "{primary_key}"
        );
    }
}

#[test]
fn postgresql_identity_columns_share_one_counter_across_partitions() {
    for primary_key in ["PRIMARY KEY (id, created_at)", "PRIMARY KEY (created_at, id)"] {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        answer(
            &mut session,
            &format!(
                "CREATE TABLE ev (id bigint GENERATED ALWAYS AS IDENTITY, created_at date NOT NULL,
                     {primary_key}) PARTITION BY RANGE (created_at);
                 CREATE TABLE ev_2024 PARTITION OF ev FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
                 CREATE TABLE ev_2025 PARTITION OF ev FOR VALUES FROM ('2025-01-01') TO ('2026-01-01')"
            ),
        );
        answer(
            &mut session,
            "INSERT INTO ev (created_at) VALUES ('2025-03-01'), ('2024-03-01'), ('2025-01-02')",
        );
        answer(&mut session, "INSERT INTO ev (created_at) VALUES ('2024-01-05')");
        assert_eq!(
            listed(&mut session, "SELECT id FROM ev_2024 ORDER BY id"),
            ["2", "4"],
            "{primary_key}"
        );
        assert_eq!(
            listed(&mut session, "SELECT last_value FROM ev_id_seq"),
            ["4"],
            "{primary_key}"
        );
        let error = failure(
            &mut session,
            "INSERT INTO ev (id, created_at) VALUES (50, '2024-02-02')",
        );
        assert_eq!(error.state().code(), "428C9", "{error}");
        answer(
            &mut session,
            "INSERT INTO ev (id, created_at) OVERRIDING SYSTEM VALUE VALUES (50, '2024-02-02')",
        );
        answer(
            &mut session,
            "INSERT INTO ev (id, created_at) OVERRIDING USER VALUE VALUES (7, '2025-05-05')",
        );
        assert_eq!(
            listed(&mut session, "SELECT id FROM ev_2025 ORDER BY id"),
            ["1", "3", "51"],
            "{primary_key}"
        );
        let error = failure(&mut session, "UPDATE ev SET id = 5 WHERE id = 1");
        assert_eq!(error.state().code(), "428C9", "{error}");
    }
}

#[test]
fn postgresql_hash_partitions_of_an_identity_key_number_rows_in_statement_order() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    answer(
        &mut session,
        "CREATE TABLE hp (id int GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, v text)
             PARTITION BY HASH (id);
         CREATE TABLE hp0 PARTITION OF hp FOR VALUES WITH (MODULUS 2, REMAINDER 0);
         CREATE TABLE hp1 PARTITION OF hp FOR VALUES WITH (MODULUS 2, REMAINDER 1);
         INSERT INTO hp (v) VALUES ('a'), ('b'), ('c'), ('d')",
    );
    assert_eq!(
        listed(&mut session, "SELECT id, v FROM hp ORDER BY id"),
        ["1|a", "2|b", "3|c", "4|d"]
    );
    answer(&mut session, "INSERT INTO hp (v) VALUES ('e')");
    assert_eq!(listed(&mut session, "SELECT max(id) FROM hp"), ["5"]);
}
