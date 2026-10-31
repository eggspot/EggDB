use crate::Endpoint;
use crate::line_protocol::{FieldValue, parse as parse_lines};
use crate::promql::{Data, evaluate, parse};
use crate::store::Series;
use eggdb_http::Request;
use eggdb_sql::{EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, SqlResult};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

type Map = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Default)]
struct MemoryStore {
    committed: Mutex<Arc<Map>>,
}

struct Snapshot(Arc<Map>);

struct Writer<'s> {
    store: &'s MemoryStore,
    map: Map,
}

fn scan(map: &Map, start: &[u8], end: &[u8], direction: ScanDirection, visit: &mut EntryVisitor<'_>) -> SqlResult<()> {
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

    fn scan(&self, start: &[u8], end: &[u8], direction: ScanDirection, visit: &mut EntryVisitor<'_>) -> SqlResult<()> {
        scan(&self.0, start, end, direction, visit)
    }
}

impl KeyValueRead for Writer<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        Ok(self.map.get(key).cloned())
    }

    fn scan(&self, start: &[u8], end: &[u8], direction: ScanDirection, visit: &mut EntryVisitor<'_>) -> SqlResult<()> {
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
    assert_eq!(points[0].tags, [("location".to_owned(), "us west".to_owned()), ("kind".to_owned(), "a,b".to_owned())]);
    assert_eq!(points[0].fields[0], ("temperature".to_owned(), FieldValue::Float(82.0)));
    assert_eq!(points[0].fields[1].1, FieldValue::Integer(43));
    assert_eq!(points[0].fields[2].1, FieldValue::Boolean(true));
    assert_eq!(points[0].fields[3].1, FieldValue::Text("say \"hi\"".to_owned()));
    assert_eq!(points[0].time_ns, 1_465_839_830_100_400_200);
    assert_eq!((points[1].measurement.as_str(), points[1].time_ns, &points[1].fields[0].1), ("my measure", 7, &FieldValue::Unsigned(1)));
    for broken in ["cpu", "cpu value=", "cpu value=nan", "cpu value=1 notatime", "cpu,host value=1", "cpu time=1", "cpu value=\"open"] {
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

fn remote_write_body(series: &[(&[(&str, &str)], &[(f64, i64)])]) -> Vec<u8> {
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
    let body = remote_write_body(&[(&[("__name__", "up"), ("job", "api")], &[(1.0, 1000), (0.0, 2000)])]);
    let points = crate::remote_write::decode(&body).unwrap();
    assert_eq!(points.len(), 2);
    assert_eq!((points[1].measurement.as_str(), &points[1].tags, points[1].time_ns), ("up", &vec![("job".to_owned(), "api".to_owned())], 2_000_000_000));
    assert!(crate::remote_write::decode(b"not snappy").is_err());
    assert!(crate::remote_write::decode(&remote_write_body(&[(&[("job", "x")], &[(1.0, 1)])])).is_err());
}

fn series(labels: &[(&str, &str)], samples: &[(i64, f64)]) -> Series {
    Series { labels: labels.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect(), samples: samples.to_vec() }
}

fn vector(data: Data) -> Vec<(Vec<(String, String)>, f64)> {
    match data {
        Data::Vector(vector) => vector,
        other => panic!("{other:?}"),
    }
}

#[test]
fn promql_selects_rates_aggregates_and_combines() {
    let requests = vec![
        series(&[("__name__", "requests"), ("code", "200"), ("job", "api")], &[(0, 0.0), (15_000, 15.0), (30_000, 30.0), (45_000, 45.0), (60_000, 60.0)]),
        series(&[("__name__", "requests"), ("code", "500"), ("job", "api")], &[(0, 100.0), (30_000, 10.0), (60_000, 40.0)]),
    ];
    let data = BTreeMap::from([("requests".to_owned(), requests)]);
    let at = |query: &str| vector(evaluate(&parse(query).unwrap(), 60_000, &data).unwrap());
    let rates = at("rate(requests{job=\"api\"}[1m])");
    assert_eq!(rates.len(), 2);
    assert!((rates[0].1 - 1.0).abs() < 1e-9, "{rates:?}");
    assert!(!rates[0].0.iter().any(|(name, _)| name == "__name__"));
    assert!((at("increase(requests{code=\"500\"}[1m])")[0].1 - 40.0).abs() < 1e-9);
    assert_eq!(at("sum by (job) (requests)"), [(vec![("job".to_owned(), "api".to_owned())], 100.0)]);
    assert_eq!(at("requests{code=~\"5..\"}")[0].1, 40.0);
    assert_eq!(at("requests{code!~\"5..\"} * 2 + 1")[0].1, 121.0);
    assert_eq!(at("requests > 50").len(), 1);
    assert_eq!(at("max_over_time(requests[1m]) - min_over_time(requests[1m])").iter().map(|(_, value)| *value).collect::<Vec<_>>(), [45.0, 30.0]);
    assert_eq!(at("count(requests offset 1m)")[0].1, 2.0);
    assert!(matches!(evaluate(&parse("1 + 2 * 3").unwrap(), 0, &data).unwrap(), Data::Scalar(value) if value == 7.0));
    for broken in ["rate(requests)", "sum(", "requests{code=200}", "requests[5x]", "{job=\"api\"}"] {
        assert!(parse(broken).and_then(|expr| evaluate(&expr, 0, &data)).is_err(), "{broken}");
    }
}

fn post(endpoint: &Endpoint<'_, MemoryStore>, path: &str, query: &str, body: &[u8]) -> (u16, String) {
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
    let lines = "cpu,host=a usage=10 1000\ncpu,host=b usage=20 1000\ncpu,host=a usage=30,idle=70i 61000\n";
    assert_eq!(post(&endpoint, "/api/v2/write", "org=x&bucket=y&precision=ms", lines.as_bytes()).0, 204);
    let body = remote_write_body(&[(&[("__name__", "up"), ("job", "db")], &[(1.0, 61_000)])]);
    assert_eq!(post(&endpoint, "/api/v1/write", "", &body).0, 204);
    let (status, answer) = post(&endpoint, "/api/v1/query", "query=sum(cpu_usage)&time=61", b"");
    assert_eq!(status, 200, "{answer}");
    assert!(answer.contains("\"value\":[61,\"50\"]"), "{answer}");
    let (_, answer) = post(&endpoint, "/api/v1/query", "query=up%7Bjob%3D%22db%22%7D&time=61", b"");
    assert!(answer.contains("\"__name__\":\"up\"") && answer.contains("\"job\":\"db\""), "{answer}");
    let (_, answer) = post(&endpoint, "/api/v1/query_range", "query=cpu_usage%7Bhost%3D%22a%22%7D&start=0&end=120&step=60", b"");
    assert!(answer.contains("[[60,\"10\"],[120,\"30\"]]"), "{answer}");
    let (_, labels) = post(&endpoint, "/api/v1/label/__name__/values", "", b"");
    assert!(labels.contains("\"cpu_usage\"") && labels.contains("\"cpu_idle\"") && labels.contains("\"up\""), "{labels}");
    let (_, hosts) = post(&endpoint, "/api/v1/label/host/values", "", b"");
    assert_eq!(hosts, "{\"status\":\"success\",\"data\":[\"a\",\"b\"]}");
    assert_eq!(post(&endpoint, "/api/v2/write", "precision=s", b"cpu,host=a usage=\"text\" 2000").0, 400);
    let refused = endpoint.answer(&Request { method: "GET".to_owned(), path: "/api/v1/labels".to_owned(), query: None, headers: Vec::new(), body: Vec::new() });
    assert_eq!(refused.status, 401);
    let mut session = eggdb_sql::Session::new(&store);
    let mut total = None;
    session.execute("SELECT count(*) FROM metrics.cpu WHERE host = 'a'", &mut |result| total = result.rows.first().and_then(|row| row.first().and_then(eggdb_sql::Value::to_text))).unwrap();
    assert_eq!(total.as_deref(), Some("2"));
}
