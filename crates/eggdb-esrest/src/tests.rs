use crate::Api;
use eggdb_http::{Request, Response};
use eggdb_keyspace::MemoryKeyspace;

const PASSWORD: &str = "secret";

pub(crate) fn api(keyspace: &MemoryKeyspace) -> Api<'_> {
    Api::new(keyspace, "eggdb", eggdb_crypto::sha256(PASSWORD.as_bytes()))
}

pub(crate) fn request(method: &str, target: &str, body: &str) -> Request {
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (target.to_owned(), None),
    };
    let credentials = eggdb_crypto::base64_encode(format!("eggdb:{PASSWORD}").as_bytes());
    Request {
        method: method.to_owned(),
        path,
        query,
        headers: vec![("authorization".to_owned(), format!("Basic {credentials}"))],
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
    assert!(
        response
            .headers
            .iter()
            .any(|(name, _)| name == "WWW-Authenticate")
    );
    let mut wrong = request("GET", "/", "");
    wrong.headers = vec![(
        "authorization".to_owned(),
        format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:nope")),
    )];
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
    assert_eq!(
        api.answer(&request(
            "PUT",
            "/logs",
            "{\"mappings\":{\"properties\":{\"message\":{\"type\":\"text\"}}}}"
        ))
        .status,
        200
    );
    assert_eq!(api.answer(&request("PUT", "/logs", "")).status, 400);
    assert_eq!(api.answer(&request("HEAD", "/logs", "")).status, 200);
    let described = api.answer(&request("GET", "/logs", ""));
    assert!(
        body(&described).contains("\"message\":{\"type\":\"text\"}"),
        "{}",
        body(&described)
    );
    assert_eq!(api.answer(&request("DELETE", "/logs", "")).status, 200);
    assert_eq!(api.answer(&request("HEAD", "/logs", "")).status, 404);
    assert_eq!(api.answer(&request("DELETE", "/logs", "")).status, 404);
    assert_eq!(api.answer(&request("PUT", "/Bad", "")).status, 400);
}

#[test]
fn documents_are_indexed_read_updated_and_deleted() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    let created = api.answer(&request(
        "PUT",
        "/people/_doc/7",
        "{\"name\":\"Ada\",\"age\":36}",
    ));
    assert_eq!(created.status, 201);
    assert!(body(&created).contains("\"result\":\"created\""));
    let replaced = api.answer(&request(
        "PUT",
        "/people/_doc/7",
        "{\"name\":\"Ada\",\"age\":37}",
    ));
    assert_eq!(replaced.status, 200);
    assert!(body(&replaced).contains("\"_version\":2"));
    let fetched = api.answer(&request("GET", "/people/_doc/7", ""));
    assert!(
        body(&fetched).contains("\"_source\":{\"name\":\"Ada\",\"age\":37}"),
        "{}",
        body(&fetched)
    );
    let updated = api.answer(&request(
        "POST",
        "/people/_update/7",
        "{\"doc\":{\"age\":38,\"city\":\"London\"}}",
    ));
    assert_eq!(updated.status, 200);
    let fetched = api.answer(&request("GET", "/people/_doc/7", ""));
    assert!(
        body(&fetched).contains("\"age\":38") && body(&fetched).contains("\"city\":\"London\"")
    );
    assert_eq!(
        api.answer(&request("PUT", "/people/_create/7", "{}"))
            .status,
        409
    );
    assert_eq!(
        api.answer(&request("DELETE", "/people/_doc/7", "")).status,
        200
    );
    assert_eq!(
        api.answer(&request("GET", "/people/_doc/7", "")).status,
        404
    );
    assert_eq!(
        api.answer(&request("PUT", "/people/_doc/8", "[1,2]"))
            .status,
        400
    );
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
fn a_match_query_ranks_by_relevance_with_the_standard_analyzer() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let unstemmed = body(&api.answer(&request(
        "POST",
        "/books/_search",
        "{\"query\":{\"match\":{\"title\":\"fox\"}}}",
    )));
    assert!(
        unstemmed.contains("\"total\":{\"value\":1,\"relation\":\"eq\"}"),
        "{unstemmed}"
    );
    let response = api.answer(&request(
        "POST",
        "/books/_search",
        "{\"query\":{\"match\":{\"title\":\"quick fox hounds\"}}}",
    ));
    let text = body(&response);
    assert_eq!(response.status, 200, "{text}");
    assert!(
        text.contains("\"total\":{\"value\":2,\"relation\":\"eq\"}"),
        "{text}"
    );
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
    assert!(
        text.contains("{\"key\":\"fable\",\"doc_count\":2}"),
        "{text}"
    );
    assert!(text.contains("\"longest\":{\"value\":300"), "{text}");
    assert!(text.contains("\"average\":{\"value\":166.66"), "{text}");
}

#[test]
fn a_query_string_parameter_searches_every_field() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let text = body(&api.answer(&request("GET", "/books/_search?q=turtle", "")));
    assert!(
        text.contains("\"_id\":\"3\"") && text.contains("\"value\":1,"),
        "{text}"
    );
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
    let response = api.answer(&request(
        "POST",
        "/books/_search",
        "{\"query\":{\"nonsense\":{}}}",
    ));
    assert_eq!(response.status, 400, "{}", body(&response));
    let response = api.answer(&request(
        "POST",
        "/books/_search",
        "{\"from\":9999,\"size\":10}",
    ));
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
    assert!(
        text.contains("\"status\":409") && text.contains("\"result\":\"deleted\""),
        "{text}"
    );
}

#[test]
fn hostile_bodies_return_errors() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let deep = "[".repeat(10_000);
    for text in [
        deep.as_str(),
        "{",
        "\u{0}",
        "{\"query\":",
        "{\"index\":{}}\n",
    ] {
        for path in [
            "/books/_search",
            "/_bulk",
            "/books/_doc/1",
            "/books/_update/1",
            "/fresh",
        ] {
            let response = api.answer(&request("POST", path, text));
            assert!(
                response.status >= 400
                    || (matches!(path, "/books/_search" | "/books/_doc/1")
                        && text.starts_with("{\"index")),
                "{path} {text:?} {}",
                body(&response)
            );
        }
    }
}

