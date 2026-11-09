use crate::Endpoint;
use crate::request::Query;
use eggdb_http::{Request, Response};
use eggdb_sql::{
    EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, Session, SqlResult,
};
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

fn call(
    endpoint: &Endpoint<'_, MemoryStore>,
    method: &str,
    target: &str,
    body: &str,
    prefer: Option<&str>,
) -> Response {
    let (path, query) = target
        .split_once('?')
        .map_or((target, None), |(path, query)| {
            (path, Some(query.to_owned()))
        });
    let mut headers = vec![(
        "authorization".to_owned(),
        format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:secret")),
    )];
    if let Some(prefer) = prefer {
        headers.push(("prefer".to_owned(), prefer.to_owned()));
    }
    endpoint.answer(&Request {
        method: method.to_owned(),
        path: path.to_owned(),
        query,
        headers,
        body: body.as_bytes().to_vec(),
    })
}

fn text(response: &Response) -> String {
    String::from_utf8(response.body.clone()).unwrap()
}

#[test]
fn filters_render_as_quoted_sql() {
    let query = Query::parse("select=id,label:name&name=like.Egg*&id=in.(1,%222,3%22)&price=not.is.null&order=price.desc.nullslast,id&limit=5&offset=10").unwrap();
    assert_eq!(query.projection(), "\"id\", \"name\" AS \"label\"");
    assert_eq!(
        query.where_clause(),
        " WHERE \"name\" LIKE 'Egg%' AND \"id\" IN ('1', '2,3') AND NOT (\"price\" IS NULL)"
    );
    assert_eq!(
        query.order_clause(),
        " ORDER BY \"price\" DESC NULLS LAST, \"id\""
    );
    assert_eq!(query.page_clause(), " LIMIT 5 OFFSET 10");
    for bad in [
        "id=zz.1",
        "x;drop=eq.1",
        "select=a)b",
        "order=id.sideways",
        "limit=many",
        "id=in.1,2",
        "id=eq",
    ] {
        assert!(Query::parse(bad).is_err(), "{bad}");
    }
    assert_eq!(
        Query::parse("name=eq.O'Brien").unwrap().where_clause(),
        " WHERE \"name\" = 'O''Brien'"
    );
}

