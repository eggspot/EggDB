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

fn graphql(endpoint: &Endpoint<'_, MemoryStore>, query: &str, variables: &str) -> String {
    let body = format!(
        "{{\"query\":{},\"variables\":{variables}}}",
        crate::json_string(query)
    );
    let answered = call(endpoint, "POST", "/graphql", &body, None);
    assert_eq!(answered.status, 200, "{}", text(&answered));
    text(&answered)
}

#[test]
fn graphql_reads_and_writes_tables_with_relationships() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "CREATE TABLE authors (id integer PRIMARY KEY, name text NOT NULL)",
        "CREATE TABLE books (id integer PRIMARY KEY, title text NOT NULL, pages integer, author_id integer REFERENCES authors (id))",
        "INSERT INTO authors VALUES (1, 'Ada'), (2, 'Brian')",
        "INSERT INTO books VALUES (10, 'Engines', 300, 1), (11, 'Notes', 40, 1), (12, 'C', 270, 2)",
    ] {
        session.execute(sql, &mut |_| {}).unwrap();
    }
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb", "public");
    let listed = graphql(
        &endpoint,
        "query Long($min: Int!) { books(where: {pages: {_gte: $min}}, order_by: {id: asc}) { id title author { name } } }",
        "{\"min\":100}",
    );
    assert_eq!(
        listed,
        "{\"data\":{\"books\":[{\"id\":10,\"title\":\"Engines\",\"author\":{\"name\":\"Ada\"}},{\"id\":12,\"title\":\"C\",\"author\":{\"name\":\"Brian\"}}]}}"
    );
    let nested = graphql(&endpoint, "{ authors_by_pk(id: 1) { name books(order_by: {id: desc}) { title } } }", "{}");
    assert!(nested.contains("\"books\":[{\"title\":\"Notes\"},{\"title\":\"Engines\"}]"), "{nested}");
    let counted = graphql(&endpoint, "{ books_aggregate { aggregate { count max { pages } } } }", "{}");
    assert!(counted.contains("\"count\":3"), "{counted}");
    assert!(counted.contains("\"pages\":300"), "{counted}");
    let changed = graphql(
        &endpoint,
        "mutation { insert_books_one(object: {id: 13, title: \"New\", author_id: 2}) { id } update_books(where: {author_id: {_eq: 2}}, _inc: {pages: 1}) { affected_rows } }",
        "{}",
    );
    assert!(changed.contains("\"insert_books_one\":{\"id\":13}"), "{changed}");
    assert!(changed.contains("\"affected_rows\":2"), "{changed}");
    let described = graphql(
        &endpoint,
        "query Introspect { __schema { queryType { name } types { kind name fields { name type { kind ofType { kind name } } } } } __type(name: \"books\") { name fields { name } } }",
        "{}",
    );
    assert!(described.contains("\"queryType\":{\"name\":\"query_root\"}"), "{described}");
    assert!(described.contains("\"name\":\"books_bool_exp\""), "{described}");
    assert!(described.contains("\"__type\":{\"name\":\"books\",\"fields\":[{\"name\":\"id\"}"), "{described}");
    let failed = graphql(&endpoint, "{ nothing { id } }", "{}");
    assert!(failed.contains("\"errors\""), "{failed}");
    let refused = graphql(&endpoint, "mutation { insert_books_one(object: {id: 10, title: \"Dup\"}) { id } }", "{}");
    assert!(refused.contains("\"errors\""), "{refused}");
}

#[test]
fn graphql_nested_inserts_follow_relationships() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "CREATE TABLE authors (id integer PRIMARY KEY, name text NOT NULL)",
        "CREATE TABLE books (id integer PRIMARY KEY, title text NOT NULL, author_id integer REFERENCES authors (id))",
    ] {
        session.execute(sql, &mut |_| {}).unwrap();
    }
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb", "public");
    let with_books = graphql(
        &endpoint,
        "mutation { insert_authors_one(object: {id: 1, name: \"Ada\", books: {data: [{id: 10, title: \"Engines\"}, {id: 11, title: \"Notes\"}]}}) { name books(order_by: {id: asc}) { title } } }",
        "{}",
    );
    assert!(
        with_books.contains("\"insert_authors_one\":{\"name\":\"Ada\",\"books\":[{\"title\":\"Engines\"},{\"title\":\"Notes\"}]}"),
        "{with_books}"
    );
    let with_author = graphql(
        &endpoint,
        "mutation { insert_books(objects: [{id: 12, title: \"C\", author: {data: {id: 2, name: \"Brian\"}}}]) { affected_rows returning { title author { name } } } }",
        "{}",
    );
    assert!(with_author.contains("\"affected_rows\":1"), "{with_author}");
    assert!(with_author.contains("\"author\":{\"name\":\"Brian\"}"), "{with_author}");
    let counted = graphql(&endpoint, "{ books_aggregate { aggregate { count } } }", "{}");
    assert!(counted.contains("\"count\":3"), "{counted}");
}