#[test]
fn ids_in_urls_are_percent_decoded_and_bulk_checks_index_names() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    let stored = api.answer(&request(
        "PUT",
        "/users/_doc/user%3A1%20a",
        "{\"name\":\"Ada\"}",
    ));
    assert_eq!(stored.status, 201, "{}", body(&stored));
    assert!(
        body(&stored).contains("\"_id\":\"user:1 a\""),
        "{}",
        body(&stored)
    );
    let bulk = "{\"index\":{\"_index\":\"users\",\"_id\":\"user:1 a\"}}\n{\"name\":\"Grace\"}\n";
    assert_eq!(api.answer(&request("POST", "/_bulk", bulk)).status, 200);
    let counted = api.answer(&request("GET", "/users/_count", ""));
    assert!(body(&counted).contains("\"count\":1"), "{}", body(&counted));
    assert_eq!(
        api.answer(&request("GET", "/users/_doc/bad%zz", "")).status,
        400
    );
    let hostile = "{\"index\":{\"_index\":\"users\\u0000b\",\"_id\":\"1\"}}\n{\"x\":1}\n\
                   {\"index\":{\"_index\":\"Upper\",\"_id\":\"1\"}}\n{\"x\":1}\n";
    let answer = body(&api.answer(&request("POST", "/_bulk", hostile)));
    assert!(answer.contains("\"errors\":true"), "{answer}");
    assert_eq!(
        answer.matches("invalid_index_name_exception").count(),
        4,
        "{answer}"
    );
    assert_eq!(answer.matches("\"status\":400").count(), 2, "{answer}");
    let counted = api.answer(&request("GET", "/users/_count", ""));
    assert!(body(&counted).contains("\"count\":1"), "{}", body(&counted));
    assert_eq!(api.answer(&request("PUT", "/_private", "")).status, 400);
}

#[test]
fn a_request_past_the_memory_budget_is_answered_like_a_tripped_circuit_breaker() {
    let response = crate::over_memory_budget();
    assert_eq!(response.status, 429);
    let text = body(&response);
    assert!(
        text.contains("\"type\":\"circuit_breaking_exception\""),
        "{text}"
    );
    assert!(text.contains("\"status\":429"), "{text}");
}

fn seeded_users(keyspace: &MemoryKeyspace) -> Api<'_> {
    let api = api(keyspace);
    let bulk = concat!(
        "{\"index\":{\"_index\":\"my-index-000001\",\"_id\":\"1\"}}\n",
        "{\"user\":{\"id\":\"kimchy\"},\"count\":1,\"message\":\"trying out Elasticsearch\"}\n",
        "{\"index\":{\"_index\":\"my-index-000001\",\"_id\":\"2\"}}\n",
        "{\"user\":{\"id\":\"kimchy\"},\"count\":5,\"message\":\"another test\"}\n",
        "{\"index\":{\"_index\":\"my-index-000001\",\"_id\":\"3\"}}\n",
        "{\"user\":{\"id\":\"elkbee\"},\"count\":2,\"message\":\"a third test\"}\n",
    );
    let response = api.answer(&request("POST", "/_bulk", bulk));
    assert!(
        body(&response).contains("\"errors\":false"),
        "{}",
        body(&response)
    );
    api
}

fn count_of(api: &Api<'_>, index: &str) -> String {
    body(&api.answer(&request("GET", &format!("/{index}/_count"), "")))
}

#[test]
fn mget_returns_each_document_in_order_from_the_reference_examples() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let response = api.answer(&request(
        "GET",
        "/_mget",
        "{\"docs\":[{\"_index\":\"books\",\"_id\":\"1\"},{\"_index\":\"books\",\"_id\":\"9\"},{\"_index\":\"books\",\"_id\":\"2\",\"routing\":\"key2\"}]}",
    ));
    assert_eq!(response.status, 200);
    let text = body(&response);
    assert!(text.starts_with("{\"docs\":[{\"_index\":\"books\",\"_id\":\"1\",\"_version\":1,\"_seq_no\":0,\"_primary_term\":1,\"found\":true,\"_source\":{\"title\":\"The quick brown fox\""), "{text}");
    assert!(
        text.contains("{\"_index\":\"books\",\"_id\":\"9\",\"found\":false}"),
        "{text}"
    );
    assert!(
        text.find("\"_id\":\"9\"") < text.find("\"_id\":\"2\""),
        "{text}"
    );

    let by_index = body(&api.answer(&request(
        "POST",
        "/books/_mget",
        "{\"docs\":[{\"_id\":\"1\"},{\"_id\":\"2\"}]}",
    )));
    assert_eq!(by_index.matches("\"found\":true").count(), 2, "{by_index}");
    let short = body(&api.answer(&request(
        "GET",
        "/books/_mget",
        "{\"ids\":[\"1\",\"3\",\"4\"]}",
    )));
    assert_eq!(short.matches("\"found\":true").count(), 2, "{short}");
    assert_eq!(short.matches("\"found\":false").count(), 1, "{short}");
}

#[test]
fn mget_filters_sources_per_document_and_by_url_parameters() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let text = body(&api.answer(&request(
        "GET",
        "/_mget",
        "{\"docs\":[{\"_index\":\"books\",\"_id\":\"1\",\"_source\":false},{\"_index\":\"books\",\"_id\":\"2\",\"_source\":[\"genre\",\"pages\"]},{\"_index\":\"books\",\"_id\":\"3\",\"_source\":{\"include\":[\"*\"],\"exclude\":[\"title\"]}}]}",
    )));
    assert!(
        text.contains(
            "\"_id\":\"1\",\"_version\":1,\"_seq_no\":0,\"_primary_term\":1,\"found\":true}"
        ),
        "{text}"
    );
    assert!(
        text.contains("\"_source\":{\"genre\":\"nature\",\"pages\":300}"),
        "{text}"
    );
    assert!(
        text.contains("\"_source\":{\"genre\":\"fable\",\"pages\":80}"),
        "{text}"
    );
    let hidden = body(&api.answer(&request(
        "GET",
        "/books/_mget?_source=false",
        "{\"ids\":[\"1\"]}",
    )));
    assert!(!hidden.contains("_source"), "{hidden}");
    let included = body(&api.answer(&request(
        "GET",
        "/books/_mget?_source_includes=title&_source_excludes=pages",
        "{\"ids\":[\"2\"]}",
    )));
    assert!(
        included.contains("\"_source\":{\"title\":\"Foxes and hounds running quickly\"}"),
        "{included}"
    );
}

#[test]
fn mget_reports_missing_indices_per_document_and_bad_requests_as_validation_errors() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let response = api.answer(&request(
        "GET",
        "/_mget",
        "{\"docs\":[{\"_index\":\"nowhere\",\"_id\":\"1\"},{\"_index\":\"books\",\"_id\":\"1\"}]}",
    ));
    assert_eq!(response.status, 200);
    let text = body(&response);
    assert!(text.contains("{\"_index\":\"nowhere\",\"_id\":\"1\",\"error\":{\"root_cause\":[{\"type\":\"index_not_found_exception\""), "{text}");
    assert!(text.contains("\"found\":true"), "{text}");
    for (target, request_body, reason) in [
        ("/_mget", "{\"ids\":[\"1\"]}", "index is missing for doc 0"),
        (
            "/_mget",
            "{\"docs\":[{\"_id\":\"1\"},{\"_index\":\"books\"}]}",
            "1: index is missing for doc 0;2: id is missing for doc 1;",
        ),
        ("/books/_mget", "{\"docs\":[]}", "no documents to get"),
        ("/books/_mget", "", "no documents to get"),
    ] {
        let refused = api.answer(&request("POST", target, request_body));
        assert_eq!(refused.status, 400, "{request_body}");
        let text = body(&refused);
        assert!(
            text.contains("action_request_validation_exception"),
            "{text}"
        );
        assert!(text.contains(reason), "{text}");
    }
    for hostile in [
        "{\"docs\":",
        "{\"docs\":{}}",
        "{\"docs\":[1]}",
        "{\"ids\":[{}]}",
        "{\"docs\":[{\"_id\":\"1\",\"_source\":7}]}",
    ] {
        assert_eq!(
            api.answer(&request("POST", "/books/_mget", hostile)).status,
            400,
            "{hostile}"
        );
    }
}

