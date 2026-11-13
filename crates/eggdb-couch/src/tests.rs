use crate::CouchEndpoint;
use eggdb_http::{Request, Response};
use eggdb_keyspace::MemoryKeyspace;
use eggdb_sql::json::{Json, parse_document};

fn trusted(user: &str, password: &str) -> bool {
    user == "admin" && password == "secret"
}

fn call(endpoint: &CouchEndpoint<'_>, method: &str, target: &str, body: &str) -> Response {
    let (path, query) = target.split_once('?').map_or((target, None), |(path, query)| (path, Some(query.to_owned())));
    endpoint.answer(&Request {
        method: method.to_owned(),
        path: path.to_owned(),
        query,
        headers: vec![("authorization".to_owned(), format!("Basic {}", eggdb_crypto::base64_encode(b"admin:secret")))],
        body: body.as_bytes().to_vec(),
    })
}

fn text(response: &Response) -> String {
    String::from_utf8_lossy(&response.body).into_owned()
}

fn field(response: &Response, name: &str) -> Option<String> {
    let body = text(response);
    let Json::Object(members) = parse_document(&body).ok()?.value else {
        return None;
    };
    members.into_iter().find(|(member, _)| member == name).map(|(_, node)| match node.value {
        Json::String(value) | Json::Number(value) => value,
        other => format!("{other:?}"),
    })
}

#[test]
fn documents_get_revisions_and_conflicting_updates_are_refused() {
    let keyspace = MemoryKeyspace::default();
    let endpoint = CouchEndpoint::new(&keyspace, &trusted, "node-a");
    assert_eq!(call(&endpoint, "PUT", "/app", "").status, 201);
    assert_eq!(call(&endpoint, "PUT", "/App", "").status, 400);
    let created = call(&endpoint, "PUT", "/app/egg", "{\"colour\":\"brown\"}");
    assert_eq!(created.status, 201, "{}", text(&created));
    let first = field(&created, "rev").unwrap();
    assert!(first.starts_with("1-"));
    assert_eq!(call(&endpoint, "PUT", "/app/egg", "{\"colour\":\"white\"}").status, 409);
    let updated = call(&endpoint, "PUT", "/app/egg", &format!("{{\"_rev\":\"{first}\",\"colour\":\"white\"}}"));
    let second = field(&updated, "rev").unwrap();
    assert!(second.starts_with("2-"));
    let read = call(&endpoint, "GET", "/app/egg", "");
    assert_eq!(field(&read, "colour").as_deref(), Some("white"));
    assert_eq!(field(&read, "_rev"), Some(second.clone()));
    let deleted = call(&endpoint, "DELETE", &format!("/app/egg?rev={second}"), "");
    assert_eq!(deleted.status, 200);
    assert_eq!(call(&endpoint, "GET", "/app/egg", "").status, 404);
    assert_eq!(field(&call(&endpoint, "GET", "/app", ""), "doc_del_count").as_deref(), Some("1"));
    assert_eq!(call(&endpoint, "PUT", "/app/photo", "{\"_attachments\":{}}").status, 415);
}

#[test]
fn a_replication_round_moves_every_revision_and_conflicts_resolve_alike() {
    let (source_keys, target_keys) = (MemoryKeyspace::default(), MemoryKeyspace::default());
    let source = CouchEndpoint::new(&source_keys, &trusted, "source");
    let target = CouchEndpoint::new(&target_keys, &trusted, "target");
    for endpoint in [&source, &target] {
        call(endpoint, "PUT", "/notes", "");
    }
    call(&source, "PUT", "/notes/a", "{\"n\":1}");
    let target_own = field(&call(&target, "PUT", "/notes/a", "{\"n\":2}"), "rev").unwrap();
    call(&source, "PUT", "/notes/b", "{\"n\":3}");
    let changes = call(&source, "GET", "/notes/_changes?style=all_docs&since=0", "");
    let body = text(&changes);
    assert!(body.contains("\"id\":\"a\"") && body.contains("\"id\":\"b\""), "{body}");
    let last_seq = field(&changes, "last_seq").unwrap();
    let a_rev = field(&call(&source, "GET", "/notes/a", ""), "_rev").unwrap();
    let b_rev = field(&call(&source, "GET", "/notes/b", ""), "_rev").unwrap();
    let diff = call(&target, "POST", "/notes/_revs_diff", &format!("{{\"a\":[\"{a_rev}\"],\"b\":[\"{b_rev}\"]}}"));
    assert!(text(&diff).contains(&a_rev) && text(&diff).contains(&b_rev), "{}", text(&diff));
    let fetched = call(&source, "POST", "/notes/_bulk_get?revs=true", &format!("{{\"docs\":[{{\"id\":\"a\",\"rev\":\"{a_rev}\"}},{{\"id\":\"b\",\"rev\":\"{b_rev}\"}}]}}"));
    assert!(text(&fetched).contains("_revisions"), "{}", text(&fetched));
    let a_doc = call(&source, "GET", "/notes/a?revs=true", "");
    let b_doc = call(&source, "GET", "/notes/b?revs=true", "");
    let bulk = format!("{{\"new_edits\":false,\"docs\":[{},{}]}}", text(&a_doc), text(&b_doc));
    assert_eq!(call(&target, "POST", "/notes/_bulk_docs", &bulk).status, 201);
    assert_eq!(text(&call(&target, "POST", "/notes/_revs_diff", &format!("{{\"a\":[\"{a_rev}\"]}}"))), "{}");
    call(&target, "PUT", "/notes/_local/checkpoint", &format!("{{\"last_seq\":\"{last_seq}\"}}"));
    assert_eq!(field(&call(&target, "GET", "/notes/_local/checkpoint", ""), "last_seq"), Some(last_seq));
    let open = text(&call(&target, "GET", "/notes/a?open_revs=all", ""));
    assert_eq!(open.matches("\"ok\"").count(), 2, "{open}");
    let target_winner = field(&call(&target, "GET", "/notes/a", ""), "_rev").unwrap();
    let back = format!("{{\"new_edits\":false,\"docs\":[{}]}}", text(&call(&target, "GET", &format!("/notes/a?rev={target_own}&revs=true"), "")));
    assert_eq!(call(&source, "POST", "/notes/_bulk_docs", &back).status, 201);
    assert_eq!(field(&call(&source, "GET", "/notes/a", ""), "_rev"), Some(target_winner));
}

#[test]
fn every_request_needs_the_servers_credentials() {
    let keyspace = MemoryKeyspace::default();
    let endpoint = CouchEndpoint::new(&keyspace, &trusted, "node");
    let anonymous = endpoint.answer(&Request { method: "GET".to_owned(), path: "/".to_owned(), query: None, headers: Vec::new(), body: Vec::new() });
    assert_eq!(anonymous.status, 401);
    assert_eq!(field(&call(&endpoint, "GET", "/", ""), "couchdb").as_deref(), Some("Welcome"));
    let longpoll_started = std::time::Instant::now();
    call(&endpoint, "PUT", "/quiet", "");
    let quiet = call(&endpoint, "GET", "/quiet/_changes?feed=longpoll&timeout=300", "");
    assert!(longpoll_started.elapsed() >= std::time::Duration::from_millis(300));
    assert_eq!(field(&quiet, "last_seq").as_deref(), Some("0"));
}
