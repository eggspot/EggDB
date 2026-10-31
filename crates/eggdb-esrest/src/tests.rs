use crate::Api;
use eggdb_http::{Request, Response};
use eggdb_resp::MemoryKeyspace;

const PASSWORD: &str = "secret";

fn api(keyspace: &MemoryKeyspace) -> Api<'_> {
    Api::new(keyspace, "eggdb", eggdb_crypto::sha256(PASSWORD.as_bytes()))
}

fn request(method: &str, target: &str, body: &str) -> Request {
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (target.to_owned(), None),
    };
    let credentials = eggdb_crypto::base64_encode(format!("eggdb:{PASSWORD}").as_bytes());
    Request {
        method: method.to_owned(),
        path,
        query,
        headers: vec![("Authorization".to_owned(), format!("Basic {credentials}"))],
        body: body.as_bytes().to_vec(),
    }
}

fn body(response: &Response) -> String {
    String::from_utf8(response.body.clone()).expect("the body is UTF-8")
}

fn seeded(keyspace: &MemoryKeyspace) -> Api<'_> {
    let api = api(keyspace);
    let bulk = concat!(
        "{\"index\":{\"_index\":\"books\",\"_id\":\"1\"}}\n",
        "{\"title\":\"The quick brown fox\",\"genre\":\"fable\",\"pages\":120}\n",
        "{\"index\":{\"_index\":\"books\",\"_id\":\"2\"}}\n",
        "{\"title\":\"Foxes and hounds running quickly\",\"genre\":\"nature\",\"pages\":300}\n",
        "{\"index\":{\"_index\":\"books\",\"_id\":\"3\"}}\n",
        "{\"title\":\"A slow green turtle\",\"genre\":\"fable\",\"pages\":80}\n",
    );
    let response = api.answer(&request("POST", "/_bulk", bulk));
    assert_eq!(response.status, 200, "{}", body(&response));
    assert!(body(&response).contains("\"errors\":false"));
    api
}

#[test]
fn a_request_without_credentials_is_refused() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    let mut unauthenticated = request("GET", "/", "");
    unauthenticated.headers.clear();
    let response = api.answer(&unauthenticated);
    assert_eq!(response.status, 401);
    assert!(response.headers.iter().any(|(name, _)| name == "WWW-Authenticate"));
    let mut wrong = request("GET", "/", "");
    wrong.headers = vec![("Authorization".to_owned(), format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:nope")))];
    assert_eq!(api.answer(&wrong).status, 401);
}

#[test]
fn the_root_describes_the_cluster() {
    let keyspace = MemoryKeyspace::default();
    let response = api(&keyspace).answer(&request("GET", "/", ""));
    assert_eq!(response.status, 200);
    assert!(body(&response).contains("\"number\":\"2.11.0\""));
}

#[test]
fn indices_are_created_listed_and_deleted() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    assert_eq!(api.answer(&request("PUT", "/logs", "{\"mappings\":{\"properties\":{\"message\":{\"type\":\"text\"}}}}")).status, 200);
    assert_eq!(api.answer(&request("PUT", "/logs", "")).status, 400);
    assert_eq!(api.answer(&request("HEAD", "/logs", "")).status, 200);
    let described = api.answer(&request("GET", "/logs", ""));
    assert!(body(&described).contains("\"message\":{\"type\":\"text\"}"), "{}", body(&described));
    assert_eq!(api.answer(&request("DELETE", "/logs", "")).status, 200);
    assert_eq!(api.answer(&request("HEAD", "/logs", "")).status, 404);
    assert_eq!(api.answer(&request("DELETE", "/logs", "")).status, 404);
    assert_eq!(api.answer(&request("PUT", "/Bad", "")).status, 400);
}

#[test]
fn documents_are_indexed_read_updated_and_deleted() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    let created = api.answer(&request("PUT", "/people/_doc/7", "{\"name\":\"Ada\",\"age\":36}"));
    assert_eq!(created.status, 201);
    assert!(body(&created).contains("\"result\":\"created\""));
    let replaced = api.answer(&request("PUT", "/people/_doc/7", "{\"name\":\"Ada\",\"age\":37}"));
    assert_eq!(replaced.status, 200);
    assert!(body(&replaced).contains("\"_version\":2"));
    let fetched = api.answer(&request("GET", "/people/_doc/7", ""));
    assert!(body(&fetched).contains("\"_source\":{\"name\":\"Ada\",\"age\":37}"), "{}", body(&fetched));
    let updated = api.answer(&request("POST", "/people/_update/7", "{\"doc\":{\"age\":38,\"city\":\"London\"}}"));
    assert_eq!(updated.status, 200);
    let fetched = api.answer(&request("GET", "/people/_doc/7", ""));
    assert!(body(&fetched).contains("\"age\":38") && body(&fetched).contains("\"city\":\"London\""));
    assert_eq!(api.answer(&request("PUT", "/people/_create/7", "{}")).status, 409);
    assert_eq!(api.answer(&request("DELETE", "/people/_doc/7", "")).status, 200);
    assert_eq!(api.answer(&request("GET", "/people/_doc/7", "")).status, 404);
    assert_eq!(api.answer(&request("PUT", "/people/_doc/8", "[1,2]")).status, 400);
}