#[test]
fn msearch_answers_each_search_with_its_own_status_from_the_reference_example() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let searches = concat!(
        "{ }\n",
        "{\"query\":{\"match\":{\"title\":\"fox\"}}}\n",
        "{\"index\":\"books\"}\n",
        "{\"query\":{\"term\":{\"genre\":\"fable\"}},\"size\":1}\n",
        "{\"index\":\"missing\"}\n",
        "{\"query\":{\"match_all\":{}}}\n",
        "{\"index\":[\"books\"]}\n",
        "{\"query\":{\"bogus\":{}}}\n",
        "{\"index\":\"missing\",\"ignore_unavailable\":true}\n",
        "{}\n",
    );
    let response = api.answer(&request("GET", "/books/_msearch", searches));
    assert_eq!(response.status, 200);
    let text = body(&response);
    assert!(text.starts_with("{\"took\":"), "{text}");
    assert!(text.contains("\"responses\":[{\"took\":0"), "{text}");
    assert_eq!(text.matches("\"status\":200}").count(), 3, "{text}");
    assert!(
        text.contains("\"total\":{\"value\":1,\"relation\":\"eq\"}"),
        "{text}"
    );
    assert!(
        text.contains("\"total\":{\"value\":2,\"relation\":\"eq\"}"),
        "{text}"
    );
    assert!(
        text.contains("\"total\":{\"value\":0,\"relation\":\"eq\"}"),
        "{text}"
    );
    assert!(
        text.contains("\"type\":\"index_not_found_exception\""),
        "{text}"
    );
    assert!(text.contains("\"status\":404}"), "{text}");
    assert!(text.contains("\"type\":\"parsing_exception\""), "{text}");
    assert!(text.contains("\"status\":400}"), "{text}");

    let everywhere = body(&api.answer(&request(
        "POST",
        "/_msearch",
        "{}\r\n{\"query\":{\"match_all\":{}}}\r\n",
    )));
    assert!(everywhere.contains("\"value\":3"), "{everywhere}");
}

#[test]
fn msearch_refuses_malformed_bodies_as_a_whole() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    for (searches, kind) in [
        ("", "action_request_validation_exception"),
        ("\n\n", "action_request_validation_exception"),
        ("{}\n", "illegal_argument_exception"),
        ("{}\n{\"query\":\n", "parse_exception"),
        ("[1]\n{}\n", "illegal_argument_exception"),
        ("{\"index\n{}\n", "parse_exception"),
    ] {
        let response = api.answer(&request("POST", "/_msearch", searches));
        assert_eq!(response.status, 400, "{searches:?}");
        assert!(
            body(&response).contains(kind),
            "{searches:?}: {}",
            body(&response)
        );
    }
    let too_many = api.answer(&request("POST", "/_msearch", &"{}\n{}\n".repeat(1001)));
    assert_eq!(too_many.status, 400);
    assert!(body(&too_many).contains("at most 1000 searches"));
    let most = api.answer(&request("POST", "/_msearch", &"{}\n{}\n".repeat(1000)));
    assert_eq!(most.status, 200);
    let bad_name = body(&api.answer(&request("POST", "/_msearch", "{\"index\":\"Bad\"}\n{}\n")));
    assert!(
        bad_name.contains("invalid_index_name_exception"),
        "{bad_name}"
    );
}

#[test]
fn update_by_query_runs_a_script_on_matching_documents_from_the_reference_example() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded_users(&keyspace);
    let response = api.answer(&request(
        "POST",
        "/my-index-000001/_update_by_query?conflicts=proceed&refresh=true",
        "{\"script\":{\"source\":\"ctx._source.count++\",\"lang\":\"painless\"},\"query\":{\"term\":{\"user.id\":\"kimchy\"}}}",
    ));
    assert_eq!(response.status, 200, "{}", body(&response));
    let text = body(&response);
    assert!(text.contains("\"timed_out\":false,\"total\":2,\"updated\":2,\"deleted\":0,\"batches\":1,\"version_conflicts\":0,\"noops\":0,\"retries\":{\"bulk\":0,\"search\":0},\"throttled_millis\":0,\"requests_per_second\":-1.0,\"throttled_until_millis\":0,\"failures\":[]}"), "{text}");
    let first = body(&api.answer(&request("GET", "/my-index-000001/_doc/1", "")));
    assert!(first.contains("\"_version\":2"), "{first}");
    assert!(first.contains("\"count\":2"), "{first}");
    let second = body(&api.answer(&request("GET", "/my-index-000001/_doc/2", "")));
    assert!(second.contains("\"count\":6"), "{second}");
    let untouched = body(&api.answer(&request("GET", "/my-index-000001/_doc/3", "")));
    assert!(untouched.contains("\"_version\":1"), "{untouched}");
    assert!(untouched.contains("\"count\":2"), "{untouched}");
}

#[test]
fn update_by_query_without_a_script_rewrites_and_scripts_can_skip_or_delete() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded_users(&keyspace);
    let all = body(&api.answer(&request(
        "POST",
        "/my-index-000001/_update_by_query?conflicts=proceed",
        "",
    )));
    assert!(all.contains("\"total\":3,\"updated\":3"), "{all}");
    let rewritten = body(&api.answer(&request("GET", "/my-index-000001/_doc/3", "")));
    assert!(rewritten.contains("\"_version\":2"), "{rewritten}");

    let mixed = body(&api.answer(&request(
        "POST",
        "/my-index-000001/_update_by_query",
        "{\"script\":{\"source\":\"ctx.op = params.op\",\"params\":{\"op\":\"noop\"}},\"query\":{\"term\":{\"user.id\":\"kimchy\"}}}",
    )));
    assert!(
        mixed.contains("\"total\":2,\"updated\":0,\"deleted\":0"),
        "{mixed}"
    );
    assert!(mixed.contains("\"noops\":2"), "{mixed}");
    let deleted = body(&api.answer(&request(
        "POST",
        "/my-index-000001/_update_by_query?q=third",
        "{\"script\":\"ctx.op = 'delete'\"}",
    )));
    assert!(
        deleted.contains("\"total\":1,\"updated\":0,\"deleted\":1"),
        "{deleted}"
    );
    assert!(count_of(&api, "my-index-000001").contains("\"count\":2"));

    let limited = body(&api.answer(&request(
        "POST",
        "/my-index-000001/_update_by_query?max_docs=1&scroll_size=1",
        "{\"script\":\"ctx._source.count += 10\"}",
    )));
    assert!(limited.contains("\"total\":1,\"updated\":1"), "{limited}");
    assert!(limited.contains("\"batches\":1"), "{limited}");
}

