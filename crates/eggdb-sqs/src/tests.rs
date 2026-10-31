use crate::Endpoint;
use crate::md5::md5_hex;
use crate::sigv4::sign;
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
    sign(&mut request, "eggdb", secret, "20261031T120000Z", "us-east-1");
    let response = endpoint.answer(&request);
    (response.status, String::from_utf8(response.body).unwrap())
}

fn field<'t>(text: &'t str, name: &str) -> &'t str {
    let marker = format!("\"{name}\":\"");
    let start = text.find(&marker).map(|at| at + marker.len()).expect(name);
    &text[start..start + text[start..].find('"').unwrap()]
}

#[test]
fn md5_matches_rfc_1321() {
    assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    assert_eq!(md5_hex(&[b'a'; 100]), "36a92cc94a9e0fa21f625f8bfb007adf");
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
    assert_eq!(field(&sent, "MD5OfMessageBody"), md5_hex(b"hello"));
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