#[test]
fn a_document_without_an_id_gets_one() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    let first = body(&api.answer(&request("POST", "/notes/_doc", "{\"text\":\"one\"}")));
    let second = body(&api.answer(&request("POST", "/notes/_doc", "{\"text\":\"two\"}")));
    assert_ne!(first, second);
    let counted = api.answer(&request("GET", "/notes/_count", ""));
    assert!(body(&counted).contains("\"count\":2"));
}

#[test]
fn a_match_query_ranks_by_relevance_with_stemming() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let response = api.answer(&request("POST", "/books/_search", "{\"query\":{\"match\":{\"title\":\"fox quick\"}}}"));
    let text = body(&response);
    assert_eq!(response.status, 200, "{text}");
    assert!(text.contains("\"total\":{\"value\":2,\"relation\":\"eq\"}"), "{text}");
    let first = text.find("\"_id\":\"1\"").expect("book 1 matches");
    let second = text.find("\"_id\":\"2\"").expect("book 2 matches");
    assert!(first < second, "{text}");
    assert!(!text.contains("\"_id\":\"3\""));
}

#[test]
fn a_bool_query_combines_filters() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let query = "{\"query\":{\"bool\":{\"filter\":[{\"term\":{\"genre\":\"fable\"}},{\"range\":{\"pages\":{\"gte\":100}}}]}}}";
    let text = body(&api.answer(&request("POST", "/books/_search", query)));
    assert!(text.contains("\"value\":1,"), "{text}");
    assert!(text.contains("\"_id\":\"1\""));
}

#[test]
fn search_sorts_pages_and_aggregates() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let query = "{\"size\":2,\"from\":1,\"sort\":[{\"pages\":{\"order\":\"desc\"}}],\"aggs\":{\"genres\":{\"terms\":{\"field\":\"genre\"}},\"longest\":{\"max\":{\"field\":\"pages\"}},\"average\":{\"avg\":{\"field\":\"pages\"}}}}";
    let text = body(&api.answer(&request("POST", "/books/_search", query)));
    let first = text.find("\"_id\":\"1\"").expect("book 1 is on the page");
    let second = text.find("\"_id\":\"3\"").expect("book 3 is on the page");
    assert!(first < second && !text.contains("\"_id\":\"2\""), "{text}");
    assert!(text.contains("{\"key\":\"fable\",\"doc_count\":2}"), "{text}");
    assert!(text.contains("\"longest\":{\"value\":300"), "{text}");
    assert!(text.contains("\"average\":{\"value\":166.66"), "{text}");
}

#[test]
fn a_query_string_parameter_searches_every_field() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let text = body(&api.answer(&request("GET", "/books/_search?q=turtle", "")));
    assert!(text.contains("\"_id\":\"3\"") && text.contains("\"value\":1,"), "{text}");
}

#[test]
fn searching_a_missing_index_is_an_error() {
    let keyspace = MemoryKeyspace::default();
    let response = api(&keyspace).answer(&request("POST", "/nothing/_search", ""));
    assert_eq!(response.status, 404);
    assert!(body(&response).contains("index_not_found_exception"));
}

#[test]
fn a_malformed_query_is_a_parsing_error() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let response = api.answer(&request("POST", "/books/_search", "{\"query\":{\"nonsense\":{}}}"));
    assert_eq!(response.status, 400, "{}", body(&response));
    let response = api.answer(&request("POST", "/books/_search", "{\"from\":9999,\"size\":10}"));
    assert_eq!(response.status, 400);
}

#[test]
fn bulk_reports_each_item() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let bulk = concat!(
        "{\"create\":{\"_id\":\"1\"}}\n",
        "{\"title\":\"duplicate\"}\n",
        "{\"delete\":{\"_id\":\"3\"}}\n",
    );
    let text = body(&api.answer(&request("POST", "/books/_bulk", bulk)));
    assert!(text.contains("\"errors\":true"), "{text}");
    assert!(text.contains("\"status\":409") && text.contains("\"result\":\"deleted\""), "{text}");
}

#[test]
fn hostile_bodies_return_errors() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let deep = "[".repeat(10_000);
    for text in [deep.as_str(), "{", "\u{0}", "{\"query\":", "{\"index\":{}}\n"] {
        for path in ["/books/_search", "/_bulk", "/books/_doc/1", "/books/_update/1", "/fresh"] {
            let response = api.answer(&request("POST", path, text));
            assert!(response.status >= 400 || (path == "/books/_search" && text.starts_with("{\"index")), "{path} {text:?} {}", body(&response));
        }
    }
}