#[test]
fn a_failing_script_changes_nothing_and_reports_the_failure() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded_users(&keyspace);
    let response = api.answer(&request(
        "POST",
        "/my-index-000001/_update_by_query",
        "{\"script\":\"ctx._source.count += 1; ctx._source.missing.inner = 2\"}",
    ));
    assert_eq!(response.status, 400, "{}", body(&response));
    let text = body(&response);
    assert!(text.contains("\"updated\":0"), "{text}");
    assert!(text.contains("\"failures\":[{\"index\":\"my-index-000001\",\"id\":\"1\",\"cause\":{\"type\":\"script_exception\""), "{text}");
    let unchanged = body(&api.answer(&request("GET", "/my-index-000001/_doc/2", "")));
    assert!(unchanged.contains("\"count\":5"), "{unchanged}");
    assert!(unchanged.contains("\"_version\":1"), "{unchanged}");

    let compile = api.answer(&request(
        "POST",
        "/my-index-000001/_update_by_query",
        "{\"script\":\"while (true) {}\"}",
    ));
    assert_eq!(compile.status, 400);
    assert!(body(&compile).contains("script_exception"));
}

#[test]
fn delete_by_query_deletes_matching_documents_from_the_reference_examples() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded_users(&keyspace);
    let response = api.answer(&request(
        "POST",
        "/my-index-000001/_delete_by_query",
        "{\"query\":{\"match\":{\"user.id\":\"elkbee\"}}}",
    ));
    assert_eq!(response.status, 200, "{}", body(&response));
    let text = body(&response);
    assert!(text.contains("\"timed_out\":false,\"total\":1,\"deleted\":1,\"batches\":1,\"version_conflicts\":0,\"noops\":0"), "{text}");
    assert!(!text.contains("\"updated\""), "{text}");
    assert!(count_of(&api, "my-index-000001").contains("\"count\":2"));

    let limited = body(&api.answer(&request(
        "POST",
        "/my-index-000001/_delete_by_query?scroll_size=5000",
        "{\"query\":{\"term\":{\"user.id\":\"kimchy\"}},\"max_docs\":1}",
    )));
    assert!(limited.contains("\"total\":1,\"deleted\":1"), "{limited}");
    assert!(count_of(&api, "my-index-000001").contains("\"count\":1"));

    let rest = body(&api.answer(&request(
        "POST",
        "/my-index-000001/_delete_by_query?conflicts=proceed",
        "{\"query\":{\"match_all\":{}}}",
    )));
    assert!(rest.contains("\"deleted\":1"), "{rest}");
    assert!(count_of(&api, "my-index-000001").contains("\"count\":0"));
    let nothing = body(&api.answer(&request("POST", "/my-index-000001/_delete_by_query", "")));
    assert!(
        nothing.contains("\"total\":0,\"deleted\":0,\"batches\":0"),
        "{nothing}"
    );
}

#[test]
fn by_query_requests_refuse_bad_options() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded_users(&keyspace);
    assert_eq!(
        api.answer(&request("POST", "/nowhere/_delete_by_query", ""))
            .status,
        404
    );
    for (target, request_body) in [
        ("/my-index-000001/_delete_by_query?conflicts=maybe", ""),
        (
            "/my-index-000001/_delete_by_query",
            "{\"conflicts\":\"maybe\"}",
        ),
        ("/my-index-000001/_delete_by_query", "{\"max_docs\":0}"),
        ("/my-index-000001/_delete_by_query", "{\"max_docs\":-1}"),
        ("/my-index-000001/_delete_by_query?max_docs=x", ""),
        ("/my-index-000001/_delete_by_query?scroll_size=0", ""),
        (
            "/my-index-000001/_delete_by_query",
            "{\"script\":\"ctx.op = 'delete'\"}",
        ),
        (
            "/my-index-000001/_delete_by_query",
            "{\"query\":{\"bogus\":{}}}",
        ),
        ("/my-index-000001/_delete_by_query", "{\"query\":"),
        ("/my-index-000001/_delete_by_query", "[]"),
        ("/my-index-000001/_update_by_query", "{\"doc\":{\"a\":1}}"),
        (
            "/my-index-000001/_update_by_query",
            "{\"script\":{\"id\":\"stored\"}}",
        ),
    ] {
        let response = api.answer(&request("POST", target, request_body));
        assert_eq!(
            response.status,
            400,
            "{target} {request_body}: {}",
            body(&response)
        );
        assert!(
            body(&response).contains("\"error\":{\"root_cause\""),
            "{}",
            body(&response)
        );
    }
    assert!(count_of(&api, "my-index-000001").contains("\"count\":3"));
}

#[test]
fn update_and_bulk_update_run_scripts() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    assert_eq!(
        api.answer(&request(
            "PUT",
            "/test/_doc/1",
            "{\"counter\":1,\"tags\":[\"red\"]}"
        ))
        .status,
        201
    );
    let response = api.answer(&request(
        "POST",
        "/test/_update/1",
        "{\"script\":{\"source\":\"ctx._source.counter += params.count\",\"lang\":\"painless\",\"params\":{\"count\":4}}}",
    ));
    assert_eq!(response.status, 200, "{}", body(&response));
    assert!(body(&response).contains("\"_version\":2,\"result\":\"updated\""));
    let bulk = concat!(
        "{\"update\":{\"_id\":\"1\",\"_index\":\"test\"}}\n",
        "{\"script\":{\"source\":\"ctx._source.tags.add(params['tag'])\",\"lang\":\"painless\",\"params\":{\"tag\":\"blue\"}}}\n",
    );
    assert!(body(&api.answer(&request("POST", "/_bulk", bulk))).contains("\"errors\":false"));
    let document = body(&api.answer(&request("GET", "/test/_doc/1", "")));
    assert!(
        document.contains("\"_source\":{\"counter\":5,\"tags\":[\"red\",\"blue\"]}"),
        "{document}"
    );
    let noop = body(&api.answer(&request(
        "POST",
        "/test/_update/1",
        "{\"script\":\"ctx.op = 'noop'\"}",
    )));
    assert!(
        noop.contains("\"_version\":3,\"result\":\"noop\""),
        "{noop}"
    );
    let failed = api.answer(&request(
        "POST",
        "/test/_update/1",
        "{\"script\":\"ctx._source.tags++\"}",
    ));
    assert_eq!(failed.status, 400);
    assert!(body(&failed).contains("script_exception"));
    let deleted = body(&api.answer(&request(
        "POST",
        "/test/_update/1",
        "{\"script\":\"ctx.op = 'delete'\"}",
    )));
    assert!(deleted.contains("\"result\":\"deleted\""), "{deleted}");
    assert_eq!(api.answer(&request("GET", "/test/_doc/1", "")).status, 404);
}

