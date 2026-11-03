use crate::memory_store::MemoryStore;
use crate::session::Session;
use crate::tests::render;

fn run(session: &mut Session<'_>, sql: &str) -> String {
    let mut out = Vec::new();
    session
        .execute(sql, &mut |result| out.push(render(&result)))
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    out.join("\n")
}

#[test]
fn materialized_views_hold_results_until_refreshed() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE sales (region text, amount int)");
    run(
        &mut session,
        "INSERT INTO sales VALUES ('north', 10), ('north', 5), ('south', 7)",
    );
    run(
        &mut session,
        "CREATE MATERIALIZED VIEW totals AS SELECT region, sum(amount) AS total FROM sales GROUP BY region",
    );
    let before = run(
        &mut session,
        "SELECT region, total FROM totals ORDER BY region",
    );
    run(&mut session, "INSERT INTO sales VALUES ('south', 3)");
    assert_eq!(
        run(
            &mut session,
            "SELECT region, total FROM totals ORDER BY region"
        ),
        before
    );
    run(&mut session, "REFRESH MATERIALIZED VIEW totals");
    assert_eq!(
        run(
            &mut session,
            "SELECT total FROM totals WHERE region = 'south'"
        ),
        run(&mut session, "SELECT 10::bigint AS total")
    );
    run(
        &mut session,
        "CREATE MATERIALIZED VIEW IF NOT EXISTS totals AS SELECT 1",
    );
    assert!(
        session
            .execute("CREATE MATERIALIZED VIEW totals AS SELECT 1", &mut |_| {})
            .is_err()
    );
    run(
        &mut session,
        "CREATE MATERIALIZED VIEW empty_until_refreshed AS SELECT region FROM sales WITH NO DATA",
    );
    assert_eq!(
        run(&mut session, "SELECT count(*) FROM empty_until_refreshed"),
        run(&mut session, "SELECT 0::bigint AS count")
    );
    run(
        &mut session,
        "DROP MATERIALIZED VIEW totals, empty_until_refreshed",
    );
    assert!(
        session
            .execute("SELECT * FROM totals", &mut |_| {})
            .is_err()
    );
    assert!(
        session
            .execute("REFRESH MATERIALIZED VIEW totals", &mut |_| {})
            .is_err()
    );
    run(&mut session, "DROP MATERIALIZED VIEW IF EXISTS totals");
}

#[test]
fn the_first_materialized_view_is_created_inside_a_transaction_block() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        "BEGIN; CREATE TABLE dingalings (dingaling_id int PRIMARY KEY, data varchar(30)); \
         INSERT INTO dingalings VALUES (1, 'one'), (2, 'two'); \
         CREATE VIEW dingalings_plain AS SELECT * FROM dingalings",
    );
    run(
        &mut session,
        "CREATE MATERIALIZED VIEW dingalings_v AS SELECT * FROM dingalings",
    );
    run(&mut session, "CREATE INDEX mat_index ON dingalings_v (data)");
    assert_eq!(
        run(&mut session, "SELECT count(*) FROM dingalings_v"),
        run(&mut session, "SELECT 2::bigint AS count")
    );
    run(&mut session, "COMMIT");
    assert_eq!(
        run(
            &mut session,
            "SELECT data FROM dingalings_v WHERE dingaling_id = 2"
        ),
        run(&mut session, "SELECT 'two'::varchar(30) AS data")
    );
}

#[test]
fn a_rolled_back_materialized_view_leaves_nothing_behind() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        "CREATE TABLE sales (amount int); BEGIN; \
         CREATE MATERIALIZED VIEW totals AS SELECT sum(amount) AS total FROM sales; ROLLBACK",
    );
    assert!(
        session
            .execute("SELECT * FROM totals", &mut |_| {})
            .is_err()
    );
    run(
        &mut session,
        "CREATE MATERIALIZED VIEW totals AS SELECT sum(amount) AS total FROM sales",
    );
}

#[test]
fn materialized_views_live_in_the_schema_they_are_named_in() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(
        &mut session,
        "CREATE SCHEMA test_schema; \
         CREATE TABLE dingalings (data varchar(30)); \
         CREATE TABLE test_schema.dingalings (data varchar(30)); \
         INSERT INTO test_schema.dingalings VALUES ('in the schema')",
    );
    run(
        &mut session,
        "BEGIN; \
         CREATE MATERIALIZED VIEW dingalings_v AS SELECT * FROM dingalings; \
         CREATE INDEX mat_index ON dingalings_v (data); \
         CREATE MATERIALIZED VIEW test_schema.dingalings_v AS SELECT * FROM test_schema.dingalings; \
         CREATE INDEX mat_index ON test_schema.dingalings_v (data); \
         COMMIT",
    );
    assert_eq!(
        run(&mut session, "SELECT count(*) FROM dingalings_v"),
        run(&mut session, "SELECT 0::bigint AS count")
    );
    run(
        &mut session,
        "INSERT INTO test_schema.dingalings VALUES ('refreshed'); \
         REFRESH MATERIALIZED VIEW test_schema.dingalings_v",
    );
    assert_eq!(
        run(&mut session, "SELECT count(*) FROM test_schema.dingalings_v"),
        run(&mut session, "SELECT 2::bigint AS count")
    );
    run(
        &mut session,
        "DROP MATERIALIZED VIEW test_schema.dingalings_v",
    );
    assert!(
        session
            .execute("SELECT * FROM test_schema.dingalings_v", &mut |_| {})
            .is_err()
    );
    assert_eq!(
        run(&mut session, "SELECT count(*) FROM dingalings_v"),
        run(&mut session, "SELECT 0::bigint AS count")
    );
}
