use crate::Endpoint;
use crate::promql::{Data, Evaluator, Failure, Work, parse};
use crate::store::Series;
use crate::tests::{MemoryStore, evaluate, post, series, vector};
use eggdb_platform::connection_limits::MemoryBudget;
use std::collections::BTreeMap;

type Fetched = BTreeMap<String, Vec<Series>>;
type Row = (Vec<(String, String)>, f64);

fn fetched(all: Vec<Series>) -> Fetched {
    let mut data: Fetched = BTreeMap::new();
    for one in all {
        let name = one
            .labels
            .iter()
            .find(|(label, _)| label == "__name__")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        data.entry(name).or_default().push(one);
    }
    data
}

fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn sorted(query: &str, time_ms: i64, data: &Fetched) -> Vec<Row> {
    let parsed = parse(query).unwrap_or_else(|error| panic!("{query}: {error}"));
    let mut rows =
        vector(evaluate(&parsed, time_ms, data).unwrap_or_else(|error| panic!("{query}: {error}")));
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

fn failure(query: &str, time_ms: i64, data: &Fetched) -> String {
    match parse(query).and_then(|parsed| evaluate(&parsed, time_ms, data)) {
        Ok(result) => panic!("{query} answered {result:?}"),
        Err(message) => message,
    }
}

fn close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() < 1e-9 || (actual.is_nan() && expected.is_nan()) || actual == expected
}

fn values(rows: &[Row]) -> Vec<f64> {
    rows.iter().map(|(_, value)| *value).collect()
}

fn http_requests() -> Fetched {
    let mut all = Vec::new();
    for (index, (job, instance, group)) in [
        ("api-server", "0", "production"),
        ("api-server", "1", "production"),
        ("api-server", "0", "canary"),
        ("api-server", "1", "canary"),
        ("app-server", "0", "production"),
        ("app-server", "1", "production"),
        ("app-server", "0", "canary"),
        ("app-server", "1", "canary"),
    ]
    .into_iter()
    .enumerate()
    {
        all.push(series(
            &[
                ("__name__", "http_requests"),
                ("group", group),
                ("instance", instance),
                ("job", job),
            ],
            &[(30_000, 100.0 * (index as f64 + 1.0))],
        ));
    }
    for (job, instance, group, version) in [
        ("api-server", "0", "production", 6.0),
        ("api-server", "1", "production", 6.0),
        ("api-server", "0", "canary", 8.0),
        ("api-server", "1", "canary", 8.0),
        ("app-server", "0", "production", 6.0),
        ("app-server", "1", "production", 6.0),
        ("app-server", "0", "canary", 7.0),
        ("app-server", "1", "canary", 7.0),
    ] {
        all.push(series(
            &[
                ("__name__", "version"),
                ("group", group),
                ("instance", instance),
                ("job", job),
            ],
            &[(30_000, version)],
        ));
    }
    fetched(all)
}

#[test]
fn aggregations_answer_as_the_prometheus_docs_and_tests_show() {
    let data = http_requests();
    let at = |query: &str| sorted(query, 60_000, &data);
    assert_eq!(
        at("sum by (group) (http_requests)"),
        [
            (labels(&[("group", "canary")]), 2200.0),
            (labels(&[("group", "production")]), 1400.0),
        ]
    );
    let deviations = at("stddev by (instance) (http_requests)");
    assert_eq!(deviations.len(), 2);
    assert!(
        deviations
            .iter()
            .all(|(_, value)| close(*value, 223.606_797_749_979)),
        "{deviations:?}"
    );
    assert!(close(
        at("stdvar by (instance) (http_requests)")[0].1,
        50_000.0
    ));
    assert_eq!(
        at("group by (job) (http_requests)"),
        [
            (labels(&[("job", "api-server")]), 1.0),
            (labels(&[("job", "app-server")]), 1.0),
        ]
    );
    let top = vector(evaluate(&parse("topk(3, http_requests)").unwrap(), 60_000, &data).unwrap());
    assert_eq!(values(&top), [800.0, 700.0, 600.0]);
    assert_eq!(
        top[0].0,
        labels(&[
            ("__name__", "http_requests"),
            ("group", "canary"),
            ("instance", "1"),
            ("job", "app-server"),
        ])
    );
    let bottom =
        vector(evaluate(&parse("bottomk(3, http_requests)").unwrap(), 60_000, &data).unwrap());
    assert_eq!(values(&bottom), [100.0, 200.0, 300.0]);
    let per_group = at("topk by (group) (1, http_requests)");
    assert_eq!(values(&per_group), [800.0, 600.0]);
    assert_eq!(
        values(&at("topk(1, http_requests) by (group)")),
        [800.0, 600.0]
    );
    assert!(at("topk(0, http_requests)").is_empty());
    assert_eq!(
        values(&at("topk(scalar(vector(2)), http_requests)")).len(),
        2
    );
    assert!(close(at("quantile(0.5, http_requests)")[0].1, 450.0));
    assert!(close(at("quantile(0.8, http_requests)")[0].1, 660.0));
    assert_eq!(at("quantile(-1, http_requests)")[0].1, f64::NEG_INFINITY);
    assert_eq!(at("quantile(2, http_requests)")[0].1, f64::INFINITY);
    assert_eq!(
        at("count_values(\"version\", version)"),
        [
            (labels(&[("version", "6")]), 4.0),
            (labels(&[("version", "7")]), 2.0),
            (labels(&[("version", "8")]), 2.0),
        ]
    );
    let without_instance = at("count_values without (instance) (\"version\", version)");
    assert_eq!(without_instance.len(), 4);
    assert!(without_instance.contains(&(
        labels(&[
            ("group", "production"),
            ("job", "api-server"),
            ("version", "6")
        ]),
        2.0
    )));
    for (broken, expected) in [
        ("topk(http_requests)", "expected 2, got 1"),
        ("sum(1, http_requests)", "expected 1, got 2"),
        ("count_values(1, version)", "expected type string"),
        ("quantile(\"x\", version)", "expected type scalar"),
        ("count_values(\"1x\", version)", "invalid label name"),
        ("topk(http_requests, http_requests)", "expected type scalar"),
    ] {
        let message = failure(broken, 60_000, &data);
        assert!(message.contains(expected), "{broken}: {message}");
    }
}