fn number_after(text: &str, key: &str) -> u64 {
    let start = text.find(key).map_or(text.len(), |at| at + key.len());
    text.get(start..)
        .unwrap_or_default()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or(u64::MAX)
}

#[test]
fn manual_slices_split_the_matching_documents_between_them() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    let mut bulk = String::new();
    for id in 0..20 {
        bulk.push_str(&format!(
            "{{\"index\":{{\"_index\":\"counters\",\"_id\":\"{id}\"}}}}\n{{\"n\":0}}\n"
        ));
    }
    assert!(body(&api.answer(&request("POST", "/_bulk", &bulk))).contains("\"errors\":false"));
    let mut updated = 0;
    for slice in 0..2 {
        let text = body(&api.answer(&request(
            "POST",
            "/counters/_update_by_query",
            &format!("{{\"slice\":{{\"id\":{slice},\"max\":2}},\"script\":\"ctx._source.n++\"}}"),
        )));
        let count = number_after(&text, "\"updated\":");
        assert!(count < 20, "{text}");
        updated += count;
    }
    assert_eq!(updated, 20);
    for id in 0..20 {
        let document = body(&api.answer(&request("GET", &format!("/counters/_doc/{id}"), "")));
        assert!(document.contains("\"_source\":{\"n\":1}"), "{document}");
    }
    let mut deleted = 0;
    for slice in 0..3 {
        let text = body(&api.answer(&request(
            "POST",
            "/counters/_delete_by_query",
            &format!("{{\"slice\":{{\"id\":{slice},\"max\":3}}}}"),
        )));
        deleted += number_after(&text, "\"deleted\":");
    }
    assert_eq!(deleted, 20);
    for refused in [
        "{\"slice\":{\"id\":0,\"max\":1}}",
        "{\"slice\":{\"id\":2,\"max\":2}}",
        "{\"slice\":{\"id\":-1,\"max\":2}}",
        "{\"slice\":{\"max\":2}}",
        "{\"slice\":{\"id\":0,\"max\":2,\"other\":1}}",
        "{\"slice\":2}",
    ] {
        let response = api.answer(&request("POST", "/counters/_delete_by_query", refused));
        assert_eq!(response.status, 400, "{refused}: {}", body(&response));
    }
}

#[test]
fn msearch_headers_expand_wildcard_index_patterns() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    let bulk = concat!(
        "{\"index\":{\"_index\":\"logs-a\",\"_id\":\"1\"}}\n{\"m\":1}\n",
        "{\"index\":{\"_index\":\"logs-b\",\"_id\":\"2\"}}\n{\"m\":2}\n",
        "{\"index\":{\"_index\":\"metrics\",\"_id\":\"3\"}}\n{\"m\":3}\n",
    );
    assert!(body(&api.answer(&request("POST", "/_bulk", bulk))).contains("\"errors\":false"));
    let searches = concat!(
        "{\"index\":\"logs-*\"}\n{}\n",
        "{\"index\":[\"logs-a\",\"met*\"]}\n{}\n",
        "{\"index\":\"nothing-*\"}\n{}\n",
        "{\"index\":\"Bad*\"}\n{}\n",
    );
    let text = body(&api.answer(&request("POST", "/_msearch", searches)));
    let totals: Vec<u64> = text
        .match_indices("\"total\":{\"value\":")
        .map(|(at, key)| number_after(&text[at..], key))
        .collect();
    assert_eq!(totals, vec![2, 2, 0], "{text}");
    assert!(text.contains("invalid_index_name_exception"), "{text}");
}

#[test]
fn update_refuses_both_a_script_and_a_doc() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    assert_eq!(
        api.answer(&request("PUT", "/test/_doc/1", "{\"n\":1}"))
            .status,
        201
    );
    let response = api.answer(&request(
        "POST",
        "/test/_update/1",
        "{\"script\":\"ctx._source.n++\",\"doc\":{\"n\":5}}",
    ));
    assert_eq!(response.status, 400);
    assert!(
        body(&response).contains("can't provide both script and doc"),
        "{}",
        body(&response)
    );
    let document = body(&api.answer(&request("GET", "/test/_doc/1", "")));
    assert!(document.contains("\"_source\":{\"n\":1}"), "{document}");
}

#[test]
fn scripts_that_would_corrupt_or_explode_a_document_are_refused() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    assert_eq!(
        api.answer(&request("PUT", "/test/_doc/1", "{\"s\":\"x\"}"))
            .status,
        201
    );
    let doubling = format!(
        "{{\"script\":\"{}\"}}",
        "ctx._source.s += ctx._source.s; ".repeat(40)
    );
    let response = api.answer(&request("POST", "/test/_update/1", &doubling));
    assert_eq!(response.status, 400, "{}", body(&response));
    assert!(body(&response).contains("script_exception"));
    let literal = api.answer(&request(
        "POST",
        "/test/_update/1",
        "{\"script\":\"ctx._source.a = 1.\"}",
    ));
    assert_eq!(literal.status, 200, "{}", body(&literal));
    let document = body(&api.answer(&request("GET", "/test/_doc/1", "")));
    assert!(
        document.contains("\"_source\":{\"s\":\"x\",\"a\":1.0}"),
        "{document}"
    );
    assert!(eggdb_sql::json::parse_document(&document).is_ok());
}