#[test]
fn odata_serves_metadata_sets_keys_and_writes() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "CREATE TABLE products (id integer PRIMARY KEY, name text NOT NULL, price numeric)",
        "INSERT INTO products VALUES (1, 'Egg', 0.5), (2, 'Omelette', 4.25), (3, 'Toast', NULL)",
    ] {
        session.execute(sql, &mut |_| {}).unwrap();
    }
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb", "public");
    let metadata = call(&endpoint, "GET", "/odata/$metadata", "", None);
    assert_eq!(metadata.status, 200);
    assert!(text(&metadata).contains("<EntityType Name=\"products\"><Key><PropertyRef Name=\"id\"/></Key>"), "{}", text(&metadata));
    assert!(text(&metadata).contains("<Property Name=\"price\" Type=\"Edm.Decimal\"/>"), "{}", text(&metadata));
    let listed = call(
        &endpoint,
        "GET",
        "/odata/products?$filter=price%20gt%201%20or%20price%20eq%20null&$select=id,name&$orderby=id%20desc&$count=true",
        "",
        None,
    );
    assert_eq!(listed.status, 200, "{}", text(&listed));
    assert!(
        text(&listed).contains("\"@odata.count\":2,\"value\":[{\"id\":3,\"name\":\"Toast\"},{\"id\":2,\"name\":\"Omelette\"}]"),
        "{}",
        text(&listed)
    );
    let one = call(&endpoint, "GET", "/odata/products(1)", "", None);
    assert!(text(&one).contains("\"name\":\"Egg\""), "{}", text(&one));
    let counted = call(&endpoint, "GET", "/odata/products/$count", "", None);
    assert_eq!(text(&counted), "3");
    let created = call(&endpoint, "POST", "/odata/products", "{\"id\":4,\"name\":\"Pancake\"}", None);
    assert_eq!(created.status, 201, "{}", text(&created));
    let changed = call(&endpoint, "PATCH", "/odata/products(4)", "{\"price\":2}", None);
    assert_eq!(changed.status, 204, "{}", text(&changed));
    let removed = call(&endpoint, "DELETE", "/odata/products(4)", "", None);
    assert_eq!(removed.status, 204);
    let missing = call(&endpoint, "GET", "/odata/products(4)", "", None);
    assert_eq!(missing.status, 404);
    let refused = call(&endpoint, "GET", "/odata/products?$filter=nope%20eq%201", "", None);
    assert_eq!(refused.status, 400);
}

fn live(endpoint: &Endpoint<'_, MemoryStore>, method: &str, path: &str, query: Option<&str>, body: &str) -> String {
    let headers = vec![
        (
            "authorization".to_owned(),
            format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:secret")),
        ),
        ("accept".to_owned(), "text/event-stream".to_owned()),
        ("x-eggdb-live-events".to_owned(), "1".to_owned()),
    ];
    let answered = endpoint.answer(&Request {
        method: method.to_owned(),
        path: path.to_owned(),
        query: query.map(str::to_owned),
        headers,
        body: body.as_bytes().to_vec(),
    });
    assert_eq!(answered.status, 200, "{}", text(&answered));
    assert!(answered.content_type.starts_with("text/event-stream"));
    text(&answered)
}

#[test]
fn live_queries_send_server_sent_events() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "CREATE TABLE readings (id integer PRIMARY KEY, celsius integer)",
        "INSERT INTO readings VALUES (1, 20), (2, 25)",
    ] {
        session.execute(sql, &mut |_| {}).unwrap();
    }
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb", "public");
    let rows = live(&endpoint, "GET", "/readings", Some("celsius=gt.21"), "");
    assert_eq!(rows, "event: rows\ndata: [{\"id\":2,\"celsius\":25}]\n\n");
    let subscribed = live(
        &endpoint,
        "POST",
        "/graphql",
        None,
        "{\"query\":\"subscription { readings(order_by: {id: asc}) { id } }\"}",
    );
    assert_eq!(
        subscribed,
        "event: next\ndata: {\"data\":{\"readings\":[{\"id\":1},{\"id\":2}]}}\n\nevent: complete\ndata: \n\n"
    );
}

#[test]
fn ogc_features_filter_with_cql2() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "CREATE TABLE cities (id integer PRIMARY KEY, name text, population integer, geom geometry)",
        "INSERT INTO cities VALUES (1, 'Berlin', 3600000, ST_SetSRID(ST_MakePoint(13.4, 52.5), 4326)), (2, 'Bonn', 330000, ST_SetSRID(ST_MakePoint(7.1, 50.7), 4326)), (3, 'Paris', 2100000, ST_SetSRID(ST_MakePoint(2.35, 48.85), 4326))",
    ] {
        session.execute(sql, &mut |_| {}).unwrap();
    }
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb", "public");
    let filtered = call(
        &endpoint,
        "GET",
        "/ogc/collections/cities/items?filter=population%20%3E%201000000%20AND%20S_INTERSECTS(geom%2C%20BBOX(5%2C45%2C15%2C55))",
        "",
        None,
    );
    assert_eq!(filtered.status, 200, "{}", text(&filtered));
    assert!(text(&filtered).contains("\"numberReturned\":1"), "{}", text(&filtered));
    assert!(text(&filtered).contains("Berlin"), "{}", text(&filtered));
    let queryables = call(&endpoint, "GET", "/ogc/collections/cities/queryables", "", None);
    assert!(text(&queryables).contains("\"population\":{\"type\":\"integer\"}"), "{}", text(&queryables));
    let refused = call(&endpoint, "GET", "/ogc/collections/cities/items?filter=secret%20%3D%201", "", None);
    assert_eq!(refused.status, 400);
}