fn errors_and_requests() -> Fetched {
    let mut all = Vec::new();
    for (method, code, value) in [
        ("get", "500", 24.0),
        ("get", "404", 30.0),
        ("put", "501", 3.0),
        ("post", "500", 6.0),
        ("post", "404", 21.0),
    ] {
        all.push(series(
            &[
                ("__name__", "method_code:http_errors:rate5m"),
                ("code", code),
                ("method", method),
            ],
            &[(30_000, value)],
        ));
    }
    for (method, value) in [("get", 600.0), ("del", 34.0), ("post", 120.0)] {
        all.push(series(
            &[
                ("__name__", "method:http_requests:rate5m"),
                ("method", method),
            ],
            &[(30_000, value)],
        ));
    }
    all.push(series(
        &[
            ("__name__", "method_info"),
            ("method", "get"),
            ("owner", "web"),
        ],
        &[(30_000, 1.0)],
    ));
    fetched(all)
}

#[test]
fn vector_matching_follows_the_prometheus_operator_docs() {
    let data = errors_and_requests();
    let at = |query: &str| sorted(query, 60_000, &data);
    assert_eq!(
        at(
            "method_code:http_errors:rate5m{code=\"500\"} / ignoring(code) method:http_requests:rate5m"
        ),
        [
            (labels(&[("method", "get")]), 0.04),
            (labels(&[("method", "post")]), 0.05),
        ]
    );
    assert_eq!(
        at("method_code:http_errors:rate5m{code=\"500\"} / on(method) method:http_requests:rate5m"),
        [
            (labels(&[("method", "get")]), 0.04),
            (labels(&[("method", "post")]), 0.05),
        ]
    );
    let grouped = at(
        "method_code:http_errors:rate5m / ignoring(code) group_left method:http_requests:rate5m",
    );
    assert_eq!(grouped.len(), 4);
    for (expected_labels, expected) in [
        (labels(&[("code", "404"), ("method", "get")]), 0.05),
        (labels(&[("code", "404"), ("method", "post")]), 0.175),
        (labels(&[("code", "500"), ("method", "get")]), 0.04),
        (labels(&[("code", "500"), ("method", "post")]), 0.05),
    ] {
        assert!(
            grouped
                .iter()
                .any(|(found, value)| *found == expected_labels && close(*value, expected)),
            "{grouped:?}"
        );
    }
    let reversed = at(
        "method:http_requests:rate5m / ignoring(code) group_right method_code:http_errors:rate5m",
    );
    assert_eq!(reversed.len(), 4);
    assert!(reversed.contains(&(labels(&[("code", "500"), ("method", "get")]), 25.0)));
    let owners = at("method_code:http_errors:rate5m * on(method) group_left(owner) method_info");
    assert_eq!(
        owners,
        [
            (
                labels(&[("code", "404"), ("method", "get"), ("owner", "web")]),
                30.0
            ),
            (
                labels(&[("code", "500"), ("method", "get"), ("owner", "web")]),
                24.0
            ),
        ]
    );
    assert!(
        failure(
            "method_code:http_errors:rate5m / ignoring(code) method:http_requests:rate5m",
            60_000,
            &data
        )
        .contains("many-to-one matching must be explicit (group_left/group_right)")
    );
    let many_to_many = failure(
        "method:http_requests:rate5m / ignoring(code) group_left method_code:http_errors:rate5m",
        60_000,
        &data,
    );
    assert!(
        many_to_many.contains("found duplicate series for the match group {method=\"get\"} on the right hand-side of the operation")
            && many_to_many.contains("many-to-many matching not allowed: matching labels must be unique on one side"),
        "{many_to_many}"
    );
    assert_eq!(
        values(&at(
            "method_code:http_errors:rate5m and on(method) method:http_requests:rate5m{method=\"get\"}"
        )),
        [30.0, 24.0]
    );
    assert_eq!(
        at("method_code:http_errors:rate5m unless on(method) method:http_requests:rate5m").len(),
        1
    );
    assert_eq!(
        at("method:http_requests:rate5m or method:http_requests:rate5m{method=\"get\"}").len(),
        3
    );
    assert_eq!(
        at("method:http_requests:rate5m or on(method) method_code:http_errors:rate5m").len(),
        4
    );
    for (broken, expected) in [
        ("method_info and group_left method_info", "unexpected"),
        (
            "method_info and on(method) group_left method_info",
            "no grouping allowed for \"and\" operation",
        ),
        (
            "method_info * on(owner) group_left(owner) method_info",
            "must not occur in ON and GROUP clause at once",
        ),
        (
            "method_info + on(method) 1",
            "only allowed between instant vectors",
        ),
        ("1 and 1", "only allowed between instant vectors"),
    ] {
        let message = failure(broken, 60_000, &data);
        assert!(message.contains(expected), "{broken}: {message}");
    }
}

