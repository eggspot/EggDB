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

fn years(store: &MemoryStore, dialect: Dialect) -> Session<'_> {
    let mut session = Session::with_dialect(store, dialect);
    answer(
        &mut session,
        "CREATE TABLE o (id int NOT NULL, y int NOT NULL, PRIMARY KEY (id, y))
             PARTITION BY RANGE (y) (
               PARTITION p0 VALUES LESS THAN (2000),
               PARTITION p1 VALUES LESS THAN (2020)
             )",
    );
    answer(&mut session, "INSERT INTO o VALUES (1, 1999), (2, 2010), (3, 2015)");
    session
}

#[test]
fn mysql_adds_a_range_partition_above_the_last_and_refuses_one_below() {
    let store = MemoryStore::default();
    let mut session = years(&store, Dialect::MySql);
    answer(
        &mut session,
        "ALTER TABLE o ADD PARTITION (PARTITION p2 VALUES LESS THAN (2030))",
    );
    answer(&mut session, "INSERT INTO o VALUES (4, 2025)");
    assert_eq!(
        answer(&mut session, "SELECT id FROM o PARTITION (p2)"),
        "id\n4\nSELECT 1"
    );
    let error = failure(
        &mut session,
        "ALTER TABLE o ADD PARTITION (PARTITION p3 VALUES LESS THAN (2010))",
    );
    assert!(error.message().contains("strictly increasing"), "{error}");
}

#[test]
fn dropping_a_partition_removes_its_rows() {
    let store = MemoryStore::default();
    let mut session = years(&store, Dialect::MySql);
    answer(&mut session, "ALTER TABLE o DROP PARTITION p0");
    assert_eq!(
        answer(&mut session, "SELECT id FROM o ORDER BY id"),
        "id\n2\n3\nSELECT 2"
    );
    let error = failure(&mut session, "ALTER TABLE o DROP PARTITION p1");
    assert!(error.message().contains("Cannot remove all partitions"), "{error}");
}

#[test]
fn truncating_a_partition_keeps_the_others() {
    let store = MemoryStore::default();
    let mut session = years(&store, Dialect::MySql);
    answer(&mut session, "ALTER TABLE o TRUNCATE PARTITION p1");
    assert_eq!(
        answer(&mut session, "SELECT id FROM o"),
        "id\n1\nSELECT 1"
    );
}

#[test]
fn mysql_reorganizes_one_partition_into_two_and_keeps_every_row() {
    let store = MemoryStore::default();
    let mut session = years(&store, Dialect::MySql);
    answer(
        &mut session,
        "ALTER TABLE o REORGANIZE PARTITION p1 INTO (
             PARTITION p1a VALUES LESS THAN (2012),
             PARTITION p1b VALUES LESS THAN (2020))",
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM o PARTITION (p1b)"),
        "id\n3\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM o"),
        "count(*)\n3\nSELECT 1"
    );
}

#[test]
fn oracle_splits_a_range_partition_at_a_value() {
    let store = MemoryStore::default();
    let mut session = years(&store, Dialect::Oracle);
    answer(
        &mut session,
        "ALTER TABLE o SPLIT PARTITION p1 AT (2012) INTO (PARTITION p1a, PARTITION p1b)",
    );
    assert_eq!(
        answer(&mut session, "SELECT id FROM o PARTITION (p1a)"),
        "ID\n2\nSELECT 1"
    );
}

#[test]
fn a_partition_exchanges_its_rows_with_a_table() {
    let store = MemoryStore::default();
    let mut session = years(&store, Dialect::MySql);
    answer(
        &mut session,
        "CREATE TABLE swap (id int NOT NULL, y int NOT NULL, PRIMARY KEY (id, y));
         INSERT INTO swap VALUES (9, 1990)",
    );
    answer(&mut session, "ALTER TABLE o EXCHANGE PARTITION p0 WITH TABLE swap");
    assert_eq!(
        answer(&mut session, "SELECT id FROM o PARTITION (p0)"),
        "id\n9\nSELECT 1"
    );
    assert_eq!(answer(&mut session, "SELECT id FROM swap"), "id\n1\nSELECT 1");
    answer(&mut session, "INSERT INTO swap VALUES (8, 2050)");
    let error = failure(&mut session, "ALTER TABLE o EXCHANGE PARTITION p0 WITH TABLE swap");
    assert!(error.message().contains("partition constraint"), "{error}");
}

#[test]
fn mysql_adds_hash_partitions_and_spreads_the_rows_again() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    answer(
        &mut session,
        "CREATE TABLE h (id int NOT NULL, PRIMARY KEY (id)) PARTITION BY HASH (id) PARTITIONS 2;
         INSERT INTO h VALUES (1), (2), (3), (4), (5), (6)",
    );
    answer(&mut session, "ALTER TABLE h ADD PARTITION PARTITIONS 2");
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM h"),
        "count(*)\n6\nSELECT 1"
    );
    assert_eq!(
        answer(&mut session, "SELECT count(*) FROM h PARTITION (p0, p1, p2, p3)"),
        "count(*)\n6\nSELECT 1"
    );
}

#[test]
fn mysql_partitions_by_range_list_and_hash_take_only_integers_and_its_own_functions() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::MySql);
    let error = failure(
        &mut session,
        "CREATE TABLE d (id int NOT NULL, at date NOT NULL) PARTITION BY RANGE (at)
             (PARTITION p0 VALUES LESS THAN (10))",
    );
    assert_eq!(error.vendor_code(), Some(1659), "{error}");
    let error = failure(
        &mut session,
        "CREATE TABLE n (id int NOT NULL, name varchar(20) NOT NULL)
             PARTITION BY HASH (length(name)) PARTITIONS 2",
    );
    assert_eq!(error.vendor_code(), Some(1564), "{error}");
    answer(
        &mut session,
        "CREATE TABLE c (id int NOT NULL, at date NOT NULL) PARTITION BY RANGE COLUMNS (at)
             (PARTITION p0 VALUES LESS THAN ('2020-01-01'))",
    );
}

#[test]
fn a_postgresql_partition_key_with_an_unknown_function_is_42883() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    let error = failure(
        &mut session,
        "CREATE TABLE f (id int, v text) PARTITION BY LIST (no_such_function(v))",
    );
    assert_eq!(error.state().code(), "42883", "{error}");
}
