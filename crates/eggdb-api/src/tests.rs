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
fn graphql_nested_inserts_go_several_relationships_deep_with_on_conflict() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "CREATE TABLE publishers (id integer PRIMARY KEY, name text NOT NULL)",
        "CREATE TABLE authors (id integer PRIMARY KEY, name text NOT NULL, publisher_id integer REFERENCES publishers (id))",
        "CREATE TABLE books (id integer PRIMARY KEY, title text NOT NULL, author_id integer REFERENCES authors (id))",
    ] {
        session.execute(sql, &mut |_| {}).unwrap();
    }
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb", "public");
    let deep = graphql(
        &endpoint,
        "mutation { insert_publishers_one(object: {id: 1, name: \"Tor\", authors: {data: [{id: 1, name: \"Ada\", books: {data: [{id: 10, title: \"Engines\"}, {id: 11, title: \"Notes\"}]}}]}}) { name authors { name books(order_by: {id: asc}) { title } } } }",
        "{}",
    );
    assert!(
        deep.contains("\"authors\":[{\"name\":\"Ada\",\"books\":[{\"title\":\"Engines\"},{\"title\":\"Notes\"}]}]"),
        "{deep}"
    );
    let upward = graphql(
        &endpoint,
        "mutation { insert_books_one(object: {id: 12, title: \"Cities\", author: {data: {id: 2, name: \"Italo\", publisher: {data: {id: 2, name: \"Einaudi\"}}}}}) { title author { name publisher { name } } } }",
        "{}",
    );
    assert!(upward.contains("\"publisher\":{\"name\":\"Einaudi\"}"), "{upward}");
    let merged = graphql(
        &endpoint,
        "mutation { insert_books_one(object: {id: 13, title: \"Lathe\", author: {data: {id: 1, name: \"Ada L.\"}, on_conflict: {constraint: authors_pkey, update_columns: [name]}}}) { author { id name } } }",
        "{}",
    );
    assert!(merged.contains("\"author\":{\"id\":1,\"name\":\"Ada L.\"}"), "{merged}");
    let refused = graphql(
        &endpoint,
        "mutation { insert_books_one(object: {id: 14, title: \"X\", author: {data: {id: 1, name: \"Ada\"}, on_conflict: {constraint: authors_pkey, update_columns: []}}}) { id } }",
        "{}",
    );
    assert!(refused.contains("needs update_columns"), "{refused}");
    let counted = graphql(&endpoint, "{ books_aggregate { aggregate { count } } authors_aggregate { aggregate { count } } }", "{}");
    assert!(counted.contains("\"count\":4"), "{counted}");
    assert!(counted.contains("\"count\":2"), "{counted}");
}

fn batch(endpoint: &Endpoint<'_, MemoryStore>, content_type: &str, body: &str) -> Response {
    endpoint.answer(&Request {
        method: "POST".to_owned(),
        path: "/odata/$batch".to_owned(),
        query: None,
        headers: vec![
            (
                "authorization".to_owned(),
                format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:secret")),
            ),
            ("content-type".to_owned(), content_type.to_owned()),
        ],
        body: body.as_bytes().to_vec(),
    })
}

