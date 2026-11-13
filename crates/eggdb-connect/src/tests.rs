use crate::json::{Json, parse};
use crate::rest::handle;
use crate::run_connectors_once;
use crate::store::Connectors;
use eggdb_http::Request;
use eggdb_keyspace::MemoryKeyspace;

fn call(connectors: &Connectors<'_>, method: &str, path: &str, body: &str) -> (u16, Json) {
    let (path, query) = match path.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (path.to_owned(), None),
    };
    let response = handle(connectors, &Request { method: method.to_owned(), path, query, headers: Vec::new(), body: body.as_bytes().to_vec() });
    let json = if response.body.is_empty() { Json::Null } else { parse(&response.body).unwrap() };
    (response.status, json)
}

fn scratch(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("eggdb-connect-{name}-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn connectors_are_created_listed_paused_and_deleted_as_connect_does() {
    let keyspace = MemoryKeyspace::default();
    let connectors = Connectors::new(&keyspace);
    let created = call(&connectors, "POST", "/connectors", r#"{"name": "lines", "config": {"connector.class": "FileStreamSource", "file": "/tmp/in.txt", "topic": "lines", "tasks.max": 1}}"#);
    assert_eq!(created.0, 201);
    assert_eq!(created.1.get("type").and_then(Json::text), Some("source"));
    assert_eq!(call(&connectors, "POST", "/connectors", r#"{"name": "lines", "config": {}}"#).0, 409);
    assert_eq!(call(&connectors, "GET", "/connectors", "").1, Json::Array(vec![Json::Text("lines".to_owned())]));
    let expanded = call(&connectors, "GET", "/connectors?expand=status", "").1;
    assert_eq!(
        expanded.get("lines").and_then(|entry| entry.get("status")).and_then(|status| status.get("connector")).and_then(|connector| connector.get("state")).and_then(Json::text),
        Some("RUNNING")
    );
    assert_eq!(call(&connectors, "PUT", "/connectors/lines/pause", "").0, 202);
    assert_eq!(call(&connectors, "GET", "/connectors/lines/status", "").1.get("connector").and_then(|connector| connector.get("state")).and_then(Json::text), Some("PAUSED"));
    assert_eq!(call(&connectors, "GET", "/connectors/lines/config", "").1.get("tasks.max").and_then(Json::text), Some("1"));
    let invalid = call(&connectors, "PUT", "/connectors/broken/config", r#"{"connector.class": "FileStreamSink"}"#);
    assert_eq!(invalid.0, 400);
    let validated = call(&connectors, "PUT", "/connector-plugins/FileStreamSink/config/validate", r#"{"connector.class": "FileStreamSink"}"#);
    assert_eq!(validated.1.get("error_count"), Some(&Json::Number(2.0)));
    assert_eq!(call(&connectors, "DELETE", "/connectors/lines", "").0, 204);
    assert_eq!(call(&connectors, "GET", "/connectors/lines", "").0, 404);
    assert_eq!(call(&connectors, "GET", "/connector-plugins", "").1, call(&connectors, "GET", "/connector-plugins", "").1);
}

#[test]
fn a_file_flows_through_a_topic_into_another_file_and_resumes_from_its_position() {
    let connect = MemoryKeyspace::default();
    let kafka = MemoryKeyspace::default();
    let connectors = Connectors::new(&connect);
    let input = scratch("input");
    let output = scratch("output");
    std::fs::write(&input, "one\ntwo\nthree").unwrap();
    let source = format!(r#"{{"name": "in", "config": {{"connector.class": "org.apache.kafka.connect.file.FileStreamSourceConnector", "file": "{}", "topic": "words"}}}}"#, input.display());
    let sink = format!(r#"{{"name": "out", "config": {{"connector.class": "FileStreamSink", "file": "{}", "topics": "words"}}}}"#, output.display());
    assert_eq!(call(&connectors, "POST", "/connectors", &source).0, 201);
    assert_eq!(call(&connectors, "POST", "/connectors", &sink).0, 201);
    run_connectors_once(&connect, &kafka).unwrap();
    run_connectors_once(&connect, &kafka).unwrap();
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "one\ntwo\n");
    std::fs::write(&input, "one\ntwo\nthree\nfour\n").unwrap();
    run_connectors_once(&connect, &kafka).unwrap();
    run_connectors_once(&connect, &kafka).unwrap();
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "one\ntwo\nthree\nfour\n");
    let missing = r#"{"name": "gone", "config": {"connector.class": "FileStreamSource", "file": "/no/such/file", "topic": "x"}}"#;
    call(&connectors, "POST", "/connectors", missing);
    run_connectors_once(&connect, &kafka).unwrap();
    assert_eq!(call(&connectors, "GET", "/connectors/gone/status", "").1.get("connector").and_then(|connector| connector.get("state")).and_then(Json::text), Some("FAILED"));
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
}