#[test]
fn the_bool_modifier_returns_zero_or_one_and_drops_the_metric_name() {
    let data = errors_and_requests();
    let at = |query: &str| sorted(query, 60_000, &data);
    assert_eq!(
        at("method:http_requests:rate5m > bool 100"),
        [
            (labels(&[("method", "del")]), 0.0),
            (labels(&[("method", "get")]), 1.0),
            (labels(&[("method", "post")]), 1.0),
        ]
    );
    let filtered = at("method:http_requests:rate5m > 100");
    assert_eq!(filtered.len(), 2);
    assert!(
        filtered
            .iter()
            .all(|(found, _)| found.first().is_some_and(|(name, _)| name == "__name__"))
    );
    assert_eq!(
        values(&at(
            "method_code:http_errors:rate5m{code=\"500\"} >= bool ignoring(code) method:http_requests:rate5m / 20"
        )),
        [0.0, 1.0]
    );
    assert!(
        matches!(evaluate(&parse("2 > bool 1").unwrap(), 0, &data), Ok(Data::Scalar(value)) if value == 1.0)
    );
    assert!(
        matches!(evaluate(&parse("2 == bool 1").unwrap(), 0, &data), Ok(Data::Scalar(value)) if value == 0.0)
    );
    assert!(
        failure("1 > 2", 0, &data).contains("comparisons between scalars must use BOOL modifier")
    );
    assert!(
        failure("1 + bool 2", 0, &data)
            .contains("bool modifier can only be used on comparison operators")
    );
    assert!(
        matches!(evaluate(&parse("2 ^ 3 ^ 2").unwrap(), 0, &data), Ok(Data::Scalar(value)) if value == 512.0)
    );
    assert!(
        matches!(evaluate(&parse("-2 ^ 2").unwrap(), 0, &data), Ok(Data::Scalar(value)) if value == -4.0)
    );
    assert!(
        matches!(evaluate(&parse("7 % 4").unwrap(), 0, &data), Ok(Data::Scalar(value)) if value == 3.0)
    );
}

fn buckets(job: &str, counts: &[(&str, f64)], time_ms: i64) -> Vec<Series> {
    counts
        .iter()
        .map(|(le, count)| {
            series(
                &[
                    ("__name__", "http_request_duration_seconds_bucket"),
                    ("job", job),
                    ("le", *le),
                ],
                &[(time_ms, *count)],
            )
        })
        .collect()
}

