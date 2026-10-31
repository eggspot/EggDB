use crate::cql::{Assignment, Statement, Term, parse};
use crate::frame::{AUTH_RESPONSE, AUTH_SUCCESS, AUTHENTICATE, Body, ERROR, EXECUTE, Out, PREPARE, QUERY, RESULT, STARTUP};
use crate::server::{ServerConfig, serve_stream};
use crate::translate::{Catalog, TableInfo, translate};
use crate::types::{CqlType, encode_value, parameter_text, varint_round_trip};
use eggdb_sql::{EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, Session, SqlResult, Value};
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
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

struct FixedCatalog;

impl Catalog for FixedCatalog {
    fn table(&mut self, keyspace: &str, table: &str) -> Result<TableInfo, String> {
        Ok(TableInfo {
            keyspace: keyspace.to_owned(),
            table: table.to_owned(),
            columns: vec![("k".to_owned(), CqlType::Varchar), ("c".to_owned(), CqlType::Int), ("v".to_owned(), CqlType::Counter)],
            partition: vec!["k".to_owned()],
            clustering: vec!["c".to_owned()],
            descending: vec!["c".to_owned()],
        })
    }
}

fn sql(cql: &str, values: &[Option<&str>]) -> String {
    let (statement, markers) = parse(cql).unwrap();
    let translation = translate(&statement, markers, Some("ks"), &mut FixedCatalog).unwrap();
    translation.render(&values.iter().map(|value| value.map(str::to_owned)).collect::<Vec<_>>())
}

#[test]
fn statements_parse_into_their_parts() {
    let (statement, markers) = parse("UPDATE ks.t USING TTL 0 SET v = v + ?, \"Name\" = 'it''s' WHERE k = ? AND c = 3;").unwrap();
    assert_eq!(markers, 2);
    let Statement::Update { assignments, relations, .. } = statement else { panic!("{statement:?}") };
    assert_eq!(assignments[0], ("v".to_owned(), Assignment::Add(Term::Marker(0))));
    assert_eq!(assignments[1], ("Name".to_owned(), Assignment::Set(Term::Text("it's".to_owned()))));
    assert_eq!(relations[0].terms, [Term::Marker(1)]);
    let (create, _) = parse("CREATE TABLE IF NOT EXISTS t (a int, b text, c uuid, PRIMARY KEY ((a, b), c)) WITH CLUSTERING ORDER BY (c DESC) AND comment = 'x' AND compaction = {'class': 'X'}").unwrap();
    let Statement::CreateTable { partition, clustering, descending, .. } = create else { panic!() };
    assert_eq!((partition.len(), clustering, descending), (2, vec!["c".to_owned()], vec!["c".to_owned()]));
    assert!(matches!(parse("SELECT * FROM t WHERE id = 123e4567-e89b-12d3-a456-426614174000").unwrap().0, Statement::Select { .. }));
    assert!(matches!(parse("BEGIN BATCH INSERT INTO t (a) VALUES (1); DELETE FROM t WHERE a = 2; APPLY BATCH").unwrap().0, Statement::Batch(children) if children.len() == 2));
    for refused in ["INSERT INTO t (a) VALUES (1) USING TTL 60", "CREATE TABLE t (a list<int> PRIMARY KEY)", "UPDATE t SET a = 1 WHERE b = 2 IF EXISTS", "SELECT * FROM t WHERE a = 'unclosed", "GRANT ALL ON t TO x", "SELECT * FROM t extra"] {
        assert!(parse(refused).is_err(), "{refused}");
    }
}

#[test]
fn statements_translate_into_upserts_and_ordered_selects() {
    assert_eq!(
        sql("INSERT INTO t (k, c) VALUES ('a', ?)", &[Some("1")]),
        "INSERT INTO \"ks\".\"t\" (\"k\", \"c\") VALUES ('a'::text, '1'::integer) ON CONFLICT (\"k\", \"c\") DO NOTHING"
    );
    assert_eq!(
        sql("UPDATE t SET v = v + 2 WHERE k = 'a' AND c = 1", &[]),
        "INSERT INTO \"ks\".\"t\" (\"k\", \"c\", \"v\") VALUES ('a'::text, 1::integer, 2::bigint) ON CONFLICT (\"k\", \"c\") DO UPDATE SET \"v\" = COALESCE(\"t\".\"v\", 0) + EXCLUDED.\"v\""
    );
    assert_eq!(
        sql("SELECT k, v FROM t WHERE k = ? LIMIT 5", &[Some("o'k")]),
        "SELECT \"k\", \"v\" FROM \"ks\".\"t\" WHERE \"k\" = 'o''k'::text ORDER BY \"k\", \"c\" DESC LIMIT 5::integer"
    );
    assert_eq!(sql("DELETE v FROM t WHERE k IN ('a', 'b')", &[]), "UPDATE \"ks\".\"t\" SET \"v\" = NULL WHERE \"k\" IN ('a'::text, 'b'::text)");
    let (statement, markers) = parse("UPDATE t SET v = 1 WHERE k = 'a'").unwrap();
    assert!(translate(&statement, markers, Some("ks"), &mut FixedCatalog).unwrap_err().contains("missing: c"));
    let (statement, markers) = parse("SELECT * FROM t").unwrap();
    assert!(translate(&statement, markers, None, &mut FixedCatalog).unwrap_err().contains("No keyspace"));
}

