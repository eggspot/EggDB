use crate::memory_store::MemoryStore;
use crate::session::Session;
use crate::tests::render;

fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> String {
    let mut out = Vec::new();
    session.execute(sql, &mut |result| out.push(render(&result))).unwrap_or_else(|error| panic!("{sql}: {error}"));
    out.join("\n")
}

#[test]
fn materialized_views_hold_results_until_refreshed() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    run(&mut session, "CREATE TABLE sales (region text, amount int)");
    run(&mut session, "INSERT INTO sales VALUES ('north', 10), ('north', 5), ('south', 7)");
    run(&mut session, "CREATE MATERIALIZED VIEW totals AS SELECT region, sum(amount) AS total FROM sales GROUP BY region");
    let before = run(&mut session, "SELECT region, total FROM totals ORDER BY region");
    run(&mut session, "INSERT INTO sales VALUES ('south', 3)");
    assert_eq!(run(&mut session, "SELECT region, total FROM totals ORDER BY region"), before);
    run(&mut session, "REFRESH MATERIALIZED VIEW totals");
    assert_eq!(
        run(&mut session, "SELECT total FROM totals WHERE region = 'south'"),
        run(&mut session, "SELECT 10::bigint AS total")
    );
    run(&mut session, "CREATE MATERIALIZED VIEW IF NOT EXISTS totals AS SELECT 1");
    assert!(session.execute("CREATE MATERIALIZED VIEW totals AS SELECT 1", &mut |_| {}).is_err());
    run(&mut session, "CREATE MATERIALIZED VIEW empty_until_refreshed AS SELECT region FROM sales WITH NO DATA");
    assert_eq!(run(&mut session, "SELECT count(*) FROM empty_until_refreshed"), run(&mut session, "SELECT 0::bigint"));
    run(&mut session, "DROP MATERIALIZED VIEW totals, empty_until_refreshed");
    assert!(session.execute("SELECT * FROM totals", &mut |_| {}).is_err());
    assert!(session.execute("REFRESH MATERIALIZED VIEW totals", &mut |_| {}).is_err());
    run(&mut session, "DROP MATERIALIZED VIEW IF EXISTS totals");
}