#[test]
fn histogram_quantile_interpolates_classic_buckets_as_prometheus_does() {
    let mut all = buckets(
        "api",
        &[("0.1", 50.0), ("0.5", 80.0), ("1", 95.0), ("+Inf", 100.0)],
        30_000,
    );
    all.extend(buckets("unbounded", &[("0.1", 5.0), ("1", 10.0)], 30_000));
    all.extend(buckets("empty", &[("1", 0.0), ("+Inf", 0.0)], 30_000));
    all.extend(buckets(
        "negative",
        &[("-1", 10.0), ("1", 20.0), ("+Inf", 20.0)],
        30_000,
    ));
    all.extend(buckets(
        "unsorted",
        &[("+Inf", 100.0), ("1", 95.0), ("0.5", 80.0), ("0.1", 50.0)],
        30_000,
    ));
    all.extend(buckets(
        "dipping",
        &[("0.1", 50.0), ("0.5", 40.0), ("1", 95.0), ("+Inf", 100.0)],
        30_000,
    ));
    let data = fetched(all);
    let quantile = |query: &str, job: &str| {
        sorted(query, 60_000, &data)
            .into_iter()
            .find(|(found, _)| found == &labels(&[("job", job)]))
            .map(|(_, value)| value)
            .unwrap_or_else(|| panic!("{query}: no {job}"))
    };
    let at_90 = "histogram_quantile(0.9, http_request_duration_seconds_bucket)";
    assert!(close(quantile(at_90, "api"), 0.5 + 0.5 * (10.0 / 15.0)));
    assert!(close(quantile(at_90, "unsorted"), quantile(at_90, "api")));
    assert!(close(
        quantile(
            "histogram_quantile(0.25, http_request_duration_seconds_bucket)",
            "api"
        ),
        0.05
    ));
    assert_eq!(
        quantile(
            "histogram_quantile(0.99, http_request_duration_seconds_bucket)",
            "api"
        ),
        1.0
    );
    assert!(quantile(at_90, "unbounded").is_nan());
    assert!(quantile(at_90, "empty").is_nan());
    assert_eq!(
        quantile(
            "histogram_quantile(0.1, http_request_duration_seconds_bucket)",
            "negative"
        ),
        -1.0
    );
    assert!(close(
        quantile(
            "histogram_quantile(0.5, http_request_duration_seconds_bucket)",
            "dipping"
        ),
        0.1
    ));
    assert_eq!(
        quantile(
            "histogram_quantile(1.5, http_request_duration_seconds_bucket)",
            "api"
        ),
        f64::INFINITY
    );
    assert_eq!(
        quantile(
            "histogram_quantile(-0.5, http_request_duration_seconds_bucket)",
            "api"
        ),
        f64::NEG_INFINITY
    );
    assert!(
        quantile(
            "histogram_quantile(scalar(vector(0) / 0), http_request_duration_seconds_bucket)",
            "api"
        )
        .is_nan()
    );

    let mut counters = Vec::new();
    for (le, total) in [("0.1", 50.0), ("0.5", 80.0), ("1", 95.0), ("+Inf", 100.0)] {
        counters.push(series(
            &[
                ("__name__", "http_request_duration_seconds_bucket"),
                ("job", "api"),
                ("le", le),
            ],
            &[(60_000, 0.0), (600_000, total)],
        ));
    }
    let rated = fetched(counters);
    let rows = sorted(
        "histogram_quantile(0.9, sum by (job, le) (rate(http_request_duration_seconds_bucket[10m])))",
        600_000,
        &rated,
    );
    assert_eq!(rows.len(), 1);
    assert!(close(rows[0].1, 0.5 + 0.5 * (10.0 / 15.0)), "{rows:?}");
    assert!(
        failure(
            "histogram_quantile(http_request_duration_seconds_bucket)",
            600_000,
            &rated
        )
        .contains("wrong number of arguments")
    );
}

#[test]
fn label_replace_and_label_join_answer_the_prometheus_docs_examples() {
    let data = fetched(vec![
        series(
            &[
                ("__name__", "up"),
                ("job", "api-server"),
                ("service", "a:c"),
            ],
            &[(30_000, 1.0)],
        ),
        series(
            &[("__name__", "up"), ("job", "other"), ("service", "a:c")],
            &[(30_000, 1.0)],
        ),
        series(
            &[
                ("__name__", "up"),
                ("job", "api-server"),
                ("src1", "a"),
                ("src2", "b"),
                ("src3", "c"),
            ],
            &[(30_000, 1.0)],
        ),
    ]);
    let at = |query: &str| sorted(query, 60_000, &data);
    assert_eq!(
        at(
            "label_replace(up{job=\"api-server\",service=\"a:c\"}, \"foo\", \"$1\", \"service\", \"(.*):.*\")"
        ),
        [(
            labels(&[
                ("__name__", "up"),
                ("foo", "a"),
                ("job", "api-server"),
                ("service", "a:c")
            ]),
            1.0
        )]
    );
    assert_eq!(
        at(
            "label_replace(up{job=\"api-server\",service=\"a:c\"}, \"foo\", \"$1\", \"service\", \"a\")"
        ),
        [(
            labels(&[
                ("__name__", "up"),
                ("job", "api-server"),
                ("service", "a:c")
            ]),
            1.0
        )]
    );
    assert_eq!(
        at("label_replace(up{job=\"api-server\",service=\"a:c\"}, \"service\", \"${2}-$1$$\", \"service\", \"(.*):(.*)\")")[0].0,
        labels(&[("__name__", "up"), ("job", "api-server"), ("service", "c-a$")])
    );
    assert_eq!(
        at("label_replace(up{job=\"other\"}, \"service\", \"\", \"service\", \".*\")")[0].0,
        labels(&[("__name__", "up"), ("job", "other")])
    );
    assert_eq!(
        at("label_join(up{job=\"api-server\",src1=\"a\",src2=\"b\",src3=\"c\"}, \"foo\", \",\", \"src1\", \"src2\", \"src3\")")[0].0,
        labels(&[
            ("__name__", "up"),
            ("foo", "a,b,c"),
            ("job", "api-server"),
            ("src1", "a"),
            ("src2", "b"),
            ("src3", "c"),
        ])
    );
    for (broken, expected) in [
        (
            "label_replace(up, \"1foo\", \"$1\", \"job\", \"(.*)\")",
            "invalid destination label name",
        ),
        (
            "label_replace(up, \"foo\", \"$1\", \"job\", \"(.\")",
            "invalid regular expression",
        ),
        (
            "label_replace(up{service=\"a:c\"}, \"job\", \"same\", \"job\", \".*\")",
            "same labelset",
        ),
        (
            "label_join(up, \"foo\", \",\", \"1src\")",
            "invalid source label name",
        ),
        (
            "label_replace(up, \"foo\", \"$1\", \"job\")",
            "wrong number of arguments",
        ),
        ("label_join(up, \"foo\", 1)", "expected type string"),
    ] {
        let message = failure(broken, 60_000, &data);
        assert!(message.contains(expected), "{broken}: {message}");
    }
}