#[test]
fn odata_expands_with_options_and_answers_batches() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "CREATE TABLE authors (id integer PRIMARY KEY, name text NOT NULL)",
        "CREATE TABLE books (id integer PRIMARY KEY, title text NOT NULL, pages integer, author_id integer REFERENCES authors (id))",
        "INSERT INTO authors VALUES (1, 'Ada'), (2, 'Brian')",
        "INSERT INTO books VALUES (10, 'Engines', 300, 1), (11, 'Notes', 50, 1), (12, 'C', 200, 2)",
    ] {
        session.execute(sql, &mut |_| {}).unwrap();
    }
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb", "public");
    let filtered = call(
        &endpoint,
        "GET",
        "/odata/authors?$expand=books($select=title;$filter=pages%20gt%20100;$orderby=title%20desc;$count=true)&$orderby=id",
        "",
        None,
    );
    assert_eq!(filtered.status, 200, "{}", text(&filtered));
    assert!(
        text(&filtered).contains("\"name\":\"Ada\",\"books@odata.count\":1,\"books\":[{\"title\":\"Engines\"}]"),
        "{}",
        text(&filtered)
    );
    let nested = call(&endpoint, "GET", "/odata/books(11)?$expand=author($expand=books($top=1;$select=id))", "", None);
    assert!(
        text(&nested).contains("\"author\":{\"id\":1,\"name\":\"Ada\",\"books\":[{\"id\":10}]}"),
        "{}",
        text(&nested)
    );
    let refused = call(&endpoint, "GET", "/odata/authors?$expand=books($levels=2)", "", None);
    assert_eq!(refused.status, 400);
    let multipart = "--batch_1\r\nContent-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n\r\nGET authors(1) HTTP/1.1\r\nAccept: application/json\r\n\r\n\r\n\
--batch_1\r\nContent-Type: multipart/mixed; boundary=changeset_1\r\n\r\n\
--changeset_1\r\nContent-Type: application/http\r\nContent-ID: 1\r\n\r\nPOST authors HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{\"id\":3,\"name\":\"Cleo\"}\r\n\
--changeset_1\r\nContent-Type: application/http\r\nContent-ID: 2\r\n\r\nPATCH $1 HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{\"name\":\"Cleo B.\"}\r\n\
--changeset_1--\r\n\
--batch_1--\r\n";
    let answered = batch(&endpoint, "multipart/mixed; boundary=batch_1", multipart);
    assert_eq!(answered.status, 200, "{}", text(&answered));
    assert!(answered.content_type.starts_with("multipart/mixed; boundary=batchresponse_"));
    for expected in ["HTTP/1.1 200 OK", "HTTP/1.1 201 Created", "HTTP/1.1 204 No Content", "Content-ID: 2"] {
        assert!(text(&answered).contains(expected), "{expected}: {}", text(&answered));
    }
    assert!(text(&call(&endpoint, "GET", "/odata/authors(3)", "", None)).contains("\"name\":\"Cleo B.\""));
    let undone = "--b\r\nContent-Type: multipart/mixed; boundary=c\r\n\r\n\
--c\r\nContent-Type: application/http\r\n\r\nPOST authors HTTP/1.1\r\n\r\n{\"id\":4,\"name\":\"Dee\"}\r\n\
--c\r\nContent-Type: application/http\r\n\r\nDELETE authors(99) HTTP/1.1\r\n\r\n\r\n\
--c--\r\n\
--b--\r\n";
    let failed = batch(&endpoint, "multipart/mixed; boundary=b", undone);
    assert!(text(&failed).contains("HTTP/1.1 404 Not Found"), "{}", text(&failed));
    assert_eq!(call(&endpoint, "GET", "/odata/authors(4)", "", None).status, 404);
    let json = batch(
        &endpoint,
        "application/json",
        "{\"requests\":[{\"id\":\"a\",\"method\":\"POST\",\"url\":\"authors\",\"atomicityGroup\":\"g1\",\"body\":{\"id\":5,\"name\":\"Eve\"}},{\"id\":\"b\",\"method\":\"DELETE\",\"url\":\"authors(99)\",\"atomicityGroup\":\"g1\"},{\"id\":\"c\",\"method\":\"GET\",\"url\":\"authors(1)\"}]}",
    );
    assert_eq!(json.status, 200, "{}", text(&json));
    assert!(text(&json).contains("{\"id\":\"a\",\"atomicityGroup\":\"g1\",\"status\":404"), "{}", text(&json));
    assert!(text(&json).contains("{\"id\":\"c\",\"status\":200"), "{}", text(&json));
    assert_eq!(call(&endpoint, "GET", "/odata/authors(5)", "", None).status, 404);
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