#[test]
fn update_upserts_a_missing_document_from_the_reference_examples() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    let with_upsert = "{\"script\":{\"source\":\"ctx._source.counter += params.count\",\"lang\":\"painless\",\"params\":{\"count\":4}},\"upsert\":{\"counter\":1}}";
    let created = api.answer(&request("POST", "/test/_update/1", with_upsert));
    assert_eq!(created.status, 201, "{}", body(&created));
    assert!(
        body(&created).contains("\"_version\":1,\"result\":\"created\""),
        "{}",
        body(&created)
    );
    let document = body(&api.answer(&request("GET", "/test/_doc/1", "")));
    assert!(
        document.contains("\"_source\":{\"counter\":1}"),
        "{document}"
    );
    let updated = api.answer(&request("POST", "/test/_update/1", with_upsert));
    assert_eq!(updated.status, 200, "{}", body(&updated));
    assert!(body(&updated).contains("\"_version\":2,\"result\":\"updated\""));
    let document = body(&api.answer(&request("GET", "/test/_doc/1", "")));
    assert!(
        document.contains("\"_source\":{\"counter\":5}"),
        "{document}"
    );

    let as_upsert = "{\"doc\":{\"name\":\"new_name\"},\"doc_as_upsert\":true}";
    let created = api.answer(&request("POST", "/test/_update/2", as_upsert));
    assert_eq!(created.status, 201, "{}", body(&created));
    let document = body(&api.answer(&request("GET", "/test/_doc/2", "")));
    assert!(
        document.contains("\"_source\":{\"name\":\"new_name\"}"),
        "{document}"
    );
    let again = body(&api.answer(&request("POST", "/test/_update/2", as_upsert)));
    assert!(
        again.contains("\"_version\":1,\"result\":\"noop\",\"_shards\":{\"total\":0,\"successful\":0,\"failed\":0}"),
        "{again}"
    );

    let scripted = "{\"scripted_upsert\":true,\"script\":{\"source\":\"ctx._source.counter = params.count\",\"params\":{\"count\":4}},\"upsert\":{}}";
    let created = api.answer(&request("POST", "/test/_update/3", scripted));
    assert_eq!(created.status, 201, "{}", body(&created));
    let document = body(&api.answer(&request("GET", "/test/_doc/3", "")));
    assert!(
        document.contains("\"_source\":{\"counter\":4}"),
        "{document}"
    );
    let skipped = api.answer(&request(
        "POST",
        "/test/_update/4",
        "{\"scripted_upsert\":true,\"script\":\"ctx.op = 'none'\",\"upsert\":{\"a\":1}}",
    ));
    assert_eq!(skipped.status, 200, "{}", body(&skipped));
    assert!(
        body(&skipped).contains("\"_version\":-1,\"result\":\"noop\""),
        "{}",
        body(&skipped)
    );
    assert_eq!(api.answer(&request("GET", "/test/_doc/4", "")).status, 404);
    let refused = api.answer(&request(
        "POST",
        "/test/_update/4",
        "{\"scripted_upsert\":true,\"script\":\"ctx.op = 'delete'\",\"upsert\":{}}",
    ));
    assert_eq!(refused.status, 400, "{}", body(&refused));
    assert!(body(&refused).contains("script_exception"));
    assert_eq!(api.answer(&request("GET", "/test/_doc/4", "")).status, 404);
    let plain = api.answer(&request(
        "POST",
        "/test/_update/5",
        "{\"script\":\"ctx._source.n++\",\"upsert\":{\"n\":0}}",
    ));
    assert_eq!(plain.status, 201, "{}", body(&plain));
    let document = body(&api.answer(&request("GET", "/test/_doc/5", "")));
    assert!(document.contains("\"_source\":{\"n\":0}"), "{document}");

    let missing = api.answer(&request("POST", "/test/_update/6", "{\"doc\":{\"a\":1}}"));
    assert_eq!(missing.status, 404);
    assert!(body(&missing).contains("document_missing_exception"));
    let fresh = api.answer(&request(
        "POST",
        "/fresh/_update/1",
        "{\"doc\":{\"a\":1},\"doc_as_upsert\":true}",
    ));
    assert_eq!(fresh.status, 201, "{}", body(&fresh));
    assert_eq!(api.answer(&request("HEAD", "/fresh", "")).status, 200);
}

#[test]
fn update_validates_upsert_options_as_elasticsearch_does() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    let refused = api.answer(&request(
        "POST",
        "/test/_update/1",
        "{\"doc_as_upsert\":true,\"script\":\"ctx.op = 'noop'\"}",
    ));
    assert_eq!(refused.status, 400);
    assert!(
        body(&refused)
            .contains("Validation Failed: 1: doc must be specified if doc_as_upsert is enabled;"),
        "{}",
        body(&refused)
    );
    let refused = api.answer(&request("POST", "/test/_update/1", "{\"upsert\":{}}"));
    assert!(
        body(&refused).contains("Validation Failed: 1: script or doc is missing;"),
        "{}",
        body(&refused)
    );
    for bad in [
        "{\"doc\":{},\"detect_noop\":5}",
        "{\"doc\":5}",
        "{\"doc\":{},\"upsert\":[1]}",
        "{\"doc\":{},\"_source\":5}",
        "[1]",
    ] {
        let response = api.answer(&request("POST", "/test/_update/1", bad));
        assert_eq!(response.status, 400, "{bad} {}", body(&response));
    }
    assert_eq!(api.answer(&request("HEAD", "/test", "")).status, 404);
}

#[test]
fn a_doc_update_merges_nested_objects_and_detects_noops() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    assert_eq!(
        api.answer(&request(
            "PUT",
            "/test/_doc/1",
            "{\"user\":{\"name\":\"Ada\",\"age\":36},\"tags\":[\"x\"],\"n\":1}"
        ))
        .status,
        201
    );
    let updated = body(&api.answer(&request(
        "POST",
        "/test/_update/1",
        "{\"doc\":{\"user\":{\"age\":37}}}",
    )));
    assert!(
        updated.contains("\"_version\":2,\"result\":\"updated\""),
        "{updated}"
    );
    let document = body(&api.answer(&request("GET", "/test/_doc/1", "")));
    assert!(
        document.contains(
            "\"_source\":{\"user\":{\"name\":\"Ada\",\"age\":37},\"tags\":[\"x\"],\"n\":1}"
        ),
        "{document}"
    );
    let unchanged = body(&api.answer(&request(
        "POST",
        "/test/_update/1",
        "{\"doc\":{\"user\":{\"age\":37},\"n\":1,\"tags\":[\"x\"]}}",
    )));
    assert!(
        unchanged.contains("\"_version\":2,\"result\":\"noop\",\"_shards\":{\"total\":0,\"successful\":0,\"failed\":0},\"_seq_no\":1,\"_primary_term\":1"),
        "{unchanged}"
    );
    let forced = body(&api.answer(&request(
        "POST",
        "/test/_update/1",
        "{\"doc\":{\"user\":{\"age\":37}},\"detect_noop\":false}",
    )));
    assert!(
        forced.contains("\"_version\":3,\"result\":\"updated\""),
        "{forced}"
    );
    let bulk = concat!(
        "{\"update\":{\"_index\":\"test\",\"_id\":\"1\"}}\n",
        "{\"doc\":{\"user\":{\"city\":\"London\"}}}\n",
        "{\"update\":{\"_index\":\"test\",\"_id\":\"2\"}}\n",
        "{\"doc\":{\"n\":2},\"doc_as_upsert\":true}\n",
        "{\"update\":{\"_index\":\"test\",\"_id\":\"1\"}}\n",
        "{\"doc\":{\"user\":{\"city\":\"London\"}}}\n",
    );
    let answer = body(&api.answer(&request("POST", "/_bulk", bulk)));
    assert!(answer.contains("\"errors\":false"), "{answer}");
    assert!(
        answer.contains("\"result\":\"created\"") && answer.contains("\"status\":201"),
        "{answer}"
    );
    assert!(answer.contains("\"result\":\"noop\""), "{answer}");
    let document = body(&api.answer(&request("GET", "/test/_doc/1", "")));
    assert!(
        document.contains("\"user\":{\"name\":\"Ada\",\"age\":37,\"city\":\"London\"}"),
        "{document}"
    );
    let decimal = body(&api.answer(&request("POST", "/test/_update/1", "{\"doc\":{\"n\":1.0}}")));
    assert!(decimal.contains("\"result\":\"updated\""), "{decimal}");
}

