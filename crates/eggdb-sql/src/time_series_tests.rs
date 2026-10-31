use crate::memory_store::MemoryStore;
use crate::session::Session;
use crate::tests::render;

fn same(store: &MemoryStore, sql: &str, expected: &str) {
    let mut session = Session::new(store);
    let mut run = |text: &str| {
        let mut out = Vec::new();
        session.execute(text, &mut |result| out.push(render(&result))).unwrap_or_else(|error| panic!("{text}: {error}"));
        out.join("\n")
    };
    assert_eq!(run(sql), run(expected), "{sql}");
}

#[test]
fn time_bucket_bins_timestamps_from_timescaledbs_origin() {
    let store = MemoryStore::default();
    same(&store, "SELECT time_bucket('15 minutes', TIMESTAMP '2024-03-05 10:37:12')", "SELECT TIMESTAMP '2024-03-05 10:30:00'");
    same(&store, "SELECT time_bucket(INTERVAL '1 week', TIMESTAMP '2024-03-07 10:00')", "SELECT TIMESTAMP '2024-03-04 00:00'");
    same(&store, "SELECT time_bucket('1 day', TIMESTAMPTZ '2024-03-05 23:59:59+00')", "SELECT TIMESTAMPTZ '2024-03-05 00:00:00+00'");
    same(&store, "SELECT time_bucket('1 hour', TIMESTAMP '2024-03-05 10:37', INTERVAL '15 minutes')", "SELECT TIMESTAMP '2024-03-05 10:15'");
    same(&store, "SELECT time_bucket('1 hour', TIMESTAMP '2024-03-05 10:37', TIMESTAMP '2024-01-01 00:20')", "SELECT TIMESTAMP '2024-03-05 10:20'");
}

#[test]
fn time_bucket_handles_months_dates_and_integers() {
    let store = MemoryStore::default();
    same(&store, "SELECT time_bucket('3 months', TIMESTAMP '2024-08-17 12:00')", "SELECT TIMESTAMP '2024-07-01 00:00'");
    same(&store, "SELECT time_bucket('1 year', DATE '2024-08-17')", "SELECT DATE '2024-01-01'");
    same(&store, "SELECT time_bucket('2 days', DATE '2024-08-17')", "SELECT DATE '2024-08-16'");
    same(&store, "SELECT time_bucket(10, 23), time_bucket(10, -3), time_bucket(10, 23, 5)", "SELECT 20, -10, 15");
    same(&store, "SELECT time_bucket('1 hour', NULL::timestamp) IS NULL", "SELECT true");
    let mut session = Session::new(&store);
    for bad in ["SELECT time_bucket('0 minutes', now())", "SELECT time_bucket('1 month 2 days', now())", "SELECT time_bucket(0, 5)"] {
        assert!(session.execute(bad, &mut |_| {}).is_err(), "{bad}");
    }
}

#[test]
fn time_bucket_groups_readings() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session
        .execute(
            "CREATE TABLE readings (at timestamptz, value double precision);
             INSERT INTO readings VALUES ('2024-01-01 00:01+00', 1), ('2024-01-01 00:14+00', 3), ('2024-01-01 00:16+00', 10)",
            &mut |_| {},
        )
        .unwrap();
    same(
        &store,
        "SELECT time_bucket('15 minutes', at) AS bucket, avg(value) FROM readings GROUP BY bucket ORDER BY bucket",
        "SELECT TIMESTAMPTZ '2024-01-01 00:00+00' AS bucket, 2::double precision AS avg UNION ALL SELECT TIMESTAMPTZ '2024-01-01 00:15+00', 10",
    );
}
