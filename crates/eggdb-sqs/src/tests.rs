use crate::Endpoint;
use eggdb_auth::sigv4::{SignedRequest, authorization, format_amz_date};
use eggdb_http::Request;
use eggdb_keyspace::MemoryKeyspace;

fn call(endpoint: &Endpoint<'_>, action: &str, body: &str, secret: &str) -> (u16, String) {
    let mut request = Request {
        method: "POST".to_owned(),
        path: "/".to_owned(),
        query: None,
        headers: vec![("host".to_owned(), "127.0.0.1:9324".to_owned()), ("x-amz-target".to_owned(), format!("AmazonSQS.{action}"))],
        body: body.as_bytes().to_vec(),
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    request.headers.push(("x-amz-date".to_owned(), format_amz_date(i64::try_from(now).unwrap())));
    let signed = SignedRequest { method: &request.method, path: &request.path, query: None, headers: &request.headers, body: &request.body };
    let value = authorization(&signed, &["host", "x-amz-date", "x-amz-target"], "eggdb", secret, "us-east-1", "sqs").unwrap();
    request.headers.push(("authorization".to_owned(), value));
    let response = endpoint.answer(&request);
    (response.status, String::from_utf8(response.body).unwrap())
}

fn field<'t>(text: &'t str, name: &str) -> &'t str {
    let marker = format!("\"{name}\":\"");
    let start = text.find(&marker).map(|at| at + marker.len()).expect(name);
    &text[start..start + text[start..].find('"').unwrap()]
}

#[test]
fn a_queue_sends_receives_and_deletes_messages() {
    let keyspace = MemoryKeyspace::default();
    let endpoint = Endpoint::new(&keyspace, "http://127.0.0.1:9324", "eggdb", "secret");
    let (status, created) = call(&endpoint, "CreateQueue", r#"{"QueueName":"jobs"}"#, "secret");
    assert_eq!(status, 200, "{created}");
    let url = field(&created, "QueueUrl").to_owned();
    assert_eq!(url, "http://127.0.0.1:9324/000000000000/jobs");
    let (_, sent) = call(&endpoint, "SendMessage", &format!(r#"{{"QueueUrl":"{url}","MessageBody":"hello"}}"#), "secret");
    assert_eq!(field(&sent, "MD5OfMessageBody"), "5d41402abc4b2a76b9719d911017c592");
    let (_, received) = call(&endpoint, "ReceiveMessage", &format!(r#"{{"QueueUrl":"{url}","MaxNumberOfMessages":10}}"#), "secret");
    assert_eq!(field(&received, "Body"), "hello");
    let handle = field(&received, "ReceiptHandle").to_owned();
    let (_, again) = call(&endpoint, "ReceiveMessage", &format!(r#"{{"QueueUrl":"{url}"}}"#), "secret");
    assert_eq!(again, r#"{"Messages":[]}"#);
    let (status, _) = call(&endpoint, "DeleteMessage", &format!(r#"{{"QueueUrl":"{url}","ReceiptHandle":"{handle}"}}"#), "secret");
    assert_eq!(status, 200);
    let (_, attributes) = call(&endpoint, "GetQueueAttributes", &format!(r#"{{"QueueUrl":"{url}"}}"#), "secret");
    assert_eq!(field(&attributes, "ApproximateNumberOfMessages"), "0");
    let (_, listed) = call(&endpoint, "ListQueues", "{}", "secret");
    assert!(listed.contains(&url));
}

#[test]
fn bad_signatures_unknown_queues_and_bad_input_are_refused() {
    let keyspace = MemoryKeyspace::default();
    let endpoint = Endpoint::new(&keyspace, "http://127.0.0.1:9324", "eggdb", "secret");
    assert_eq!(call(&endpoint, "ListQueues", "{}", "wrong").0, 403);
    let (status, body) = call(&endpoint, "SendMessage", r#"{"QueueUrl":"http://x/000000000000/nope","MessageBody":"x"}"#, "secret");
    assert_eq!(status, 400);
    assert!(body.contains("QueueDoesNotExist"));
    assert_eq!(call(&endpoint, "SendMessage", "{not json", "secret").0, 400);
    assert_eq!(call(&endpoint, "CreateQueue", r#"{"QueueName":"bad name!"}"#, "secret").0, 400);
    assert_eq!(call(&endpoint, "Explode", "{}", "secret").0, 400);
    let mut unsigned = Request { method: "POST".to_owned(), path: "/".to_owned(), query: None, headers: Vec::new(), body: Vec::new() };
    unsigned.headers.push(("authorization".to_owned(), "AWS4-HMAC-SHA256 nonsense".to_owned()));
    assert_eq!(endpoint.answer(&unsigned).status, 403);
}
