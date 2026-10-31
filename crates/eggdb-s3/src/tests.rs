use crate::Endpoint;
use eggdb_auth::sigv4::{Presign, SignedRequest, authorization, format_amz_date, presigned_query};
use eggdb_http::{Request, Response};
use eggdb_keyspace::MemoryKeyspace;

const HOST: &str = "127.0.0.1:9000";

fn now() -> i64 {
    i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn call(endpoint: &Endpoint<'_>, method: &str, target: &str, headers: &[(&str, &str)], body: &[u8], secret: &str) -> Response {
    let (path, query) = target.split_once('?').map_or((target, None), |(path, query)| (path, Some(query.to_owned())));
    let mut request = Request { method: method.to_owned(), path: path.to_owned(), query, headers: Vec::new(), body: body.to_vec() };
    request.headers.push(("host".to_owned(), HOST.to_owned()));
    request.headers.push(("x-amz-date".to_owned(), format_amz_date(now())));
    request.headers.push(("x-amz-content-sha256".to_owned(), hex(&eggdb_crypto::sha256(body))));
    for (name, value) in headers {
        request.headers.push(((*name).to_owned(), (*value).to_owned()));
    }
    let signed = SignedRequest { method, path, query: request.query.as_deref(), headers: &request.headers, body };
    let value = authorization(&signed, &["host", "x-amz-content-sha256", "x-amz-date"], "eggdb", secret, "us-east-1", "s3").unwrap();
    request.headers.push(("authorization".to_owned(), value));
    endpoint.answer(&request)
}

fn header<'r>(response: &'r Response, name: &str) -> Option<&'r str> {
    response.headers.iter().find(|(field, _)| field.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

fn text(response: &Response) -> String {
    String::from_utf8(response.body.clone()).unwrap()
}

#[test]
fn objects_are_stored_read_by_range_listed_and_copied() {
    let keyspace = MemoryKeyspace::default();
    let endpoint = Endpoint::new(&keyspace, "eggdb", "secret");
    assert_eq!(call(&endpoint, "PUT", "/photos", &[], b"", "secret").status, 200);
    assert_eq!(call(&endpoint, "HEAD", "/photos", &[], b"", "secret").status, 200);
    let put = call(&endpoint, "PUT", "/photos/2026/egg%20one.txt", &[("content-type", "text/plain"), ("x-amz-meta-camera", "x100")], b"hello eggs", "secret");
    assert_eq!(put.status, 200, "{}", text(&put));
    assert_eq!(header(&put, "etag"), Some(format!("\"{}\"", hex(&eggdb_crypto::md5_checksum(b"hello eggs"))).as_str()));
    call(&endpoint, "PUT", "/photos/2026/two.txt", &[], b"2", "secret");
    call(&endpoint, "PUT", "/photos/readme", &[], b"r", "secret");
    let got = call(&endpoint, "GET", "/photos/2026/egg%20one.txt", &[], b"", "secret");
    assert_eq!((got.status, got.body.as_slice(), got.content_type.as_str()), (200, &b"hello eggs"[..], "text/plain"));
    assert_eq!(header(&got, "x-amz-meta-camera"), Some("x100"));
    let ranged = call(&endpoint, "GET", "/photos/2026/egg%20one.txt", &[("range", "bytes=6-")], b"", "secret");
    assert_eq!((ranged.status, ranged.body.as_slice(), header(&ranged, "content-range")), (206, &b"eggs"[..], Some("bytes 6-9/10")));
    let head = call(&endpoint, "HEAD", "/photos/2026/egg%20one.txt", &[], b"", "secret");
    assert_eq!((head.body.len(), header(&head, "content-length")), (0, Some("10")));
    let listed = text(&call(&endpoint, "GET", "/photos?list-type=2&delimiter=%2F&encoding-type=url", &[], b"", "secret"));
    assert!(listed.contains("<Key>readme</Key>") && listed.contains("<CommonPrefixes><Prefix>2026/</Prefix></CommonPrefixes>") && listed.contains("<KeyCount>2</KeyCount>"), "{listed}");
    let page = text(&call(&endpoint, "GET", "/photos?list-type=2&max-keys=1", &[], b"", "secret"));
    assert!(page.contains("<IsTruncated>true</IsTruncated>") && !page.contains("egg%20one.txt") && page.contains("<Key>2026/egg one.txt</Key>"), "{page}");
    let copied = call(&endpoint, "PUT", "/photos/copy", &[("x-amz-copy-source", "/photos/2026/egg%20one.txt")], b"", "secret");
    assert!(text(&copied).contains("<CopyObjectResult"));
    assert_eq!(call(&endpoint, "GET", "/photos/copy", &[], b"", "secret").body, b"hello eggs");
    assert_eq!(call(&endpoint, "GET", "/photos/missing", &[], b"", "secret").status, 404);
    assert_eq!(call(&endpoint, "DELETE", "/photos", &[], b"", "secret").status, 409);
    let delete = b"<Delete><Object><Key>2026/egg one.txt</Key></Object><Object><Key>2026/two.txt</Key></Object><Object><Key>readme</Key></Object><Object><Key>copy</Key></Object></Delete>";
    let deleted = text(&call(&endpoint, "POST", "/photos?delete", &[], delete, "secret"));
    assert_eq!(deleted.matches("<Deleted>").count(), 4, "{deleted}");
    assert_eq!(call(&endpoint, "DELETE", "/photos", &[], b"", "secret").status, 204);
}

#[test]
fn multipart_uploads_join_their_parts() {
    let keyspace = MemoryKeyspace::default();
    let endpoint = Endpoint::new(&keyspace, "eggdb", "secret");
    call(&endpoint, "PUT", "/big", &[], b"", "secret");
    let started = text(&call(&endpoint, "POST", "/big/file?uploads", &[], b"", "secret"));
    let upload = started.split("<UploadId>").nth(1).and_then(|rest| rest.split("</UploadId>").next()).unwrap().to_owned();
    let first = vec![b'a'; 300 * 1024];
    let one = call(&endpoint, "PUT", &format!("/big/file?partNumber=1&uploadId={upload}"), &[], &first, "secret");
    let two = call(&endpoint, "PUT", &format!("/big/file?partNumber=2&uploadId={upload}"), &[], b"tail", "secret");
    let complete = format!(
        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part><Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>",
        header(&one, "etag").unwrap().replace('"', "&quot;"),
        header(&two, "etag").unwrap()
    );
    let finished = call(&endpoint, "POST", &format!("/big/file?uploadId={upload}"), &[], complete.as_bytes(), "secret");
    assert!(text(&finished).contains("-2&quot;</ETag>"), "{}", text(&finished));
    let whole = call(&endpoint, "GET", "/big/file", &[], b"", "secret");
    assert_eq!(whole.body.len(), first.len() + 4);
    assert!(whole.body.ends_with(b"tail"));
    let ranged = call(&endpoint, "GET", "/big/file", &[("range", "bytes=262140-262147")], b"", "secret");
    assert_eq!(ranged.body, b"aaaaaaaa");
    assert_eq!(call(&endpoint, "POST", &format!("/big/file?uploadId={upload}"), &[], complete.as_bytes(), "secret").status, 404);
}

#[test]
fn signatures_presigned_urls_and_digests_are_checked() {
    let keyspace = MemoryKeyspace::default();
    let endpoint = Endpoint::new(&keyspace, "eggdb", "secret");
    assert_eq!(call(&endpoint, "PUT", "/docs", &[], b"", "wrong").status, 403);
    call(&endpoint, "PUT", "/docs", &[], b"", "secret");
    assert_eq!(call(&endpoint, "PUT", "/docs/a", &[("content-md5", "AAAAAAAAAAAAAAAAAAAAAA==")], b"x", "secret").status, 400);
    call(&endpoint, "PUT", "/docs/a", &[], b"shared", "secret");
    let presign = Presign { method: "GET", path: "/docs/a", host: HOST, region: "us-east-1", service: "s3", amz_date: &format_amz_date(now()), expires_seconds: 300 };
    let query = presigned_query(&presign, "eggdb", "secret");
    let request = |path: &str| Request { method: "GET".to_owned(), path: path.to_owned(), query: Some(query.clone()), headers: vec![("host".to_owned(), HOST.to_owned())], body: Vec::new() };
    assert_eq!(endpoint.answer(&request("/docs/a")).body, b"shared");
    assert_eq!(endpoint.answer(&request("/docs/b")).status, 403);
    let anonymous = Request { method: "GET".to_owned(), path: "/docs/a".to_owned(), query: None, headers: Vec::new(), body: Vec::new() };
    assert_eq!(endpoint.answer(&anonymous).status, 403);
    assert_eq!(call(&endpoint, "PUT", "/Bad_Bucket", &[], b"", "secret").status, 400);
}

#[test]
fn the_client_stores_lists_reads_and_deletes_objects_on_a_served_endpoint() {
    let keyspace = MemoryKeyspace::default();
    let endpoint = Endpoint::new(&keyspace, "eggdb", "secret");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        let server = scope.spawn(|| eggdb_http::serve_with_body_limit(&listener, &|request| endpoint.answer(request), &|| stop.load(std::sync::atomic::Ordering::Relaxed), crate::MAX_REQUEST_BYTES));
        let settings = |name: &str| match name {
            "AWS_ENDPOINT_URL" => Some(format!("http://{address}")),
            "AWS_ACCESS_KEY_ID" => Some("eggdb".to_owned()),
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_owned()),
            _ => None,
        };
        let location = crate::client::Location::parse("s3://backups/nightly/", &settings, 1 << 20).unwrap();
        assert_eq!(location.key("a b.eggdb"), "nightly/a b.eggdb");
        call(&endpoint, "PUT", "/backups", &[], b"", "secret");
        location.bucket.put(&location.key("a b.eggdb"), b"first").unwrap();
        location.bucket.put(&location.key("c.eggdb"), &vec![7u8; 300_000]).unwrap();
        assert_eq!(location.bucket.get(&location.key("a b.eggdb")).unwrap(), Some(b"first".to_vec()));
        assert_eq!(location.bucket.get(&location.key("missing")).unwrap(), None);
        assert_eq!(location.bucket.list("nightly/").unwrap(), ["nightly/a b.eggdb", "nightly/c.eggdb"]);
        location.bucket.delete(&location.key("c.eggdb")).unwrap();
        assert_eq!(location.bucket.list("nightly/").unwrap().len(), 1);
        let wrong = |name: &str| if name == "AWS_SECRET_ACCESS_KEY" { Some("wrong".to_owned()) } else { settings(name) };
        let refused = crate::client::Location::parse("s3://backups", &wrong, 1 << 20).unwrap();
        assert!(refused.bucket.get("nightly/a b.eggdb").unwrap_err().contains("403"));
        assert!(crate::client::Location::parse("s3://backups", &|_| None, 1).is_err());
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap().unwrap();
    });
}