#[test]
fn update_returns_the_new_source_when_asked() {
    let keyspace = MemoryKeyspace::default();
    let api = api(&keyspace);
    assert_eq!(
        api.answer(&request("PUT", "/test/_doc/1", "{\"a\":1,\"b\":2}"))
            .status,
        201
    );
    let filtered = body(&api.answer(&request(
        "POST",
        "/test/_update/1?_source=a",
        "{\"doc\":{\"b\":3}}",
    )));
    assert!(
        filtered.contains(
            "\"get\":{\"_seq_no\":1,\"_primary_term\":1,\"found\":true,\"_source\":{\"a\":1}}"
        ),
        "{filtered}"
    );
    let whole = body(&api.answer(&request(
        "POST",
        "/test/_update/1",
        "{\"doc\":{\"b\":4},\"_source\":true}",
    )));
    assert!(whole.contains("\"_source\":{\"a\":1,\"b\":4}}"), "{whole}");
    let plain = body(&api.answer(&request("POST", "/test/_update/1", "{\"doc\":{\"b\":5}}")));
    assert!(!plain.contains("\"get\""), "{plain}");
}

#[test]
fn search_after_pages_through_a_multi_field_sort() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let page = |query: &str| body(&api.answer(&request("POST", "/books/_search", query)));
    let first = page("{\"size\":1,\"sort\":[{\"pages\":\"desc\"},\"_doc\"]}");
    assert!(
        first.contains("\"_id\":\"2\",\"_score\":null")
            && first.contains("\"sort\":[300,1]")
            && first.contains("\"total\":{\"value\":3,"),
        "{first}"
    );
    let second =
        page("{\"size\":1,\"sort\":[{\"pages\":\"desc\"},\"_doc\"],\"search_after\":[300,1]}");
    assert!(
        second.contains("\"_id\":\"1\"") && second.contains("\"sort\":[120,0]"),
        "{second}"
    );
    let third =
        page("{\"size\":1,\"sort\":[{\"pages\":\"desc\"},\"_doc\"],\"search_after\":[120,0]}");
    assert!(
        third.contains("\"_id\":\"3\"") && third.contains("\"sort\":[80,2]"),
        "{third}"
    );
    let done =
        page("{\"size\":1,\"sort\":[{\"pages\":\"desc\"},\"_doc\"],\"search_after\":[80,2]}");
    assert!(
        done.contains("\"hits\":[]") && done.contains("\"value\":3,"),
        "{done}"
    );
    for (query, reason) in [
        (
            "{\"from\":1,\"sort\":[\"pages\"],\"search_after\":[1]}",
            "`from` parameter must be set to 0 when `search_after` is used.",
        ),
        (
            "{\"sort\":[\"pages\",\"_doc\"],\"search_after\":[1]}",
            "search_after has 1 value(s) but sort has 2.",
        ),
        (
            "{\"search_after\":[1]}",
            "Sort must contain at least one field.",
        ),
        (
            "{\"sort\":[\"pages\"],\"search_after\":[\"many\"]}",
            "Failed to parse search_after value for field [pages].",
        ),
        ("{\"sort\":[{\"pages\":\"up\"}]}", "Unknown SortOrder [up]"),
        ("{\"sort\":[\"pages\"],\"search_after\":5}", "search_after"),
    ] {
        let response = api.answer(&request("POST", "/books/_search", query));
        assert_eq!(response.status, 400, "{query} {}", body(&response));
        assert!(
            body(&response).contains(reason),
            "{query} {}",
            body(&response)
        );
    }
}

#[test]
fn sort_orders_text_then_numbers_with_missing_values_last() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    assert_eq!(
        api.answer(&request(
            "PUT",
            "/books/_doc/4",
            "{\"title\":\"Untitled\",\"genre\":\"nature\"}"
        ))
        .status,
        201
    );
    let position = |text: &str, id: &str| {
        text.find(&format!("\"_id\":\"{id}\""))
            .unwrap_or(usize::MAX)
    };
    let text = body(&api.answer(&request(
        "POST",
        "/books/_search",
        "{\"sort\":[{\"genre.keyword\":\"asc\"},{\"pages\":{\"order\":\"desc\"}}]}",
    )));
    assert!(
        position(&text, "1") < position(&text, "3")
            && position(&text, "3") < position(&text, "2")
            && position(&text, "2") < position(&text, "4"),
        "{text}"
    );
    assert!(text.contains("\"sort\":[\"fable\",120]"), "{text}");
    assert!(
        text.contains("\"sort\":[\"nature\",-9223372036854775808]"),
        "{text}"
    );
    let first = body(&api.answer(&request(
        "POST",
        "/books/_search",
        "{\"sort\":[{\"pages\":{\"order\":\"asc\",\"missing\":\"_first\"}}]}",
    )));
    assert!(
        position(&first, "4") < position(&first, "3")
            && first.contains("\"sort\":[-9223372036854775808]"),
        "{first}"
    );
    let flagged = body(&api.answer(&request("GET", "/books/_search?sort=pages:desc", "")));
    assert!(
        position(&flagged, "2") < position(&flagged, "1")
            && position(&flagged, "1") < position(&flagged, "3")
            && position(&flagged, "3") < position(&flagged, "4"),
        "{flagged}"
    );
    let scored = body(&api.answer(&request(
        "POST",
        "/books/_search",
        "{\"sort\":[\"pages\"],\"track_scores\":true}",
    )));
    assert!(!scored.contains("\"_score\":null"), "{scored}");
}

#[test]
fn search_filters_each_hit_source() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let titles = body(&api.answer(&request(
        "POST",
        "/books/_search",
        "{\"_source\":[\"title\"],\"size\":1,\"sort\":[\"_doc\"]}",
    )));
    assert!(
        titles.contains("\"_source\":{\"title\":\"The quick brown fox\"}"),
        "{titles}"
    );
    let excluded = body(&api.answer(&request(
        "POST",
        "/books/_search",
        "{\"_source\":{\"excludes\":[\"title\"]},\"size\":1,\"sort\":[\"_doc\"]}",
    )));
    assert!(
        excluded.contains("\"_source\":{\"genre\":\"fable\",\"pages\":120}"),
        "{excluded}"
    );
    let hidden = body(&api.answer(&request("GET", "/books/_search?_source=false", "")));
    assert!(
        !hidden.contains("_source") && hidden.contains("\"_id\":\"1\""),
        "{hidden}"
    );
    assert_eq!(
        api.answer(&request("POST", "/books/_search", "{\"_source\":5}"))
            .status,
        400
    );
}

#[test]
fn get_doc_and_get_source_filter_the_source_by_url_parameters() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let hidden = body(&api.answer(&request("GET", "/books/_doc/1?_source=false", "")));
    assert!(!hidden.contains("_source") && hidden.contains("\"found\":true"), "{hidden}");
    let picked = body(&api.answer(&request(
        "GET",
        "/books/_doc/2?_source_includes=genre,pages",
        "",
    )));
    assert!(
        picked.contains("\"_source\":{\"genre\":\"nature\",\"pages\":300}"),
        "{picked}"
    );
    let source = body(&api.answer(&request("GET", "/books/_source/2?_source=genre", "")));
    assert_eq!(source, "{\"genre\":\"nature\"}");
    let missing = api.answer(&request("GET", "/books/_source/99", ""));
    assert_eq!(missing.status, 404);
}

