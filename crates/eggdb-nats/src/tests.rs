use crate::protocol::{Frame, next_frame, subject_matches, valid_subject};
use crate::{ServerConfig, serve};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn config() -> ServerConfig {
    ServerConfig { user: "eggdb".to_owned(), password_sha256: eggdb_crypto::sha256(b"secret"), max_connections: 8 }
}

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn open(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut client = Self { reader: BufReader::new(stream.try_clone().unwrap()), writer: stream };
        assert!(client.line().starts_with("INFO {"));
        client
    }

    fn connected(address: SocketAddr, options: &str) -> Self {
        let mut client = Self::open(address);
        client.send(&format!("CONNECT {options}\r\nPING\r\n"));
        assert_eq!(client.line(), "PONG");
        client
    }

    fn send(&mut self, text: &str) {
        self.writer.write_all(text.as_bytes()).unwrap();
    }

    fn line(&mut self) -> String {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        line.trim_end().to_owned()
    }

    fn payload(&mut self, size: usize) -> String {
        let mut bytes = vec![0; size + 2];
        self.reader.read_exact(&mut bytes).unwrap();
        String::from_utf8(bytes[..size].to_vec()).unwrap()
    }

    fn sync(&mut self) {
        self.send("PING\r\n");
        assert_eq!(self.line(), "PONG");
    }
}

const LOGIN: &str = r#"{"user":"eggdb","pass":"secret","verbose":false}"#;

fn with_server(test: impl FnOnce(SocketAddr)) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let server = scope.spawn(|| serve(&listener, &config(), &|| stop.load(Ordering::Relaxed)));
        test(address);
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap().unwrap();
    });
}

#[test]
fn frames_parse_and_wait_for_whole_payloads() {
    assert_eq!(next_frame(b"PUB foo 5\r\nhel"), Ok(None));
    let (frame, used) = next_frame(b"pub foo reply 5\r\nhello\r\nPING\r\n").unwrap().unwrap();
    assert_eq!(frame, Frame::Publish { subject: "foo".to_owned(), reply: Some("reply".to_owned()), headers: Vec::new(), payload: b"hello".to_vec() });
    assert_eq!(used, 24);
    let (frame, _) = next_frame(b"HPUB foo 12 14\r\nNATS/1.0\r\n\r\nhi\r\n").unwrap().unwrap();
    assert_eq!(frame, Frame::Publish { subject: "foo".to_owned(), reply: None, headers: b"NATS/1.0\r\n\r\n".to_vec(), payload: b"hi".to_vec() });
    assert_eq!(next_frame(b"SUB foo workers 7\r\n").unwrap().unwrap().0, Frame::Subscribe { subject: "foo".to_owned(), queue: Some("workers".to_owned()), sid: "7".to_owned() });
    assert!(next_frame(b"PUB foo 5\r\nhelloXX").is_err());
    assert!(next_frame(b"PUB foo 99999999999\r\n").is_err());
    assert!(next_frame(b"HPUB foo 9 3\r\n").is_err());
    assert!(next_frame(b"EXPLODE\r\n").is_err());
    assert!(next_frame(&vec![b'a'; 5000]).is_err());
}

#[test]
fn subjects_match_wildcards() {
    assert!(subject_matches("orders.*.created", "orders.eu.created"));
    assert!(!subject_matches("orders.*.created", "orders.eu.west.created"));
    assert!(subject_matches("orders.>", "orders.eu.west.created"));
    assert!(!subject_matches("orders.>", "orders"));
    assert!(subject_matches("orders", "orders"));
    assert!(valid_subject("orders.>", true));
    assert!(!valid_subject("orders.>", false));
    assert!(!valid_subject("orders.>.x", true));
    assert!(!valid_subject("orders..x", true));
    assert!(!valid_subject("", true));
}

#[test]
fn messages_reach_subscribers_and_queue_groups_share_them() {
    with_server(|address| {
        let mut subscriber = Client::connected(address, LOGIN);
        subscriber.send("SUB orders.* 1\r\nSUB orders.> 2\r\n");
        subscriber.sync();
        let mut worker_a = Client::connected(address, LOGIN);
        let mut worker_b = Client::connected(address, LOGIN);
        worker_a.send("SUB jobs workers a\r\n");
        worker_a.sync();
        worker_b.send("SUB jobs workers b\r\n");
        worker_b.sync();
        let mut publisher = Client::connected(address, LOGIN);
        publisher.send("PUB orders.new inbox.1 5\r\nhello\r\nPUB jobs 1\r\nx\r\nPUB jobs 1\r\ny\r\n");
        publisher.sync();
        let mut deliveries = vec![subscriber.line(), subscriber.line()];
        deliveries.sort();
        assert_eq!(deliveries, ["MSG orders.new 1 inbox.1 5", "MSG orders.new 2 inbox.1 5"]);
        let first = worker_a.line();
        let second = worker_b.line();
        assert!(first.starts_with("MSG jobs a ") && second.starts_with("MSG jobs b "), "{first} / {second}");
    });
}

#[test]
fn headers_auto_unsubscribe_and_echo_follow_the_client_options() {
    with_server(|address| {
        let mut client = Client::connected(address, r#"{"user":"eggdb","pass":"secret","headers":true,"echo":false}"#);
        client.send("SUB events 1\r\nUNSUB 1 1\r\n");
        client.sync();
        let mut other = Client::connected(address, LOGIN);
        other.send("SUB events 9\r\n");
        other.sync();
        client.send("HPUB events 12 14\r\nNATS/1.0\r\n\r\nhi\r\n");
        client.sync();
        assert_eq!(other.line(), "MSG events 9 2");
        assert_eq!(other.payload(2), "hi");
        other.send("HPUB events 12 15\r\nNATS/1.0\r\n\r\none\r\nPUB events 3\r\ntwo\r\n");
        other.sync();
        assert_eq!(client.line(), "HMSG events 1 12 15");
        assert_eq!(client.payload(15), "NATS/1.0\r\n\r\none");
        client.sync();
        assert_eq!(other.line(), "MSG events 9 3");
        assert_eq!(other.payload(3), "one");
        assert_eq!(other.line(), "MSG events 9 3");
        assert_eq!(other.payload(3), "two");
    });
}

#[test]
fn wrong_passwords_and_bad_input_are_refused() {
    with_server(|address| {
        let mut intruder = Client::open(address);
        intruder.send("CONNECT {\"user\":\"eggdb\",\"pass\":\"wrong\"}\r\n");
        assert_eq!(intruder.line(), "-ERR 'Authorization Violation'");
        assert_eq!(intruder.line(), "");
        let mut early = Client::open(address);
        early.send("SUB foo 1\r\n");
        assert_eq!(early.line(), "-ERR 'Authorization Violation'");
        let mut verbose = Client::open(address);
        verbose.send("CONNECT {\"user\":\"eggdb\",\"pass\":\"secret\",\"verbose\":true}\r\n");
        assert_eq!(verbose.line(), "+OK");
        verbose.send("SUB bad..subject 1\r\nPUB with.* 0\r\n\r\n");
        assert_eq!(verbose.line(), "-ERR 'Invalid Subject'");
        assert_eq!(verbose.line(), "-ERR 'Invalid Subject'");
        verbose.send("NONSENSE\r\n");
        assert_eq!(verbose.line(), "-ERR 'Unknown Protocol Operation'");
    });
}
