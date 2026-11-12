use crate::Endpoint;
use crate::line_protocol::{FieldValue, parse as parse_lines};
use crate::promql::{Data, Evaluator, Expr, Work, parse};
use crate::store::Series;
use eggdb_http::Request;
use eggdb_platform::connection_limits::MemoryBudget;
use eggdb_sql::{
    EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, SqlResult,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

type Map = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Default)]
pub(crate) struct MemoryStore {
    committed: Mutex<Arc<Map>>,
}

pub(crate) struct Snapshot(Arc<Map>);

pub(crate) struct Writer<'s> {
    store: &'s MemoryStore,
    map: Map,
}

fn scan(
    map: &Map,
    start: &[u8],
    end: &[u8],
    direction: ScanDirection,
    visit: &mut EntryVisitor<'_>,
) -> SqlResult<()> {
    if start >= end {
        return Ok(());
    }
    let range = map.range(start.to_vec()..end.to_vec());
    let entries: Box<dyn Iterator<Item = (&Vec<u8>, &Vec<u8>)>> = match direction {
        ScanDirection::Forward => Box::new(range),
        ScanDirection::Backward => Box::new(range.rev()),
    };
    for (key, value) in entries {
        if !visit(key, value)? {
            break;
        }
    }
    Ok(())
}

impl KeyValueRead for Snapshot {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        Ok(self.0.get(key).cloned())
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        scan(&self.0, start, end, direction, visit)
    }
}

impl KeyValueRead for Writer<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        Ok(self.map.get(key).cloned())
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        scan(&self.map, start, end, direction, visit)
    }
}

impl KeyValueWrite for Writer<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.map.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        Ok(self.map.remove(key).is_some())
    }

    fn commit(self) -> SqlResult<()> {
        *self.store.committed.lock().unwrap() = Arc::new(self.map);
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        Ok(())
    }
}

impl KeyValueStore for MemoryStore {
    type Read<'s> = Snapshot;
    type Write<'s> = Writer<'s>;

    fn begin_read(&self) -> SqlResult<Snapshot> {
        Ok(Snapshot(Arc::clone(&self.committed.lock().unwrap())))
    }

    fn begin_write(&self) -> SqlResult<Writer<'_>> {
        let map = self.committed.lock().unwrap().as_ref().clone();
        Ok(Writer { store: self, map })
    }
}

#[test]
fn line_protocol_reads_escapes_types_and_timestamps() {
    let points = parse_lines(
        "# comment\nweather,location=us\\ west,kind=a\\,b temperature=82,humidity=43i,ok=t,note=\"say \\\"hi\\\"\" 1465839830100400200\nmy\\ measure value=1u\n",
        1,
        7,
    )
    .unwrap();
    assert_eq!(points[0].measurement, "weather");
    assert_eq!(
        points[0].tags,
        [
            ("location".to_owned(), "us west".to_owned()),
            ("kind".to_owned(), "a,b".to_owned())
        ]
    );
    assert_eq!(
        points[0].fields[0],
        ("temperature".to_owned(), FieldValue::Float(82.0))
    );
    assert_eq!(points[0].fields[1].1, FieldValue::Integer(43));
    assert_eq!(points[0].fields[2].1, FieldValue::Boolean(true));
    assert_eq!(
        points[0].fields[3].1,
        FieldValue::Text("say \"hi\"".to_owned())
    );
    assert_eq!(points[0].time_ns, 1_465_839_830_100_400_200);
    assert_eq!(
        (
            points[1].measurement.as_str(),
            points[1].time_ns,
            &points[1].fields[0].1
        ),
        ("my measure", 7, &FieldValue::Unsigned(1))
    );
    for broken in [
        "cpu",
        "cpu value=",
        "cpu value=nan",
        "cpu value=1 notatime",
        "cpu,host value=1",
        "cpu time=1",
        "cpu value=\"open",
    ] {
        assert!(parse_lines(broken, 1, 0).is_err(), "{broken}");
    }
}

fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn length_delimited(field: u64, bytes: &[u8], out: &mut Vec<u8>) {
    varint(field << 3 | 2, out);
    varint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn label(name: &str, value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    length_delimited(1, name.as_bytes(), &mut out);
    length_delimited(2, value.as_bytes(), &mut out);
    out
}

fn sample(value: f64, time_ms: i64) -> Vec<u8> {
    let mut out = vec![1 << 3 | 1];
    out.extend_from_slice(&value.to_bits().to_le_bytes());
    varint(2 << 3, &mut out);
    varint(time_ms as u64, &mut out);
    out
}

type Samples<'a> = (&'a [(&'a str, &'a str)], &'a [(f64, i64)]);

fn remote_write_body(series: &[Samples<'_>]) -> Vec<u8> {
    let mut request = Vec::new();
    for (labels, samples) in series {
        let mut encoded = Vec::new();
        for (name, value) in *labels {
            length_delimited(1, &label(name, value), &mut encoded);
        }
        for (value, time) in *samples {
            length_delimited(2, &sample(*value, *time), &mut encoded);
        }
        length_delimited(1, &encoded, &mut request);
    }
    eggdb_columnar::snappy_compress(&request)
}

#[test]
fn remote_write_requests_decode_into_points() {
    let body = remote_write_body(&[(
        &[("__name__", "up"), ("job", "api")],
        &[(1.0, 1000), (0.0, 2000)],
    )]);
    let points = crate::remote_write::decode(&body).unwrap();
    assert_eq!(points.len(), 2);
    assert_eq!(
        (
            points[1].measurement.as_str(),
            &points[1].tags,
            points[1].time_ns
        ),
        (
            "up",
            &vec![("job".to_owned(), "api".to_owned())],
            2_000_000_000
        )
    );
    assert!(crate::remote_write::decode(b"not snappy").is_err());
    assert!(
        crate::remote_write::decode(&remote_write_body(&[(&[("job", "x")], &[(1.0, 1)])])).is_err()
    );
}

#[test]
fn remote_write_two_resolves_symbols_and_splits_histograms() {
    let mut series = Vec::new();
    let mut references = Vec::new();
    for reference in [1u64, 2, 3, 4] {
        varint(reference, &mut references);
    }
    length_delimited(1, &references, &mut series);
    length_delimited(2, &sample(7.5, 3000), &mut series);
    let mut histogram = Vec::new();
    varint(1 << 3, &mut histogram);
    varint(12, &mut histogram);
    histogram.push(3 << 3 | 1);
    histogram.extend_from_slice(&42.0f64.to_bits().to_le_bytes());
    varint(15 << 3, &mut histogram);
    varint(4000, &mut histogram);
    length_delimited(3, &histogram, &mut series);
    let mut request = Vec::new();
    for symbol in ["", "__name__", "latency", "job", "api"] {
        length_delimited(4, symbol.as_bytes(), &mut request);
    }
    length_delimited(5, &series, &mut request);
    let (points, written) =
        crate::remote_write::decode_v2(&eggdb_columnar::snappy_compress(&request)).unwrap();
    assert_eq!((written.samples, written.histograms), (1, 1));
    let names: Vec<&str> = points.iter().map(|point| point.measurement.as_str()).collect();
    assert_eq!(names, vec!["latency", "latency_count", "latency_sum", "latency_bucket", "latency_bucket"]);
    assert_eq!(points[0].tags, vec![("job".to_owned(), "api".to_owned())]);
    assert_eq!(points[2].time_ns, 4_000_000_000);
}

#[test]
fn remote_write_two_expands_native_buckets_and_keeps_exemplars() {
    let mut series = Vec::new();
    let mut references = Vec::new();
    for reference in [1u64, 2] {
        varint(reference, &mut references);
    }
    length_delimited(1, &references, &mut series);
    let mut histogram = Vec::new();
    varint(1 << 3, &mut histogram);
    varint(5, &mut histogram);
    let mut positive_span = Vec::new();
    varint(2 << 3, &mut positive_span);
    varint(2, &mut positive_span);
    length_delimited(11, &positive_span, &mut histogram);
    let mut deltas = Vec::new();
    varint(6, &mut deltas);
    varint(1, &mut deltas);
    length_delimited(12, &deltas, &mut histogram);
    varint(15 << 3, &mut histogram);
    varint(1000, &mut histogram);
    length_delimited(3, &histogram, &mut series);
    let mut exemplar = Vec::new();
    let mut exemplar_labels = Vec::new();
    for reference in [3u64, 4] {
        varint(reference, &mut exemplar_labels);
    }
    length_delimited(1, &exemplar_labels, &mut exemplar);
    exemplar.push(2 << 3 | 1);
    exemplar.extend_from_slice(&0.25f64.to_bits().to_le_bytes());
    varint(3 << 3, &mut exemplar);
    varint(1000, &mut exemplar);
    length_delimited(4, &exemplar, &mut series);
    let mut request = Vec::new();
    for symbol in ["", "__name__", "latency", "trace_id", "abc"] {
        length_delimited(4, symbol.as_bytes(), &mut request);
    }
    length_delimited(5, &series, &mut request);
    let (points, written) =
        crate::remote_write::decode_v2(&eggdb_columnar::snappy_compress(&request)).unwrap();
    assert_eq!((written.histograms, written.exemplars), (1, 1));
    let buckets: Vec<(String, f64)> = points
        .iter()
        .filter(|point| point.measurement == "latency_bucket")
        .map(|point| {
            let FieldValue::Float(value) = point.fields[0].1 else {
                panic!("a float");
            };
            (point.tags[0].1.clone(), value)
        })
        .collect();
    assert_eq!(
        buckets,
        vec![("0".to_owned(), 0.0), ("1".to_owned(), 3.0), ("2".to_owned(), 5.0), ("+Inf".to_owned(), 5.0)]
    );
    let exemplar = points.iter().find(|point| point.measurement == "latency_exemplars").unwrap();
    assert_eq!(exemplar.fields[1], ("exemplar_trace_id".to_owned(), FieldValue::Text("abc".to_owned())));
}

pub(crate) fn evaluate(
    expr: &Expr,
    time_ms: i64,
    data: &BTreeMap<String, Vec<Series>>,
) -> Result<Data, String> {
    let budget = MemoryBudget::new(usize::MAX);
    Evaluator::new(data, Work::new(&budget, usize::MAX))
        .evaluate(expr, time_ms)
        .map_err(|failure| failure.message().to_owned())
}

pub(crate) fn series(labels: &[(&str, &str)], samples: &[(i64, f64)]) -> Series {
    Series {
        labels: labels
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        samples: samples.to_vec(),
    }
}

pub(crate) fn vector(data: Data) -> Vec<(Vec<(String, String)>, f64)> {
    match data {
        Data::Vector(vector) => vector,
        other => panic!("{other:?}"),
    }
}

#[test]
fn promql_selects_rates_aggregates_and_combines() {
    let requests = vec![
        series(
            &[("__name__", "requests"), ("code", "200"), ("job", "api")],
            &[
                (0, 0.0),
                (15_000, 15.0),
                (30_000, 30.0),
                (45_000, 45.0),
                (60_000, 60.0),
            ],
        ),
        series(
            &[("__name__", "requests"), ("code", "500"), ("job", "api")],
            &[(0, 100.0), (30_000, 10.0), (60_000, 40.0)],
        ),
    ];
    let data = BTreeMap::from([("requests".to_owned(), requests)]);
    let at = |query: &str| vector(evaluate(&parse(query).unwrap(), 60_000, &data).unwrap());
    let rates = at("rate(requests{job=\"api\"}[1m])");
    assert_eq!(rates.len(), 2);
    assert!((rates[0].1 - 1.0).abs() < 1e-9, "{rates:?}");
    assert!(!rates[0].0.iter().any(|(name, _)| name == "__name__"));
    assert!((at("increase(requests{code=\"500\"}[1m])")[0].1 - 40.0).abs() < 1e-9);
    assert_eq!(
        at("sum by (job) (requests)"),
        [(vec![("job".to_owned(), "api".to_owned())], 100.0)]
    );
    assert_eq!(at("requests{code=~\"5..\"}")[0].1, 40.0);
    assert_eq!(at("requests{code!~\"5..\"} * 2 + 1")[0].1, 121.0);
    assert_eq!(at("requests > 50").len(), 1);
    assert_eq!(
        at("max_over_time(requests[1m]) - min_over_time(requests[1m])")
            .iter()
            .map(|(_, value)| *value)
            .collect::<Vec<_>>(),
        [45.0, 30.0]
    );
    assert_eq!(at("count(requests offset 1m)")[0].1, 2.0);
    assert!(
        matches!(evaluate(&parse("1 + 2 * 3").unwrap(), 0, &data).unwrap(), Data::Scalar(value) if value == 7.0)
    );
    for broken in [
        "rate(requests)",
        "sum(",
        "requests{code=200}",
        "requests[5x]",
        "{job=~\".*\"}",
    ] {
        assert!(
            parse(broken)
                .and_then(|expr| evaluate(&expr, 0, &data))
                .is_err(),
            "{broken}"
        );
    }
}

pub(crate) fn post(
    endpoint: &Endpoint<'_, MemoryStore>,
    path: &str,
    query: &str,
    body: &[u8],
) -> (u16, String) {
    let request = Request {
        method: if body.is_empty() { "GET" } else { "POST" }.to_owned(),
        path: path.to_owned(),
        query: Some(query.to_owned()),
        headers: vec![("authorization".to_owned(), "Token secret".to_owned())],
        body: body.to_vec(),
    };
    let response = endpoint.answer(&request);
    (response.status, String::from_utf8(response.body).unwrap())
}

#[test]
fn written_points_are_queried_with_sql_and_promql() {
    let store = MemoryStore::default();
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb");
    let lines =
        "cpu,host=a usage=10 1000\ncpu,host=b usage=20 1000\ncpu,host=a usage=30,idle=70i 61000\n";
    assert_eq!(
        post(
            &endpoint,
            "/api/v2/write",
            "org=x&bucket=y&precision=ms",
            lines.as_bytes()
        )
        .0,
        204
    );
    let body = remote_write_body(&[(&[("__name__", "up"), ("job", "db")], &[(1.0, 61_000)])]);
    assert_eq!(post(&endpoint, "/api/v1/write", "", &body).0, 204);
    let (status, answer) = post(
        &endpoint,
        "/api/v1/query",
        "query=sum(cpu_usage)&time=61",
        b"",
    );
    assert_eq!(status, 200, "{answer}");
    assert!(answer.contains("\"value\":[61,\"50\"]"), "{answer}");
    let (_, answer) = post(
        &endpoint,
        "/api/v1/query",
        "query=up%7Bjob%3D%22db%22%7D&time=61",
        b"",
    );
    assert!(
        answer.contains("\"__name__\":\"up\"") && answer.contains("\"job\":\"db\""),
        "{answer}"
    );
    let (_, answer) = post(
        &endpoint,
        "/api/v1/query_range",
        "query=cpu_usage%7Bhost%3D%22a%22%7D&start=0&end=120&step=60",
        b"",
    );
    assert!(answer.contains("[[60,\"10\"],[120,\"30\"]]"), "{answer}");
    let (_, labels) = post(&endpoint, "/api/v1/label/__name__/values", "", b"");
    assert!(
        labels.contains("\"cpu_usage\"")
            && labels.contains("\"cpu_idle\"")
            && labels.contains("\"up\""),
        "{labels}"
    );
    let (_, hosts) = post(&endpoint, "/api/v1/label/host/values", "", b"");
    assert_eq!(hosts, "{\"status\":\"success\",\"data\":[\"a\",\"b\"]}");
    assert_eq!(
        post(
            &endpoint,
            "/api/v2/write",
            "precision=s",
            b"cpu,host=a usage=\"text\" 2000"
        )
        .0,
        400
    );
    let refused = endpoint.answer(&Request {
        method: "GET".to_owned(),
        path: "/api/v1/labels".to_owned(),
        query: None,
        headers: Vec::new(),
        body: Vec::new(),
    });
    assert_eq!(refused.status, 401);
    let mut session = eggdb_sql::Session::new(&store);
    let mut total = None;
    session
        .execute(
            "SELECT count(*) FROM metrics.cpu WHERE host = 'a'",
            &mut |result| {
                total = result
                    .rows
                    .first()
                    .and_then(|row| row.first().and_then(eggdb_sql::Value::to_text))
            },
        )
        .unwrap();
    assert_eq!(total.as_deref(), Some("2"));
}

#[test]
fn hostile_query_api_requests_are_answered_without_panics_or_endless_loops() {
    let store = MemoryStore::default();
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb");
    let (status, _) = post(&endpoint, "/api/v1/label/values", "", b"");
    assert_eq!(status, 404);
    for query in [
        "query=1&start=1e300&end=1e300&step=1",
        "query=1&start=-1e300&end=1e300&step=1",
        "query=up%20offset%20100000000000000000000s&start=0&end=1&step=1",
    ] {
        let (status, answer) = post(&endpoint, "/api/v1/query_range", query, b"");
        assert!(status == 200 || status == 400, "{query}: {status} {answer}");
    }
    let long_sum = format!("query=1{}&time=1", "%2B1".repeat(100_000));
    let (status, answer) = post(&endpoint, "/api/v1/query", &long_sum, b"");
    assert_eq!(status, 400, "{answer}");
}

#[test]
fn the_longest_operator_chain_runs_on_a_default_thread_and_a_longer_one_is_refused() {
    let outcome = std::thread::spawn(|| {
        let data = BTreeMap::new();
        let chain = |operators: usize| format!("1{}", " + 1".repeat(operators));
        let longest = parse(&chain(256)).and_then(|expr| evaluate(&expr, 0, &data));
        let longer = parse(&chain(257));
        (vector_or_scalar(longest), longer.is_err())
    })
    .join()
    .unwrap();
    assert_eq!(outcome.0, Some(257.0));
    assert!(outcome.1);
}

fn vector_or_scalar(data: Result<Data, String>) -> Option<f64> {
    match data {
        Ok(Data::Scalar(value)) => Some(value),
        _ => None,
    }
}

const GZIP_LINES: [u8; 56] = [
    0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0x4b, 0x2e, 0x28, 0xd5, 0xc9, 0xc8,
    0x2f, 0x2e, 0xb1, 0x4d, 0x54, 0x28, 0x2d, 0x4e, 0x4c, 0x4f, 0xb5, 0x35, 0xd4, 0x33, 0x55, 0x30,
    0x34, 0x37, 0x40, 0x07, 0x5c, 0xc9, 0x30, 0x85, 0x49, 0x50, 0x85, 0x46, 0x38, 0x14, 0x02, 0x00,
    0x35, 0xbc, 0x95, 0x04, 0x52, 0x00, 0x00, 0x00,
];

#[test]
fn gzip_line_protocol_is_decoded_by_the_listener_and_remote_write_stays_snappy() {
    let to = |path: &str| Request {
        method: "POST".to_owned(),
        path: path.to_owned(),
        query: None,
        headers: Vec::new(),
        body: Vec::new(),
    };
    assert!(!crate::wants_content_decoded(&to("/api/v1/write")));
    assert!(crate::wants_content_decoded(&to("/api/v2/write")));
    assert!(crate::wants_content_decoded(&to("/write")));
    let store = MemoryStore::default();
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let target = eggdb_http::HttpTarget {
        host: "127.0.0.1".to_owned(),
        port: listener.local_addr().unwrap().port(),
        path: "/".to_owned(),
    };
    let budget = MemoryBudget::default();
    let limits = eggdb_http::Limits::new(crate::MAX_REQUEST_BYTES, &budget)
        .decoding_content_encoding_of(crate::wants_content_decoded);
    let stopped = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            eggdb_http::serve_with_limits(
                &listener,
                &|request| endpoint.answer(request),
                &|| stopped.load(std::sync::atomic::Ordering::Acquire),
                &limits,
            )
            .unwrap();
        });
        let send = |path: &str, coding: &str, body: &[u8]| {
            eggdb_http::request(
                &target,
                "POST",
                path,
                &[
                    ("Authorization".to_owned(), "Token secret".to_owned()),
                    ("Content-Encoding".to_owned(), coding.to_owned()),
                ],
                body,
                std::time::Duration::from_secs(10),
                1 << 20,
            )
            .unwrap()
            .status
        };
        assert_eq!(send("/api/v2/write?precision=ns", "gzip", &GZIP_LINES), 204);
        let remote = remote_write_body(&[(&[("__name__", "up"), ("job", "db")], &[(1.0, 61_000)])]);
        assert_eq!(send("/api/v1/write", "snappy", &remote), 204);
        assert_eq!(send("/write", "br", b"cpu usage=1"), 415);
        stopped.store(true, std::sync::atomic::Ordering::Release);
    });
    let mut session = eggdb_sql::Session::new(&store);
    let mut total = None;
    session
        .execute("SELECT count(*) FROM metrics.cpu", &mut |result| {
            total = result
                .rows
                .first()
                .and_then(|row| row.first().and_then(eggdb_sql::Value::to_text))
        })
        .unwrap();
    assert_eq!(total.as_deref(), Some("2"));
}