#[test]
fn if_seq_no_and_if_primary_term_guard_index_update_and_delete() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let stale = api.answer(&request(
        "PUT",
        "/books/_doc/1?if_seq_no=5&if_primary_term=1",
        "{\"title\":\"x\"}",
    ));
    assert_eq!(stale.status, 409);
    assert!(body(&stale).contains("version_conflict_engine_exception"));
    let fresh = api.answer(&request(
        "PUT",
        "/books/_doc/1?if_seq_no=0&if_primary_term=1",
        "{\"title\":\"x\"}",
    ));
    assert_eq!(fresh.status, 200, "{}", body(&fresh));
    let half = api.answer(&request("DELETE", "/books/_doc/1?if_seq_no=1", ""));
    assert_eq!(half.status, 400);
    let update = api.answer(&request(
        "POST",
        "/books/_update/1?if_seq_no=0&if_primary_term=1&retry_on_conflict=3",
        "{\"doc\":{\"title\":\"y\"}}",
    ));
    assert_eq!(update.status, 409);
    let deleted = api.answer(&request("DELETE", "/books/_doc/1?if_seq_no=1&if_primary_term=1", ""));
    assert_eq!(deleted.status, 200, "{}", body(&deleted));
}

#[test]
fn search_and_by_query_take_lists_and_patterns_of_indices() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    api.answer(&request("PUT", "/bookmarks/_doc/9", "{\"title\":\"fox notes\"}"));
    let counted = body(&api.answer(&request("GET", "/books,bookmarks/_count", "")));
    assert!(counted.contains("\"count\":4"), "{counted}");
    let pattern = body(&api.answer(&request("GET", "/book*/_count", "")));
    assert!(pattern.contains("\"count\":4"), "{pattern}");
    let excluded = body(&api.answer(&request("GET", "/book*,-bookmarks/_count", "")));
    assert!(excluded.contains("\"count\":3"), "{excluded}");
    let deleted = body(&api.answer(&request(
        "POST",
        "/book*/_delete_by_query",
        "{\"query\":{\"match\":{\"title\":\"fox\"}}}",
    )));
    assert!(deleted.contains("\"deleted\":2"), "{deleted}");
    let missing = api.answer(&request("POST", "/books,nowhere/_delete_by_query", "{}"));
    assert_eq!(missing.status, 404);
}

#[test]
fn search_templates_render_mustache_and_run_the_search() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let found = body(&api.answer(&request(
        "POST",
        "/books/_search/template",
        "{\"source\":{\"query\":{\"match\":{\"title\":\"{{q}}\"}}},\"params\":{\"q\":\"turtle\"}}",
    )));
    assert!(found.contains("\"_id\":\"3\"") && !found.contains("\"_id\":\"1\""), "{found}");
    let rendered = body(&api.answer(&request(
        "POST",
        "/_render/template",
        "{\"source\":\"{\\\"size\\\":{{size}}{{#sort}},\\\"sort\\\":{{#toJson}}sort{{/toJson}}{{/sort}}}\",\"params\":{\"size\":2,\"sort\":[\"pages\"]}}",
    )));
    assert_eq!(rendered, "{\"template_output\":{\"size\":2,\"sort\":[\"pages\"]}}");
    let broken = api.answer(&request(
        "POST",
        "/_render/template",
        "{\"source\":\"{{#open}}\",\"params\":{}}",
    ));
    assert_eq!(broken.status, 400);
}

#[test]
fn msearch_template_renders_each_search_and_reports_a_bad_one_in_place() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let lines = concat!(
        "{\"index\":\"books\"}\n",
        "{\"source\":{\"query\":{\"match\":{\"title\":\"{{q}}\"}}},\"params\":{\"q\":\"turtle\"}}\n",
        "{\"index\":\"books\"}\n",
        "{\"source\":\"{{#open}}\",\"params\":{}}\n",
    );
    let text = body(&api.answer(&request("POST", "/_msearch/template", lines)));
    assert!(text.contains("\"_id\":\"3\""), "{text}");
    assert!(text.contains("general_script_exception"), "{text}");
}

#[test]
fn dynamic_keyword_subfields_and_mapped_types_shape_term_and_range() {
    let keyspace = MemoryKeyspace::default();
    let api = seeded(&keyspace);
    let fables = body(&api.answer(&request(
        "POST",
        "/books/_search",
        "{\"query\":{\"term\":{\"genre.keyword\":\"fable\"}}}",
    )));
    assert!(
        fables.contains("\"_id\":\"1\"") && fables.contains("\"_id\":\"3\"") && !fables.contains("\"_id\":\"2\""),
        "{fables}"
    );
    let created = api.answer(&request(
        "PUT",
        "/logs",
        "{\"mappings\":{\"properties\":{\"at\":{\"type\":\"date\"},\"client\":{\"type\":\"ip\"},\"msg\":{\"type\":\"text\"}}}}",
    ));
    assert_eq!(created.status, 200, "{}", body(&created));
    for (id, document) in [
        ("1", "{\"at\":\"2024-01-15T10:00:00Z\",\"client\":\"10.1.2.3\",\"msg\":\"Disk ERROR found\"}"),
        ("2", "{\"at\":\"2024-02-03\",\"client\":\"192.168.0.9\",\"msg\":\"all good\"}"),
        ("3", "{\"at\":1704067200000,\"client\":\"10.9.0.1\",\"msg\":\"error again\"}"),
    ] {
        let indexed = api.answer(&request("PUT", &format!("/logs/_doc/{id}?refresh=true"), document));
        assert!(indexed.status < 300, "{}", body(&indexed));
    }
    let search = |query: &str| body(&api.answer(&request("POST", "/logs/_search", query)));
    let january = search("{\"query\":{\"range\":{\"at\":{\"gte\":\"2024-01-01\",\"lt\":\"2024-02-01\"}}}}");
    assert!(
        january.contains("\"_id\":\"1\"") && january.contains("\"_id\":\"3\"") && !january.contains("\"_id\":\"2\""),
        "{january}"
    );
    let subnet = search("{\"query\":{\"term\":{\"client\":\"10.0.0.0/8\"}}}");
    assert!(
        subnet.contains("\"_id\":\"1\"") && subnet.contains("\"_id\":\"3\"") && !subnet.contains("\"_id\":\"2\""),
        "{subnet}"
    );
    let token = search("{\"query\":{\"term\":{\"msg\":\"error\"}}}");
    assert!(
        token.contains("\"_id\":\"1\"") && token.contains("\"_id\":\"3\"") && !token.contains("\"_id\":\"2\""),
        "{token}"
    );
}