#[test]
fn values_convert_between_cql_bytes_and_sql() {
    for number in ["0", "127", "128", "-128", "-129", "255", "-1", "123456789012345678901234567890", "-98765432109876543210"] {
        assert_eq!(varint_round_trip(number), number);
    }
    assert_eq!(parameter_text(Some(&0i64.to_be_bytes()), CqlType::Timestamp).unwrap().unwrap(), "1970-01-01 00:00:00.000+00");
    assert_eq!(parameter_text(Some(&951_782_400_123i64.to_be_bytes()), CqlType::Timestamp).unwrap().unwrap(), "2000-02-29 00:00:00.123+00");
    assert_eq!(parameter_text(Some(&(1u32 << 31).to_be_bytes()), CqlType::Date).unwrap().unwrap(), "1970-01-01");
    assert_eq!(parameter_text(Some(&[0, 0, 0, 2, 0x30, 0x39]), CqlType::Decimal).unwrap().unwrap(), "123.45");
    assert_eq!(parameter_text(Some(&[0, 0, 0, 3, 0xff]), CqlType::Decimal).unwrap().unwrap(), "-0.001");
    assert_eq!(parameter_text(Some(&[127, 0, 0, 1]), CqlType::Inet).unwrap().unwrap(), "127.0.0.1");
    assert!(parameter_text(Some(&[1, 2]), CqlType::Int).is_err());
    assert_eq!(encode_value(&Value::TimestampTz(0), CqlType::Timestamp).unwrap().unwrap(), 946_684_800_000i64.to_be_bytes());
    assert_eq!(encode_value(&Value::Date(0), CqlType::Date).unwrap().unwrap(), ((1u32 << 31) + 10_957).to_be_bytes());
    assert_eq!(encode_value(&Value::Text("127.0.0.1".to_owned()), CqlType::Inet).unwrap().unwrap(), [127, 0, 0, 1]);
    assert_eq!(encode_value(&Value::Null, CqlType::Int).unwrap(), None);
}