#[test]
fn replacement_templates_expand_as_go_regexp_expand_does() {
    let groups = [Some("a:c".to_owned()), Some("a".to_owned()), None];
    let expand = |template: &str, groups: &[Option<String>]| {
        crate::promql::expand(template, groups, usize::MAX).expect("no limit")
    };
    assert_eq!(expand("$1", &groups), "a");
    assert_eq!(expand("${1}x", &groups), "ax");
    assert_eq!(expand("$1x", &groups), "");
    assert_eq!(expand("$2", &groups), "");
    assert_eq!(expand("$9", &groups), "");
    assert_eq!(expand("$$1", &groups), "$1");
    assert_eq!(expand("cost: $", &groups), "cost: $");
    assert_eq!(expand("${", &groups), "${");
    assert_eq!(expand("$0", &groups), "a:c");
    assert_eq!(
        crate::promql::expand("$1$1$1", &groups, 2),
        None,
        "a limit stops the expansion"
    );
    assert_eq!(
        crate::promql::expand("$1$1", &groups, 2).as_deref(),
        Some("aa")
    );
}

fn counter() -> Fetched {
    let samples: Vec<(i64, f64)> = (1..=120)
        .map(|step| (step * 15_000, (step * 15) as f64))
        .collect();
    fetched(vec![series(&[("__name__", "x"), ("job", "a")], &samples)])
}

#[test]
fn subqueries_evaluate_at_aligned_steps_inside_their_range() {
    let data = counter();
    let end = 1_800_000;
    let matrix = match evaluate(&parse("rate(x[1m])[30m:1m]").unwrap(), end, &data).unwrap() {
        Data::Matrix(matrix) => matrix,
        other => panic!("{other:?}"),
    };
    assert_eq!(matrix.len(), 1);
    let points = &matrix[0].1;
    assert_eq!(points.len(), 30);
    assert_eq!(points.first().map(|(time, _)| *time), Some(60_000));
    assert_eq!(points.last().map(|(time, _)| *time), Some(end));
    assert!(
        points.iter().all(|(_, value)| close(*value, 1.0)),
        "{points:?}"
    );
    assert_eq!(sorted("max_over_time(x[10m:1m])", end, &data)[0].1, 1800.0);
    assert_eq!(sorted("min_over_time(x[10m:1m])", end, &data)[0].1, 1260.0);
    assert_eq!(sorted("count_over_time(x[5m:])", end, &data)[0].1, 5.0);
    assert_eq!(
        sorted("last_over_time(x[5m:1m] offset 10m)", end, &data)[0].1,
        1200.0
    );
    assert!(close(
        sorted("max_over_time(rate(x[1m])[5m:1m])", end, &data)[0].1,
        1.0
    ));
    assert!(close(
        sorted("max_over_time(max_over_time(x[2m:30s])[5m:1m])", end, &data)[0].1,
        1800.0
    ));
    assert!(
        failure("x[5m][10m:1m]", end, &data).contains("subquery is only allowed on instant vector")
    );
    assert!(failure("max_over_time(1[5m:1m])", end, &data).contains("got scalar"));
    assert!(
        failure("max_over_time(x[1y:1ms])", end, &data)
            .contains("subqueries would evaluate more than")
    );
    assert!(parse("x[5m:0s]").is_err());
}

