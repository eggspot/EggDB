use crate::translate;

#[test]
fn pipelines_become_nested_sql() {
    assert_eq!(
        translate("Requests | where Status >= 500 and Url contains \"api\" | project Url, Took = DurationMs / 1000 | take 10").unwrap(),
        "SELECT * FROM (SELECT \"Url\", (\"DurationMs\" / 1000) AS \"Took\" FROM (SELECT * FROM (SELECT * FROM \"Requests\") AS q1 WHERE (\"Status\" >= 500 AND \"Url\" ILIKE '%api%')) AS q2) AS q3 LIMIT 10"
    );
    assert_eq!(
        translate("Events | where Timestamp > ago(1h) | summarize Errors = countif(Level == 'error'), count() by bin(Timestamp, 5m), Host").unwrap(),
        "SELECT time_bucket(interval '5 minutes', \"Timestamp\") AS \"Timestamp\", \"Host\", count(*) FILTER (WHERE \"Level\" = 'error') AS \"Errors\", count(*) AS \"count_\" FROM (SELECT * FROM (SELECT * FROM \"Events\") AS q1 WHERE \"Timestamp\" > (now() - interval '1 hours')) AS q2 GROUP BY 1, 2"
    );
    assert_eq!(
        translate("T | top 3 by Score desc").unwrap(),
        "SELECT * FROM (SELECT * FROM \"T\") AS q1 ORDER BY \"Score\" DESC LIMIT 3"
    );
    assert_eq!(
        translate(
            "T | where Name in ('a', 'b') and At between (datetime(2026-01-01) .. now()) | count"
        )
        .unwrap(),
        "SELECT count(*) AS \"Count\" FROM (SELECT * FROM (SELECT * FROM \"T\") AS q1 WHERE (\"Name\" IN ('a', 'b') AND \"At\" BETWEEN '2026-01-01'::timestamptz AND now())) AS q2"
    );
    assert_eq!(
        translate("T | extend Big = iff(N > 5, 'yes', 'no') | distinct Big").unwrap(),
        "SELECT DISTINCT \"Big\" FROM (SELECT *, (CASE WHEN \"N\" > 5 THEN 'yes' ELSE 'no' END) AS \"Big\" FROM (SELECT * FROM \"T\") AS q1) AS q2"
    );
    assert_eq!(
        translate("T | where Note !contains 'it\\'s_%'").unwrap(),
        "SELECT * FROM (SELECT * FROM \"T\") AS q1 WHERE \"Note\" NOT ILIKE '%it''s\\_\\%%'"
    );
}

#[test]
fn what_is_not_kql_or_not_supported_is_refused() {
    for bad in [
        "",
        "T | where",
        "T | frobnicate",
        "T; DROP TABLE x",
        "T | where x == 'open",
        "T | take many",
        "T | where x contains 5",
        "T | where 1h2",
        "T | summarize percentile(x, 50)",
        "T | where Name == \"a\" or",
    ] {
        assert!(translate(bad).is_err(), "{bad}");
    }
}