#[test]
fn odata_expands_navigation_properties_from_foreign_keys() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "CREATE TABLE authors (id integer PRIMARY KEY, name text NOT NULL)",
        "CREATE TABLE books (id integer PRIMARY KEY, title text NOT NULL, author_id integer REFERENCES authors (id))",
        "INSERT INTO authors VALUES (1, 'Ada'), (2, 'Brian')",
        "INSERT INTO books VALUES (10, 'Engines', 1), (11, 'Notes', 1), (12, 'Orphan', NULL)",
    ] {
        session.execute(sql, &mut |_| {}).unwrap();
    }
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb", "public");
    let metadata = text(&call(&endpoint, "GET", "/odata/$metadata", "", None));
    assert!(metadata.contains("<NavigationProperty Name=\"author\" Type=\"EggDB.authors\"><ReferentialConstraint Property=\"author_id\" ReferencedProperty=\"id\"/></NavigationProperty>"), "{metadata}");
    assert!(metadata.contains("<NavigationProperty Name=\"books\" Type=\"Collection(EggDB.books)\"/>"), "{metadata}");
    let books = text(&call(&endpoint, "GET", "/odata/books?$select=title&$expand=author", "", None));
    assert!(books.contains("{\"title\":\"Engines\",\"author\":{\"id\":1,\"name\":\"Ada\"}}"), "{books}");
    assert!(books.contains("{\"title\":\"Orphan\",\"author\":null}"), "{books}");
    let author = text(&call(&endpoint, "GET", "/odata/authors(2)?$expand=books", "", None));
    assert!(author.contains("\"name\":\"Brian\",\"books\":[]"), "{author}");
    let unknown = call(&endpoint, "GET", "/odata/books?$expand=publisher", "", None);
    assert_eq!(unknown.status, 400);
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

fn grpc_call(api: &crate::GrpcApi<'_, '_, MemoryStore>, method: &str, message: &[u8]) -> eggdb_http2::Http2Response {
    use eggdb_http2::Handler;
    api.handle(&eggdb_http2::Http2Request {
        headers: vec![
            (":method".to_owned(), "POST".to_owned()),
            (":path".to_owned(), format!("/eggdb.v1.BooksService/{method}")),
            ("content-type".to_owned(), "application/grpc".to_owned()),
            (
                "authorization".to_owned(),
                format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:secret")),
            ),
        ],
        body: eggdb_http2::grpc_message(message),
    })
}

fn grpc_status(response: &eggdb_http2::Http2Response) -> &str {
    response
        .trailers
        .iter()
        .find(|(name, _)| name == "grpc-status")
        .map_or("", |(_, value)| value.as_str())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

#[test]
fn grpc_serves_each_table_as_a_service() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session
        .execute(
            "CREATE TABLE books (id integer PRIMARY KEY, title text NOT NULL, pages integer, price double precision)",
            &mut |_| {},
        )
        .unwrap();
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb", "public");
    let proto = text(&call(&endpoint, "GET", "/grpc/eggdb.proto", "", None));
    for expected in [
        "package eggdb.v1;",
        "optional string title = 2;",
        "message BooksKey {\n  int32 id = 1;\n}",
        "rpc List(BooksListRequest) returns (stream Books);",
    ] {
        assert!(proto.contains(expected), "{expected}: {proto}");
    }
    let api = crate::GrpcApi::new(&endpoint);
    let mut engines = vec![0x08, 0x01, 0x12, 0x07];
    engines.extend_from_slice(b"Engines");
    engines.extend_from_slice(&[0x18, 0xac, 0x02]);
    let inserted = grpc_call(&api, "Insert", &engines);
    assert_eq!(grpc_status(&inserted), "0", "{:?}", inserted.trailers);
    assert!(contains(&inserted.body.concat(), b"Engines"));
    let listed = grpc_call(&api, "List", &[0x08, 0x0a]);
    assert_eq!(grpc_status(&listed), "0");
    assert_eq!(listed.body.len(), 1);
    let updated = grpc_call(&api, "Update", &[0x08, 0x01, 0x18, 0xad, 0x02]);
    assert_eq!(grpc_status(&updated), "0", "{:?}", updated.trailers);
    assert!(contains(&updated.body.concat(), &[0x18, 0xad, 0x02]));
    let found = grpc_call(&api, "Get", &[0x08, 0x01]);
    assert!(contains(&found.body.concat(), b"Engines"));
    let deleted = grpc_call(&api, "Delete", &[0x08, 0x01]);
    assert!(contains(&deleted.body.concat(), &[0x08, 0x01]));
    let missing = grpc_call(&api, "Get", &[0x08, 0x01]);
    assert_eq!(grpc_status(&missing), "5");
    let unknown = grpc_call(&api, "Rename", &[]);
    assert_eq!(grpc_status(&unknown), "12");
}