#[test]
fn tables_are_read_written_and_counted_over_http() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session.execute("CREATE TABLE eggs (id integer PRIMARY KEY, name text NOT NULL, price numeric, details jsonb)", &mut |_| {}).unwrap();
    let endpoint = Endpoint::new(
        &store,
        "eggdb",
        eggdb_crypto::sha256(b"secret"),
        "eggdb",
        "public",
    );
    let created = call(
        &endpoint,
        "POST",
        "/eggs",
        r#"[{"id":1,"name":"Brown","price":1.5},{"id":2,"name":"White","details":{"farm":"A"}}]"#,
        Some("return=representation"),
    );
    assert_eq!(created.status, 201, "{}", text(&created));
    assert!(
        text(&created).contains(r#""id":2,"name":"White","price":null,"details":{"#)
            && text(&created).contains(r#""farm""#),
        "{}",
        text(&created)
    );
    let listed = call(
        &endpoint,
        "GET",
        "/eggs?select=id,name&price=gt.1&order=id.desc",
        "",
        Some("count=exact"),
    );
    assert_eq!(
        (listed.status, text(&listed).as_str()),
        (200, r#"[{"id":1,"name":"Brown"}]"#)
    );
    assert!(
        listed
            .headers
            .iter()
            .any(|(name, value)| name == "Content-Range" && value == "0-0/1")
    );
    assert_eq!(
        call(&endpoint, "PATCH", "/eggs?id=eq.2", r#"{"price":2}"#, None).status,
        204
    );
    assert_eq!(
        text(&call(
            &endpoint,
            "GET",
            "/eggs?id=eq.2&select=price",
            "",
            None
        )),
        r#"[{"price":2}]"#
    );
    let removed = call(
        &endpoint,
        "DELETE",
        "/eggs?name=ilike.brown",
        "",
        Some("return=representation"),
    );
    assert!(text(&removed).contains("Brown"));
    assert_eq!(call(&endpoint, "GET", "/nowhere", "", None).status, 404);
    assert_eq!(
        call(
            &endpoint,
            "POST",
            "/eggs",
            r#"{"id":2,"name":"Again"}"#,
            None
        )
        .status,
        409
    );
    assert!(text(&call(&endpoint, "GET", "/", "", None)).contains("\"eggs\":{\"columns\""));
    let anonymous = endpoint.answer(&Request {
        method: "GET".to_owned(),
        path: "/eggs".to_owned(),
        query: None,
        headers: Vec::new(),
        body: Vec::new(),
    });
    assert_eq!(anonymous.status, 401);
}

#[test]
fn rows_are_streamed_in_chunks_over_a_kept_connection() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session
        .execute(
            "CREATE TABLE readings (id integer PRIMARY KEY, label text)",
            &mut |_| {},
        )
        .unwrap();
    let values: Vec<String> = (1..=3000)
        .map(|id| format!("({id}, 'reading {id}')"))
        .collect();
    session
        .execute(
            &format!("INSERT INTO readings VALUES {}", values.join(", ")),
            &mut |_| {},
        )
        .unwrap();
    let endpoint = Endpoint::new(
        &store,
        "eggdb",
        eggdb_crypto::sha256(b"secret"),
        "eggdb",
        "public",
    );
    let authorization = format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:secret"));
    let request = Request {
        method: "GET".to_owned(),
        path: "/readings".to_owned(),
        query: Some("order=id".to_owned()),
        headers: vec![("authorization".to_owned(), authorization.clone())],
        body: Vec::new(),
    };
    assert!(matches!(
        endpoint.reply(&request),
        eggdb_http::Reply::Streamed(_)
    ));
    let whole = endpoint.answer(&request);
    assert!(text(&whole).starts_with(r#"[{"id":1,"label":"reading 1"},"#));
    assert!(text(&whole).ends_with(r#"{"id":3000,"label":"reading 3000"}]"#));

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let budget = eggdb_platform::connection_limits::MemoryBudget::default();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            eggdb_http::serve_replies_with_limits(
                &listener,
                &|request| endpoint.reply(request),
                &|| stopped.load(std::sync::atomic::Ordering::Acquire),
                &eggdb_http::Limits::new(crate::MAX_REQUEST_BYTES, &budget),
            )
            .unwrap();
        });
        use std::io::{Read, Write};
        let mut client = std::net::TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let head = format!(
            "GET /readings?order=id HTTP/1.1\r\nHost: x\r\nAuthorization: {authorization}\r\n\r\nGET /readings?id=eq.7 HTTP/1.1\r\nHost: x\r\nAuthorization: {authorization}\r\nConnection: close\r\n\r\n"
        );
        client.write_all(head.as_bytes()).unwrap();
        let mut answer = Vec::new();
        client.read_to_end(&mut answer).unwrap();
        let answer = String::from_utf8(answer).unwrap();
        assert_eq!(answer.matches("HTTP/1.1 200 OK\r\n").count(), 2, "{answer}");
        assert!(
            answer.contains("Transfer-Encoding: chunked\r\n"),
            "{answer}"
        );
        assert!(answer.contains("Content-Range: 0-2999/*\r\n"), "{answer}");
        let sizes = first_chunk_sizes(&answer);
        assert!(sizes.len() > 1, "{sizes:?}");
        assert!(
            sizes
                .iter()
                .all(|&size| size > 0 && size <= eggdb_http::STREAM_CHUNK_BYTES),
            "{sizes:?}"
        );
        assert!(
            answer.ends_with("\r\n[{\"id\":7,\"label\":\"reading 7\"}]\r\n0\r\n\r\n"),
            "{answer}"
        );
        stopped.store(true, std::sync::atomic::Ordering::Release);
    });
    assert_eq!(budget.used_bytes(), 0);
}

fn first_chunk_sizes(answer: &str) -> Vec<usize> {
    let Some((_, mut rest)) = answer.split_once("\r\n\r\n") else {
        return Vec::new();
    };
    let mut sizes = Vec::new();
    while let Some((line, after)) = rest.split_once("\r\n") {
        let Ok(size) = usize::from_str_radix(line, 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        sizes.push(size);
        rest = after.get(size + 2..).unwrap_or_default();
    }
    sizes
}