fn frame(stream: i16, opcode: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![4, 0];
    out.extend_from_slice(&stream.to_be_bytes());
    out.push(opcode);
    out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn query(stream: i16, text: &str) -> Vec<u8> {
    let mut body = Out::default();
    body.int(i32::try_from(text.len()).unwrap());
    body.bytes.extend_from_slice(text.as_bytes());
    body.short(1);
    body.bytes.push(0);
    frame(stream, QUERY, &body.bytes)
}

fn login(password: &str) -> Vec<u8> {
    let mut startup = Out::default();
    startup.short(1).string("CQL_VERSION").string("3.0.0");
    let mut token = b"\0cassandra\0".to_vec();
    token.extend_from_slice(password.as_bytes());
    let mut auth = Out::default();
    auth.bytes(Some(&token));
    [frame(1, STARTUP, &startup.bytes), frame(2, AUTH_RESPONSE, &auth.bytes)].concat()
}

fn responses(output: &[u8]) -> Vec<(i16, u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 9 <= output.len() {
        let length = u32::from_be_bytes(output[at + 5..at + 9].try_into().unwrap()) as usize;
        out.push((i16::from_be_bytes([output[at + 2], output[at + 3]]), output[at + 4], output[at + 9..at + 9 + length].to_vec()));
        at += 9 + length;
    }
    out
}

fn skip_option(body: &mut Body<'_>) {
    match body.short().unwrap() {
        0x0020 | 0x0022 => skip_option(body),
        0x0021 => {
            skip_option(body);
            skip_option(body);
        }
        _ => {}
    }
}

fn rows(body: &[u8]) -> (Vec<String>, Vec<Vec<Option<Vec<u8>>>>) {
    let mut body = Body::new(body);
    assert_eq!(body.int().unwrap(), 2);
    let flags = body.int().unwrap();
    let count = body.int().unwrap();
    if flags & 1 != 0 {
        body.string().unwrap();
        body.string().unwrap();
    }
    let mut names = Vec::new();
    for _ in 0..count {
        names.push(body.string().unwrap());
        skip_option(&mut body);
    }
    let row_count = body.int().unwrap();
    let rows = (0..row_count).map(|_| (0..count).map(|_| body.bytes().unwrap()).collect()).collect();
    assert!(body.is_empty());
    (names, rows)
}

fn config() -> ServerConfig {
    ServerConfig { user: "cassandra".to_owned(), password_sha256: eggdb_crypto::sha256(b"secret"), database: "eggdb".to_owned(), max_connections: 4 }
}

fn converse(store: &MemoryStore, input: &[u8]) -> Vec<(i16, u8, Vec<u8>)> {
    let mut output = Vec::new();
    serve_stream(store, &config(), IpAddr::V4(Ipv4Addr::LOCALHOST), input, &mut output, &|| false).unwrap();
    responses(&output)
}

#[test]
fn a_driver_session_creates_writes_and_reads_tables_that_sql_also_sees() {
    let store = MemoryStore::default();
    let mut input = login("secret");
    for (stream, text) in [
        (3, "CREATE KEYSPACE shop WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}"),
        (4, "USE shop"),
        (5, "CREATE TABLE orders (customer text, placed timestamp, total decimal, items int, PRIMARY KEY (customer, placed)) WITH CLUSTERING ORDER BY (placed DESC)"),
        (6, "INSERT INTO orders (customer, placed, total, items) VALUES ('ada', '2026-10-01 10:00:00+00', 12.50, 2)"),
        (7, "INSERT INTO orders (customer, placed, total, items) VALUES ('ada', '2026-10-02 10:00:00+00', 3.25, 1)"),
        (8, "UPDATE orders SET items = 5 WHERE customer = 'ada' AND placed = '2026-10-01 10:00:00+00'"),
        (9, "SELECT placed, total, items FROM orders WHERE customer = 'ada'"),
        (10, "SELECT * FROM system.local WHERE key = 'local'"),
        (11, "SELECT keyspace_name, column_name, kind FROM system_schema.columns WHERE keyspace_name = 'shop'"),
        (12, "INSERT INTO orders (customer, placed) VALUES ('ada', '2026-10-01 10:00:00+00') IF NOT EXISTS"),
        (13, "SELECT * FROM nowhere"),
    ] {
        input.extend(query(stream, text));
    }
    let counted = "SELECT count(*) FROM shop.orders WHERE customer = ?";
    let mut prepare = Out::default();
    prepare.int(i32::try_from(counted.len()).unwrap());
    prepare.bytes.extend_from_slice(counted.as_bytes());
    input.extend(frame(14, PREPARE, &prepare.bytes));
    let replies = converse(&store, &input);
    assert_eq!(replies[0].1, AUTHENTICATE);
    assert_eq!(replies[1].1, AUTH_SUCCESS);
    for reply in &replies[2..13] {
        if reply.0 != 13 {
            assert_eq!(reply.1, RESULT, "stream {}: {:?}", reply.0, String::from_utf8_lossy(&reply.2));
        }
    }
    let (names, selected) = rows(&replies[8].2);
    assert_eq!(names, ["placed", "total", "items"]);
    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0][2], Some(1i32.to_be_bytes().to_vec()));
    assert_eq!(selected[1][2], Some(5i32.to_be_bytes().to_vec()));
    assert_eq!(selected[1][1], Some(vec![0, 0, 0, 2, 0x04, 0xe2]));
    let (local, _) = rows(&replies[9].2);
    assert!(local.contains(&"release_version".to_owned()));
    let (_, columns) = rows(&replies[10].2);
    assert_eq!(columns.len(), 4);
    assert!(columns.iter().any(|row| row[1].as_deref() == Some(b"placed") && row[2].as_deref() == Some(b"clustering")));
    assert_eq!(rows(&replies[11].2).1, [vec![Some(vec![0])]]);
    assert_eq!(replies[12].1, ERROR);
    let mut prepared = Body::new(&replies[13].2);
    assert_eq!(prepared.int().unwrap(), 4);
    let id = prepared.short_bytes().unwrap();
    let mut execute = Out::default();
    execute.short_bytes(&id).short(1);
    execute.bytes.push(1);
    execute.short(1).bytes(Some(b"ada"));
    let mut again = login("secret");
    again.extend(frame(15, EXECUTE, &execute.bytes));
    let replies = converse(&store, &again);
    assert_eq!(replies[2].1, ERROR, "a prepared id belongs to its connection");
    let mut session = Session::new(&store);
    let mut count = None;
    session.execute("SELECT sum(items) FROM shop.orders", &mut |result| count = result.rows.first().and_then(|row| row.first().cloned())).unwrap();
    assert_eq!(count.and_then(|value| value.to_text()), Some("6".to_owned()));
}

#[test]
fn wrong_passwords_and_early_queries_are_refused() {
    let store = MemoryStore::default();
    let replies = converse(&store, &[query(1, "SELECT * FROM system.local"), login("wrong")].concat());
    assert_eq!(replies.iter().map(|reply| reply.1).collect::<Vec<_>>(), [ERROR, AUTHENTICATE, ERROR]);
    let mut body = Body::new(&replies[2].2);
    assert_eq!(body.int().unwrap(), 0x0100);
    let mut wrong_version = frame(1, 0x05, &[]);
    wrong_version[0] = 5;
    assert_eq!(converse(&store, &wrong_version)[0].1, ERROR);
}