fn masked_text(text: &str) -> Vec<u8> {
    let mask = [0x11, 0x22, 0x33, 0x44];
    let payload = text.as_bytes();
    let mut frame = vec![0x81];
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(at, byte)| byte ^ mask[at % 4]));
    frame
}

fn websocket_session(endpoint: &Endpoint<'_, MemoryStore>, path: &str, protocol: &str, messages: &[&str]) -> String {
    let request = Request {
        method: "GET".to_owned(),
        path: path.to_owned(),
        query: None,
        headers: vec![
            (
                "authorization".to_owned(),
                format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:secret")),
            ),
            ("upgrade".to_owned(), "websocket".to_owned()),
            ("connection".to_owned(), "Upgrade".to_owned()),
            ("sec-websocket-version".to_owned(), "13".to_owned()),
            ("sec-websocket-key".to_owned(), "dGhlIHNhbXBsZSBub25jZQ==".to_owned()),
            ("sec-websocket-protocol".to_owned(), protocol.to_owned()),
        ],
        body: Vec::new(),
    };
    let eggdb_http::Reply::Upgraded(upgrade) = endpoint.reply(&request) else {
        panic!("{path} did not switch to a WebSocket");
    };
    assert!(upgrade.headers.iter().any(|(name, value)| name == "Sec-WebSocket-Accept" && value == "s3pPLMBiTxaQ9kYGzzhZRp1zmrE="));
    let input: Vec<u8> = messages.iter().flat_map(|message| masked_text(message)).collect();
    let mut reader = std::io::Cursor::new(input);
    let mut written = Vec::new();
    (upgrade.run)(&mut reader, &mut written).unwrap();
    String::from_utf8_lossy(&written).into_owned()
}

#[test]
fn graphql_and_live_rows_subscribe_over_websockets() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    for sql in [
        "CREATE TABLE books (id integer PRIMARY KEY, title text NOT NULL)",
        "INSERT INTO books VALUES (1, 'Engines'), (2, 'Notes')",
    ] {
        session.execute(sql, &mut |_| {}).unwrap();
    }
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb", "public");
    let graphql = websocket_session(
        &endpoint,
        "/graphql",
        "graphql-transport-ws",
        &[
            "{\"type\":\"connection_init\"}",
            "{\"type\":\"subscribe\",\"id\":\"s1\",\"payload\":{\"query\":\"subscription { books(order_by: {id: asc}) { title } }\"}}",
        ],
    );
    assert!(graphql.contains("{\"type\":\"connection_ack\"}"), "{graphql}");
    assert!(
        graphql.contains("{\"type\":\"next\",\"id\":\"s1\",\"payload\":{\"data\":{\"books\":[{\"title\":\"Engines\"},{\"title\":\"Notes\"}]}}}"),
        "{graphql}"
    );
    let early = websocket_session(
        &endpoint,
        "/graphql",
        "graphql-transport-ws",
        &["{\"type\":\"subscribe\",\"id\":\"s1\",\"payload\":{\"query\":\"subscription { books { title } }\"}}"],
    );
    assert!(early.as_bytes().windows(2).any(|pair| pair == [0x88, 0x02]), "{early:?}");
    let rows = websocket_session(
        &endpoint,
        "/realtime",
        "eggdb-live",
        &["{\"type\":\"subscribe\",\"id\":\"r1\",\"path\":\"/books?select=title&order=id.desc\"}"],
    );
    assert!(
        rows.contains("{\"type\":\"rows\",\"id\":\"r1\",\"rows\":[{\"title\":\"Notes\"},{\"title\":\"Engines\"}]}"),
        "{rows}"
    );
}