#[test]
fn selectors_without_a_metric_name_need_one_matcher_that_refuses_the_empty_value() {
    let data = fetched(vec![
        series(&[("__name__", "a"), ("job", "x")], &[(30_000, 1.0)]),
        series(&[("__name__", "b"), ("job", "x")], &[(30_000, 2.0)]),
        series(&[("__name__", "c"), ("job", "y")], &[(30_000, 3.0)]),
        series(&[("__name__", "on"), ("job", "z")], &[(30_000, 4.0)]),
    ]);
    let at = |query: &str| sorted(query, 60_000, &data);
    assert_eq!(values(&at("{job=\"x\"}")), [1.0, 2.0]);
    assert_eq!(
        at("{job=\"x\"}")[0].0,
        labels(&[("__name__", "a"), ("job", "x")])
    );
    assert_eq!(values(&at("{__name__=~\"a|b\"}")), [1.0, 2.0]);
    assert_eq!(values(&at("{__name__=\"c\"}")), [3.0]);
    assert_eq!(values(&at("{job=~\".+\"}")).len(), 4);
    assert_eq!(values(&at("{job=~\".*\", __name__=\"a\"}")), [1.0]);
    assert_eq!(values(&at("{__name__=\"on\"}")), [4.0]);
    assert_eq!(values(&at("sum({job=~\"x|y\"})")), [6.0]);
    for (broken, expected) in [
        (
            "{job=~\".*\"}",
            "vector selector must contain at least one non-empty matcher",
        ),
        (
            "{}",
            "vector selector must contain at least one non-empty matcher",
        ),
        (
            "{job=\"\"}",
            "vector selector must contain at least one non-empty matcher",
        ),
        (
            "{job!=\"x\"}",
            "vector selector must contain at least one non-empty matcher",
        ),
        ("a{__name__=\"b\"}", "metric name must not be set twice"),
        ("on{}", "unexpected keyword"),
        ("{job=~\"(\"}", "error parsing regexp"),
    ] {
        let message = failure(broken, 60_000, &data);
        assert!(message.contains(expected), "{broken}: {message}");
    }
}

#[test]
fn a_query_past_its_samples_limit_or_the_memory_budget_is_refused() {
    let data = counter();
    let expr = parse("{job=\"a\"}[30m]").unwrap();
    let roomy = MemoryBudget::new(usize::MAX);
    let refused = Evaluator::new(&data, Work::new(&roomy, 100)).evaluate(&expr, 1_800_000);
    assert_eq!(
        refused,
        Err(Failure::Execution(
            "query processing would load too many samples into memory in query execution"
                .to_owned()
        ))
    );
    assert!(
        Evaluator::new(&data, Work::new(&roomy, 120))
            .evaluate(&expr, 1_800_000)
            .is_ok()
    );
    let wide: Vec<(i64, f64)> = (1..=10_000).map(|step| (step, step as f64)).collect();
    let big = fetched(vec![series(&[("__name__", "x")], &wide)]);
    let empty = MemoryBudget::new(0);
    let over_budget = Evaluator::new(&big, Work::new(&empty, usize::MAX))
        .evaluate(&parse("x[1h]").unwrap(), 10_000);
    assert!(
        matches!(over_budget, Err(Failure::Unavailable(_))),
        "{over_budget:?}"
    );
    assert_eq!(empty.used_bytes(), 0);
}

