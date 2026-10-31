use crate::frame::{Frame, next_frame};
use crate::{ServerConfig, serve};
use eggdb_keyspace::MemoryKeyspace;
use eggdb_queues::{Depth, Queues};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

fn now_ms() -> u64 {
    u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

fn config() -> ServerConfig {
    ServerConfig { user: "eggdb".to_owned(), password_sha256: eggdb_crypto::sha256(b"secret"), max_connections: 8 }
}

struct Client {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl Client {
    fn open(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        Self { stream, buffer: Vec::new() }
    }

    fn connected(address: SocketAddr) -> Self {
        let mut client = Self::open(address);
        client.send(&Frame::new("CONNECT").with("accept-version", "1.1,1.2").with("host", "eggdb").with("login", "eggdb").with("passcode", "secret"));
        let reply = client.next();
        assert_eq!(reply.command, "CONNECTED", "{reply:?}");
        assert_eq!(reply.header("version"), Some("1.2"));
        client
    }

    fn send(&mut self, frame: &Frame) {
        self.stream.write_all(&frame.encode()).unwrap();
    }

    fn next(&mut self) -> Frame {
        loop {
            if let Some((frame, used)) = next_frame(&self.buffer).unwrap() {
                self.buffer.drain(..used);
                return frame;
            }
            let mut chunk = [0; 4096];
            let read = self.stream.read(&mut chunk).unwrap();
            assert!(read > 0, "the server closed the connection");
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }

    fn receipt(&mut self, frame: Frame, id: &str) {
        self.send(&frame.with("receipt", id));
        let reply = self.next();
        assert_eq!((reply.command.as_str(), reply.header("receipt-id")), ("RECEIPT", Some(id)), "{reply:?}");
    }
}

fn with_server(test: impl FnOnce(SocketAddr, &Queues<'_>)) {
    let keyspace = MemoryKeyspace::default();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let server = scope.spawn(|| serve(&keyspace, &listener, &config(), &|| stop.load(Ordering::Relaxed)));
        test(address, &Queues::new(&keyspace));
        stop.store(true, Ordering::Relaxed);
        server.join().unwrap().unwrap();
    });
}

#[test]
fn frames_parse_with_escapes_lengths_and_heart_beats() {
    let (frame, used) = next_frame(b"\r\n\nSEND\r\ndestination:/queue/a\\cb\ncontent-length:3\n\na\0b\0\n").unwrap().unwrap();
    assert_eq!(frame.command, "SEND");
    assert_eq!(frame.header("destination"), Some("/queue/a:b"));
    assert_eq!(frame.body, b"a\0b");
    assert_eq!(used, 55);
    let (connect, _) = next_frame(b"CONNECT\nlogin:a\\cb\n\n\0").unwrap().unwrap();
    assert_eq!(connect.header("login"), Some("a\\cb"));
    assert_eq!(next_frame(b"SEND\ndestination:/queue/a\n\nhel"), Ok(None));
    assert!(next_frame(b"SEND\nbad\\x:1\n\n\0").is_err());
    assert!(next_frame(b"SEND\ncontent-length:2\n\nabc\0").is_err());
    assert!(next_frame(b"SEND\nnocolon\n\n\0").is_err());
    let round_trip = Frame::new("MESSAGE").with("subscription", "a:b\nc").with_body(b"x");
    assert_eq!(next_frame(&round_trip.encode()).unwrap().unwrap().0.header("subscription"), Some("a:b\nc"));
}

#[test]
fn queue_messages_are_acknowledged_one_by_one_or_cumulatively() {
    with_server(|address, queues| {
        let mut producer = Client::connected(address);
        for body in ["one", "two", "three"] {
            producer.receipt(Frame::new("SEND").with("destination", "/queue/jobs").with_body(body.as_bytes()), body);
        }
        let mut consumer = Client::connected(address);
        consumer.send(&Frame::new("SUBSCRIBE").with("destination", "/queue/jobs").with("id", "s1").with("ack", "client"));
        let messages: Vec<Frame> = (0..3).map(|_| consumer.next()).collect();
        assert_eq!(messages.iter().map(|message| message.body.as_slice()).collect::<Vec<_>>(), [&b"one"[..], b"two", b"three"]);
        assert_eq!(messages[0].header("subscription"), Some("s1"));
        let second = messages[1].header("ack").unwrap().to_owned();
        consumer.receipt(Frame::new("ACK").with("id", &second), "acked");
        assert_eq!(queues.depth("jobs", now_ms()).unwrap(), Depth { ready: 0, waiting: 1 });
        let third = messages[2].header("ack").unwrap().to_owned();
        consumer.receipt(Frame::new("NACK").with("id", &third), "nacked");
        let again = consumer.next();
        assert_eq!((again.body.as_slice(), again.header("redelivered")), (&b"three"[..], Some("true")));
        consumer.receipt(Frame::new("ACK").with("id", again.header("ack").unwrap()), "done");
        assert_eq!(queues.depth("jobs", u64::MAX).unwrap(), Depth::default());
    });
}

#[test]
fn transactions_commit_or_abort_together() {
    with_server(|address, queues| {
        let mut client = Client::connected(address);
        client.send(&Frame::new("BEGIN").with("transaction", "t1"));
        client.send(&Frame::new("SEND").with("destination", "/queue/orders").with("transaction", "t1").with_body(b"a"));
        client.receipt(Frame::new("ABORT").with("transaction", "t1"), "aborted");
        assert!(!queues.exists("orders").unwrap());
        client.send(&Frame::new("BEGIN").with("transaction", "t2"));
        client.send(&Frame::new("SEND").with("destination", "/queue/orders").with("transaction", "t2").with_body(b"a"));
        client.send(&Frame::new("SEND").with("destination", "/queue/orders").with("transaction", "t2").with_body(b"b"));
        client.receipt(Frame::new("COMMIT").with("transaction", "t2"), "committed");
        assert_eq!(queues.depth("orders", u64::MAX).unwrap().ready, 2);
    });
}

#[test]
fn topics_reach_every_subscriber() {
    with_server(|address, _| {
        let mut first = Client::connected(address);
        let mut second = Client::connected(address);
        first.receipt(Frame::new("SUBSCRIBE").with("destination", "/topic/news").with("id", "a"), "a");
        second.receipt(Frame::new("SUBSCRIBE").with("destination", "/topic/news").with("id", "b"), "b");
        let mut publisher = Client::connected(address);
        publisher.receipt(Frame::new("SEND").with("destination", "/topic/news").with("content-type", "text/plain").with_body(b"hi"), "sent");
        for (client, id) in [(&mut first, "a"), (&mut second, "b")] {
            let message = client.next();
            assert_eq!((message.command.as_str(), message.header("subscription"), message.body.as_slice()), ("MESSAGE", Some(id), &b"hi"[..]));
            assert_eq!(message.header("content-type"), Some("text/plain"));
        }
    });
}

#[test]
fn wrong_passcodes_and_bad_frames_are_refused() {
    with_server(|address, _| {
        let mut intruder = Client::open(address);
        intruder.send(&Frame::new("CONNECT").with("accept-version", "1.2").with("login", "eggdb").with("passcode", "wrong"));
        assert_eq!(intruder.next().command, "ERROR");
        let mut early = Client::open(address);
        early.send(&Frame::new("SEND").with("destination", "/queue/x"));
        assert_eq!(early.next().command, "ERROR");
        let mut client = Client::connected(address);
        client.send(&Frame::new("SEND").with("destination", "/elsewhere/x").with("receipt", "r"));
        let refusal = client.next();
        assert_eq!((refusal.command.as_str(), refusal.header("receipt-id")), ("ERROR", Some("r")));
    });
}