#[test]
fn grpc_reflection_lists_services_and_describes_them() {
    use eggdb_http2::Handler;
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session
        .execute("CREATE TABLE books (id integer PRIMARY KEY, title text)", &mut |_| {})
        .unwrap();
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb", "public");
    let api = crate::GrpcApi::new(&endpoint);
    let headers = vec![
        (":method".to_owned(), "POST".to_owned()),
        (":path".to_owned(), "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo".to_owned()),
        ("content-type".to_owned(), "application/grpc".to_owned()),
        (
            "authorization".to_owned(),
            format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:secret")),
        ),
    ];
    assert!(api.streams(&headers));
    let listed = api.stream_message(&headers, &[0x3a, 0x01, b'*']).concat();
    assert!(contains(&listed, b"eggdb.v1.BooksService"));
    assert!(contains(&listed, b"grpc.reflection.v1.ServerReflection"));
    let mut symbol = vec![0x22, 21];
    symbol.extend_from_slice(b"eggdb.v1.BooksService");
    let described = api.stream_message(&headers, &symbol).concat();
    for expected in [&b"eggdb/v1/eggdb.proto"[..], b"BooksListRequest", b".eggdb.v1.BooksKey", b"proto3"] {
        assert!(contains(&described, expected), "{expected:?}");
    }
    let mut elsewhere = vec![0x22, 11];
    elsewhere.extend_from_slice(b"other.Thing");
    let missing = api.stream_message(&headers, &elsewhere).concat();
    assert!(contains(&missing, b"is not one of EggDB's gRPC symbols"));
}

#[test]
fn realtime_channels_join_postgres_changes_and_answer_heartbeats() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    session
        .execute("CREATE TABLE books (id integer PRIMARY KEY, title text)", &mut |_| {})
        .unwrap();
    let endpoint = Endpoint::new(&store, "eggdb", eggdb_crypto::sha256(b"secret"), "eggdb", "public");
    let answered = websocket_session(
        &endpoint,
        "/realtime/v1/websocket",
        "",
        &[
            "{\"topic\":\"phoenix\",\"event\":\"heartbeat\",\"payload\":{},\"ref\":\"1\"}",
            "{\"topic\":\"realtime:books-feed\",\"event\":\"phx_join\",\"payload\":{\"config\":{\"broadcast\":{\"self\":true},\"postgres_changes\":[{\"event\":\"INSERT\",\"schema\":\"public\",\"table\":\"books\",\"filter\":\"id=gt.10\"}]}},\"ref\":\"2\",\"join_ref\":\"2\"}",
            "{\"topic\":\"realtime:books-feed\",\"event\":\"broadcast\",\"payload\":{\"event\":\"cursor\",\"x\":1},\"ref\":\"3\",\"join_ref\":\"2\"}",
            "{\"topic\":\"realtime:missing\",\"event\":\"phx_join\",\"payload\":{\"config\":{\"postgres_changes\":[{\"event\":\"*\",\"schema\":\"public\",\"table\":\"nothing\"}]}},\"ref\":\"4\"}",
        ],
    );
    assert!(answered.contains("{\"topic\":\"phoenix\",\"event\":\"phx_reply\",\"payload\":{\"status\":\"ok\",\"response\":{}},\"ref\":\"1\"}"), "{answered}");
    assert!(
        answered.contains("\"response\":{\"postgres_changes\":[{\"id\":1,\"event\":\"INSERT\",\"schema\":\"public\",\"table\":\"books\",\"filter\":\"id=gt.10\"}]}"),
        "{answered}"
    );
    assert!(answered.contains("\"message\":\"Subscribed to PostgreSQL\""), "{answered}");
    assert!(answered.contains("\"event\":\"broadcast\",\"payload\":{\"event\":\"cursor\",\"x\":1}"), "{answered}");
    assert!(answered.contains("\"topic\":\"realtime:missing\",\"event\":\"phx_reply\",\"payload\":{\"status\":\"error\""), "{answered}");
}