#[test]
fn nested_label_join_and_label_replace_stop_before_a_huge_label_value() {
    let data = Fetched::new();
    let wide_separator = "x".repeat(1000);
    let missing_sources = vec!["\"b\""; 1000].join(",");
    let repeat_a = |count: usize| vec!["\"a\""; count].join(",");
    let joined = format!(
        "count(label_join(label_join(label_join(vector(1),\"a\",\"{wide_separator}\",{missing_sources}),\"a\",\"\",{}),\"c\",\"\",{}))",
        repeat_a(200),
        repeat_a(20)
    );
    let started = std::time::Instant::now();
    let message = failure(&joined, 60_000, &data);
    assert!(
        message.contains("label_join() would build a label value longer than"),
        "{message}"
    );
    let one_level = format!("label_join(vector(1),\"a\",\"{wide_separator}\",{missing_sources})");
    let rows = sorted(&one_level, 60_000, &data);
    assert_eq!(rows[0].0[0].1.len(), 999 * 1000);
    let replaced = format!(
        "label_replace(label_replace(vector(1),\"a\",\"{wide_separator}\",\"\",\"\"),\"a\",\"{}\",\"a\",\"(.*)\")",
        "$1".repeat(2000)
    );
    let message = failure(&replaced, 60_000, &data);
    assert!(
        message.contains("label_replace() would build a label value longer than"),
        "{message}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let tight = MemoryBudget::new(256 * 1024);
    let expr = parse(&one_level).unwrap();
    let refused = Evaluator::new(&data, Work::new(&tight, usize::MAX)).evaluate(&expr, 60_000);
    assert!(
        matches!(refused, Err(Failure::Unavailable(_))),
        "{refused:?}"
    );
}

#[test]
fn quantile_orders_nan_first_as_go_does() {
    let nan_one_two = "quantile({q}, label_replace(vector(0) / 0, \"k\", \"n\", \"\", \"\") or label_replace(vector(1), \"k\", \"one\", \"\", \"\") or label_replace(vector(2), \"k\", \"two\", \"\", \"\"))";
    let data = Fetched::new();
    let lowest = sorted(&nan_one_two.replace("{q}", "0"), 60_000, &data);
    assert!(lowest[0].1.is_nan(), "{lowest:?}");
    let highest = sorted(&nan_one_two.replace("{q}", "1"), 60_000, &data);
    assert_eq!(highest[0].1, 2.0);
}

fn encoded(query: &str) -> String {
    query
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[test]
fn the_query_api_answers_new_promql_and_its_errors_in_prometheus_shapes() {
    let store = MemoryStore::default();
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb");
    let lines = "cpu,host=a usage=10,idle=90 1000\ncpu,host=b usage=20,idle=80 1000\n";
    assert_eq!(
        post(&endpoint, "/api/v2/write", "precision=ms", lines.as_bytes()).0,
        204
    );
    let query = |text: &str| {
        post(
            &endpoint,
            "/api/v1/query",
            &format!("query={}&time=61", encoded(text)),
            b"",
        )
    };
    let (status, answer) = query("{host=\"a\"}");
    assert_eq!(status, 200, "{answer}");
    assert!(
        answer.contains("\"__name__\":\"cpu_usage\"")
            && answer.contains("\"__name__\":\"cpu_idle\""),
        "{answer}"
    );
    let (status, answer) = query("topk(1, cpu_usage)");
    assert_eq!(status, 200, "{answer}");
    assert!(
        answer.contains("\"host\":\"b\"") && answer.contains("\"20\""),
        "{answer}"
    );
    let (_, answer) = query("cpu_usage / on(host) cpu_idle");
    assert!(answer.contains("\"value\":[61,\"0.25\"]"), "{answer}");
    let (_, answer) = query("max_over_time(cpu_usage[5m:1m])");
    assert!(answer.contains("\"20\""), "{answer}");
    let (status, answer) = query("1 > 2");
    assert_eq!(status, 400);
    assert!(
        answer.starts_with("{\"status\":\"error\",\"errorType\":\"bad_data\",\"error\":")
            && answer.contains("BOOL"),
        "{answer}"
    );
    let (status, answer) = query("cpu_usage / ignoring(host) cpu_idle");
    assert_eq!(status, 422, "{answer}");
    assert!(answer.contains("\"errorType\":\"execution\""), "{answer}");
    let (status, answer) = query("{host=~\".*\"}");
    assert_eq!(status, 400, "{answer}");
    let (status, series) = post(
        &endpoint,
        "/api/v1/series",
        &format!("match[]={}&start=0&end=61", encoded("{host=\"a\"}")),
        b"",
    );
    assert_eq!(status, 200, "{series}");
    assert!(
        series.contains("cpu_usage") && series.contains("cpu_idle") && !series.contains("\"b\""),
        "{series}"
    );
    let limited = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb")
        .with_max_samples(1);
    let (status, answer) = post(
        &limited,
        "/api/v1/query",
        &format!("query={}&time=61", encoded("cpu_usage[5m]")),
        b"",
    );
    assert_eq!(status, 422, "{answer}");
    assert!(answer.contains("too many samples"), "{answer}");
}

#[test]
fn subquery_steps_give_back_their_samples_after_each_step() {
    let data = counter();
    let expr = parse("max_over_time(rate(x[10m])[30m:1m])").unwrap();
    let roomy = MemoryBudget::new(usize::MAX);
    let answered = Evaluator::new(&data, Work::new(&roomy, 100)).evaluate(&expr, 1_800_000);
    assert!(
        matches!(&answered, Ok(Data::Vector(vector)) if vector.len() == 1),
        "{answered:?}"
    );
    let one_step_too_many = Evaluator::new(&data, Work::new(&roomy, 30)).evaluate(
        &parse("max_over_time(rate(x[10m])[30m:1m])").unwrap(),
        1_800_000,
    );
    assert_eq!(
        one_step_too_many,
        Err(Failure::Execution(
            "query processing would load too many samples into memory in query execution"
                .to_owned()
        ))
    );
}

#[test]
fn results_with_the_same_labels_once_the_name_is_dropped_are_refused() {
    let data = fetched(vec![
        series(
            &[("__name__", "http_requests_total"), ("job", "api")],
            &[(30_000, 1.0), (60_000, 3.0)],
        ),
        series(
            &[("__name__", "http_errors_total"), ("job", "api")],
            &[(30_000, 1.0), (60_000, 2.0)],
        ),
        series(
            &[("__name__", "info"), ("job", "api"), ("owner", "web")],
            &[(30_000, 1.0), (60_000, 1.0)],
        ),
    ]);
    for query in [
        "rate({__name__=~\"http_requests_total|http_errors_total\", job=\"api\"}[5m])",
        "{__name__=~\"http_.*\", job=\"api\"} * 2",
        "2 * {__name__=~\"http_.*\"}",
        "abs({__name__=~\"http_.*\"})",
    ] {
        let message = failure(query, 60_000, &data);
        assert!(
            message.contains("vector cannot contain metrics with the same labelset"),
            "{query}: {message}"
        );
    }
    assert_eq!(
        values(&sorted("{__name__=~\"http_.*\"} > 1", 60_000, &data)),
        [2.0, 3.0]
    );
    assert_eq!(
        failure(
            "{__name__=~\"http_.*\"} * on(job) group_left(owner) info",
            60_000,
            &data
        ),
        "multiple matches for labels: grouping labels must ensure unique matches"
    );
    assert_eq!(
        failure("topk(1e19, info)", 60_000, &data),
        "Scalar value 10000000000000000000 overflows int64"
    );
}

#[test]
fn the_remaining_functions_follow_prometheus() {
    use crate::promql::functions::{date_function, range_function, regression, scalar_function};
    let samples = [(0, 1.0), (60_000, 3.0), (120_000, 2.0), (180_000, 5.0)];
    assert_eq!(range_function("changes", &samples, 0, 180_000).unwrap(), Some(3.0));
    assert_eq!(range_function("resets", &samples, 0, 180_000).unwrap(), Some(1.0));
    assert_eq!(range_function("idelta", &samples, 0, 180_000).unwrap(), Some(3.0));
    assert_eq!(range_function("present_over_time", &samples, 0, 180_000).unwrap(), Some(1.0));
    let line = [(0, 0.0), (10_000, 10.0), (20_000, 20.0)];
    let (slope, intercept) = regression(&line, 20_000).unwrap();
    assert!((slope - 1.0).abs() < 1e-9 && (intercept - 20.0).abs() < 1e-9);
    assert_eq!(date_function("year", 951_782_400.0), Some(2000.0));
    assert_eq!(date_function("days_in_month", 951_782_400.0), Some(29.0));
    assert_eq!(date_function("day_of_week", 0.0), Some(4.0));
    assert_eq!(scalar_function("sgn", -3.0), Some(-1.0));
    assert_eq!(scalar_function("deg", std::f64::consts::PI), Some(180.0));
}

#[test]
fn the_at_modifier_negative_offsets_and_limits_follow_prometheus() {
    let data = http_requests();
    let total = |query: &str, time_ms: i64| values(&sorted(query, time_ms, &data));
    assert_eq!(total("sum(http_requests @ 30)", 1_000_000), vec![3600.0]);
    assert_eq!(total("sum(http_requests offset 0s @ 30)", 1_000_000), vec![3600.0]);
    assert!(total("sum(http_requests)", 1_000_000).is_empty());
    assert_eq!(total("sum(http_requests offset -20s)", 10_000), vec![3600.0]);
    assert!(total("sum(http_requests)", 10_000).is_empty());
    assert_eq!(total("count(limitk(2, http_requests))", 60_000), vec![2.0]);
    assert_eq!(
        total("count by (job) (limitk by (job) (1, http_requests))", 60_000),
        vec![1.0, 1.0]
    );
    assert_eq!(total("count(limit_ratio(1, http_requests))", 60_000), vec![8.0]);
    let half = total("count(limit_ratio(0.5, http_requests))", 60_000).iter().sum::<f64>();
    let rest = total("count(limit_ratio(-0.5, http_requests))", 60_000).iter().sum::<f64>();
    assert_eq!(half + rest, 8.0);
    assert!(parse("http_requests @ foo").is_err());
    assert!(failure("limit_ratio(2, http_requests)", 60_000, &data).contains("ratio"));
}

#[test]
fn label_replace_expands_named_capture_groups() {
    let (plain, names) = crate::promql::functions::named_groups("(a)(?:b)(?P<x>c)[(](?<y>d)(?<=e)");
    assert_eq!(plain, "(a)(?:b)(c)[(](d)(?<=e)");
    assert_eq!(names, vec![("x".to_owned(), 2), ("y".to_owned(), 3)]);
    let data = fetched(vec![series(
        &[("__name__", "up"), ("instance", "host-1:9100")],
        &[(30_000, 1.0)],
    )]);
    let rows = sorted(
        r#"label_replace(up, "host", "$name-${port}", "instance", "(?P<name>[^:]+):(?<port>.*)")"#,
        60_000,
        &data,
    );
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].0.contains(&("host".to_owned(), "host-1-9100".to_owned())),
        "{rows:?}"
    );
}

#[test]
fn a_query_past_its_time_limit_stops_with_a_timeout() {
    let data = http_requests();
    let roomy = MemoryBudget::new(usize::MAX);
    let expr = parse("max_over_time(sum(http_requests)[10m:1s])").unwrap();
    let stopped = Evaluator::new(&data, Work::new(&roomy, usize::MAX).with_timeout(std::time::Duration::ZERO))
        .evaluate(&expr, 60_000);
    assert!(matches!(stopped, Err(Failure::Timeout(_))), "{stopped:?}");
}
