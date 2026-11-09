use crate::frame::{Frame, next_frame, pending_frame_bytes};
use crate::server::{HeartBeatOffer, HeartBeats, negotiate, serve_offering};
use crate::{ServerConfig, serve};
use eggdb_keyspace::MemoryKeyspace;
use eggdb_platform::connection_limits::MemoryBudget;
use eggdb_queues::{Depth, Queues};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn config() -> ServerConfig {
    ServerConfig {
        user: "eggdb".to_owned(),
        password_sha256: eggdb_crypto::sha256(b"secret"),
        max_connections: 8,
        memory_budget: Arc::default(),
    }
}

struct Client {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl Client {
    fn open(address: SocketAddr) -> Self {
        let stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        Self {
            stream,
            buffer: Vec::new(),
        }
    }

    fn connected(address: SocketAddr) -> Self {
        let (client, reply) = Self::connecting(address, None);
        assert_eq!(reply.command, "CONNECTED", "{reply:?}");
        assert_eq!(reply.header("version"), Some("1.2"));
        client
    }

    fn connecting(address: SocketAddr, heart_beat: Option<&str>) -> (Self, Frame) {
        let mut client = Self::open(address);
        let mut connect = Frame::new("CONNECT")
            .with("accept-version", "1.1,1.2")
            .with("host", "eggdb")
            .with("login", "eggdb")
            .with("passcode", "secret");
        if let Some(heart_beat) = heart_beat {
            connect = connect.with("heart-beat", heart_beat);
        }
        client.send(&connect);
        let reply = client.next();
        (client, reply)
    }

    fn bytes_within(&mut self, wait: Duration) -> Vec<u8> {
        let mut bytes = std::mem::take(&mut self.buffer);
        self.stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let deadline = Instant::now() + wait;
        let mut chunk = [0; 4096];
        while Instant::now() < deadline {
            match self.stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("{error}"),
            }
        }
        self.stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        bytes
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
        assert_eq!(
            (reply.command.as_str(), reply.header("receipt-id")),
            ("RECEIPT", Some(id)),
            "{reply:?}"
        );
    }
}

struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn with_server(test: impl FnOnce(SocketAddr, &Queues<'_>)) {
    with_server_config(&config(), test);
}

fn with_server_offering(offer: HeartBeatOffer, test: impl FnOnce(SocketAddr)) {
    let keyspace = MemoryKeyspace::default();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stop = AtomicBool::new(false);
    let config = config();
    std::thread::scope(|scope| {
        let server = scope.spawn(|| {
            serve_offering(&keyspace, &listener, &config, offer, &|| {
                stop.load(Ordering::Relaxed)
            })
        });
        let stopping = StopOnDrop(&stop);
        test(address);
        drop(stopping);
        server.join().unwrap().unwrap();
    });
}

const QUICK_HEART_BEATS: HeartBeatOffer = HeartBeatOffer {
    send: Duration::from_millis(100),
    expect: Duration::from_millis(100),
};

fn with_server_config(config: &ServerConfig, test: impl FnOnce(SocketAddr, &Queues<'_>)) {
    let keyspace = MemoryKeyspace::default();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let server = scope.spawn(|| {
            serve(&keyspace, &listener, config, &|| {
                stop.load(Ordering::Relaxed)
            })
        });
        let stopping = StopOnDrop(&stop);
        test(address, &Queues::new(&keyspace));
        drop(stopping);
        server.join().unwrap().unwrap();
    });
}

#[test]
fn frames_parse_with_escapes_lengths_and_heart_beats() {
    let (frame, used) =
        next_frame(b"\r\n\nSEND\r\ndestination:/queue/a\\cb\ncontent-length:3\n\na\0b\0\n")
            .unwrap()
            .unwrap();
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
    let round_trip = Frame::new("MESSAGE")
        .with("subscription", "a:b\nc")
        .with_body(b"x");
    assert_eq!(
        next_frame(&round_trip.encode())
            .unwrap()
            .unwrap()
            .0
            .header("subscription"),
        Some("a:b\nc")
    );
}

#[test]
fn queue_messages_are_acknowledged_one_by_one_or_cumulatively() {
    with_server(|address, queues| {
        let mut producer = Client::connected(address);
        for body in ["one", "two", "three"] {
            producer.receipt(
                Frame::new("SEND")
                    .with("destination", "/queue/jobs")
                    .with_body(body.as_bytes()),
                body,
            );
        }
        let mut consumer = Client::connected(address);
        consumer.send(
            &Frame::new("SUBSCRIBE")
                .with("destination", "/queue/jobs")
                .with("id", "s1")
                .with("ack", "client"),
        );
        let messages: Vec<Frame> = (0..3).map(|_| consumer.next()).collect();
        assert_eq!(
            messages
                .iter()
                .map(|message| message.body.as_slice())
                .collect::<Vec<_>>(),
            [&b"one"[..], b"two", b"three"]
        );
        assert_eq!(messages[0].header("subscription"), Some("s1"));
        let second = messages[1].header("ack").unwrap().to_owned();
        consumer.receipt(Frame::new("ACK").with("id", &second), "acked");
        assert_eq!(
            queues.depth("jobs", now_ms()).unwrap(),
            Depth {
                ready: 0,
                waiting: 1
            }
        );
        let third = messages[2].header("ack").unwrap().to_owned();
        consumer.receipt(Frame::new("NACK").with("id", &third), "nacked");
        let again = consumer.next();
        assert_eq!(
            (again.body.as_slice(), again.header("redelivered")),
            (&b"three"[..], Some("true"))
        );
        consumer.receipt(
            Frame::new("ACK").with("id", again.header("ack").unwrap()),
            "done",
        );
        assert_eq!(queues.depth("jobs", u64::MAX).unwrap(), Depth::default());
    });
}

#[test]
fn transactions_commit_or_abort_together() {
    with_server(|address, queues| {
        let mut client = Client::connected(address);
        client.send(&Frame::new("BEGIN").with("transaction", "t1"));
        client.send(
            &Frame::new("SEND")
                .with("destination", "/queue/orders")
                .with("transaction", "t1")
                .with_body(b"a"),
        );
        client.receipt(Frame::new("ABORT").with("transaction", "t1"), "aborted");
        assert!(!queues.exists("orders").unwrap());
        client.send(&Frame::new("BEGIN").with("transaction", "t2"));
        client.send(
            &Frame::new("SEND")
                .with("destination", "/queue/orders")
                .with("transaction", "t2")
                .with_body(b"a"),
        );
        client.send(
            &Frame::new("SEND")
                .with("destination", "/queue/orders")
                .with("transaction", "t2")
                .with_body(b"b"),
        );
        client.receipt(Frame::new("COMMIT").with("transaction", "t2"), "committed");
        assert_eq!(queues.depth("orders", u64::MAX).unwrap().ready, 2);
    });
}

#[test]
fn topics_reach_every_subscriber() {
    with_server(|address, _| {
        let mut first = Client::connected(address);
        let mut second = Client::connected(address);
        first.receipt(
            Frame::new("SUBSCRIBE")
                .with("destination", "/topic/news")
                .with("id", "a"),
            "a",
        );
        second.receipt(
            Frame::new("SUBSCRIBE")
                .with("destination", "/topic/news")
                .with("id", "b"),
            "b",
        );
        let mut publisher = Client::connected(address);
        publisher.receipt(
            Frame::new("SEND")
                .with("destination", "/topic/news")
                .with("content-type", "text/plain")
                .with_body(b"hi"),
            "sent",
        );
        for (client, id) in [(&mut first, "a"), (&mut second, "b")] {
            let message = client.next();
            assert_eq!(
                (
                    message.command.as_str(),
                    message.header("subscription"),
                    message.body.as_slice()
                ),
                ("MESSAGE", Some(id), &b"hi"[..])
            );
            assert_eq!(message.header("content-type"), Some("text/plain"));
        }
    });
}

#[test]
fn wrong_passcodes_and_bad_frames_are_refused() {
    with_server(|address, _| {
        let mut intruder = Client::open(address);
        intruder.send(
            &Frame::new("CONNECT")
                .with("accept-version", "1.2")
                .with("login", "eggdb")
                .with("passcode", "wrong"),
        );
        assert_eq!(intruder.next().command, "ERROR");
        let mut early = Client::open(address);
        early.send(&Frame::new("SEND").with("destination", "/queue/x"));
        assert_eq!(early.next().command, "ERROR");
        let mut client = Client::connected(address);
        client.send(
            &Frame::new("SEND")
                .with("destination", "/elsewhere/x")
                .with("receipt", "r"),
        );
        let refusal = client.next();
        assert_eq!(
            (refusal.command.as_str(), refusal.header("receipt-id")),
            ("ERROR", Some("r"))
        );
    });
}

#[test]
fn a_frame_declares_the_bytes_it_will_take() {
    assert_eq!(pending_frame_bytes(b""), 0);
    let head = b"\nSEND\ndestination:/queue/a\ncontent-length:500000\n\n";
    assert_eq!(pending_frame_bytes(head), head.len() + 500_000 + 1);
    let crlf = b"SEND\r\ncontent-length:10\r\n\r\nabc";
    assert_eq!(pending_frame_bytes(crlf), 27 + 10 + 1);
    assert_eq!(pending_frame_bytes(b"SEND\ncontent-length:500000\n"), 27);
    assert_eq!(pending_frame_bytes(b"SEND\n\nbody"), 10);
}

#[test]
fn frames_and_transactions_past_the_memory_budget_are_refused_and_others_go_on() {
    let budget = Arc::new(MemoryBudget::new(400_000));
    let config = ServerConfig {
        memory_budget: Arc::clone(&budget),
        ..config()
    };
    with_server_config(&config, |address, queues| {
        let mut greedy = Client::connected(address);
        greedy
            .stream
            .write_all(b"SEND\ndestination:/queue/big\ncontent-length:500000\n\n")
            .unwrap();
        let refusal = greedy.next();
        assert_eq!(refusal.command, "ERROR", "{refusal:?}");
        assert!(
            String::from_utf8_lossy(&refusal.body).contains("memory budget"),
            "{refusal:?}"
        );
        let mut stager = Client::connected(address);
        stager.send(&Frame::new("BEGIN").with("transaction", "t"));
        let body = vec![b'x'; 120_000];
        let staged = || {
            Frame::new("SEND")
                .with("destination", "/queue/staged")
                .with("transaction", "t")
                .with_body(&body)
        };
        stager.receipt(staged(), "first");
        stager.receipt(staged(), "second");
        stager.send(&staged().with("receipt", "third"));
        let refusal = stager.next();
        assert_eq!(refusal.command, "ERROR", "{refusal:?}");
        assert!(
            String::from_utf8_lossy(&refusal.body).contains("memory budget"),
            "{refusal:?}"
        );
        let mut other = Client::connected(address);
        other.receipt(
            Frame::new("SEND")
                .with("destination", "/queue/small")
                .with_body(b"fits"),
            "sent",
        );
        assert_eq!(queues.depth("small", u64::MAX).unwrap().ready, 1);
        assert!(!queues.exists("staged").unwrap());
    });
    assert_eq!(budget.used_bytes(), 0);
}

#[test]
fn a_small_budget_delivers_every_queued_message_to_every_subscription_without_redelivering() {
    let budget = Arc::new(MemoryBudget::new(400_000));
    let config = ServerConfig {
        memory_budget: Arc::clone(&budget),
        ..config()
    };
    with_server_config(&config, |address, queues| {
        let body = vec![b'x'; 100_000];
        for queue in ["first", "second"] {
            queues.declare(queue).unwrap();
            for _ in 0..12 {
                queues.send(queue, &body, 0, now_ms()).unwrap();
            }
        }
        let mut consumer = Client::connected(address);
        for (queue, id) in [("first", "a"), ("second", "b")] {
            consumer.send(
                &Frame::new("SUBSCRIBE")
                    .with("destination", &format!("/queue/{queue}"))
                    .with("id", id),
            );
        }
        let mut received = std::collections::BTreeMap::<String, usize>::new();
        for _ in 0..24 {
            let message = consumer.next();
            assert_eq!(message.command, "MESSAGE", "{message:?}");
            assert_eq!(message.header("redelivered"), None, "{message:?}");
            assert_eq!(message.body.len(), body.len());
            *received
                .entry(message.header("subscription").unwrap().to_owned())
                .or_default() += 1;
        }
        assert_eq!(received.get("a"), Some(&12));
        assert_eq!(received.get("b"), Some(&12));
        queues.declare("huge").unwrap();
        queues
            .send("huge", &vec![b'y'; 500_000], 0, now_ms())
            .unwrap();
        let mut greedy = Client::connected(address);
        greedy.send(
            &Frame::new("SUBSCRIBE")
                .with("destination", "/queue/huge")
                .with("id", "h"),
        );
        let refusal = greedy.next();
        assert_eq!(refusal.command, "ERROR", "{refusal:?}");
        assert!(
            String::from_utf8_lossy(&refusal.body).contains("memory budget"),
            "{refusal:?}"
        );
        let untouched = queues.receive("huge", 1, 1, u64::MAX - 1).unwrap();
        assert_eq!(untouched[0].receive_count, 1);
    });
    assert_eq!(budget.used_bytes(), 0);
}

#[test]
fn heart_beats_are_negotiated_as_the_specification_says() {
    let standard = HeartBeatOffer::STANDARD;
    let seconds = |count| Some(Duration::from_secs(count));
    assert_eq!(negotiate(standard, None), Some(HeartBeats::default()));
    assert_eq!(negotiate(standard, Some("0,0")), Some(HeartBeats::default()));
    assert_eq!(
        negotiate(standard, Some("5000,20000")),
        Some(HeartBeats {
            send_every: seconds(20),
            silence_limit: seconds(20),
        })
    );
    assert_eq!(
        negotiate(standard, Some("30000, 1000")),
        Some(HeartBeats {
            send_every: seconds(10),
            silence_limit: seconds(60),
        })
    );
    assert_eq!(
        negotiate(standard, Some("0,1")),
        Some(HeartBeats {
            send_every: seconds(10),
            silence_limit: None,
        })
    );
    let silent = HeartBeatOffer {
        send: Duration::ZERO,
        expect: Duration::ZERO,
    };
    assert_eq!(
        negotiate(silent, Some("1000,1000")),
        Some(HeartBeats::default())
    );
    for malformed in ["", "10", "a,b", "-1,0", "1,2,3", "99999999999999999999,0"] {
        assert_eq!(negotiate(standard, Some(malformed)), None, "{malformed}");
    }
}

#[test]
fn connected_offers_ten_second_heart_beats_and_a_malformed_header_is_refused() {
    with_server(|address, _| {
        let (_, reply) = Client::connecting(address, Some("0,0"));
        assert_eq!(reply.command, "CONNECTED", "{reply:?}");
        assert_eq!(reply.header("heart-beat"), Some("10000,10000"));
        let (_, refusal) = Client::connecting(address, Some("often"));
        assert_eq!(refusal.command, "ERROR", "{refusal:?}");
        assert!(
            String::from_utf8_lossy(&refusal.body).contains("heart-beat"),
            "{refusal:?}"
        );
    });
}

#[test]
fn the_server_sends_heart_beats_only_to_a_client_that_wants_them() {
    with_server_offering(QUICK_HEART_BEATS, |address| {
        let (mut wanting, reply) = Client::connecting(address, Some("0,50"));
        assert_eq!(reply.header("heart-beat"), Some("100,100"));
        let beats = wanting.bytes_within(Duration::from_millis(700));
        assert!(
            beats.len() >= 3 && beats.iter().all(|&byte| byte == b'\n'),
            "{beats:?}"
        );
        let (mut quiet, _) = Client::connecting(address, Some("0,0"));
        assert_eq!(quiet.bytes_within(Duration::from_millis(400)), b"");
        quiet.receipt(Frame::new("DISCONNECT"), "bye");
    });
}

#[test]
fn a_silent_client_is_dropped_after_twice_the_agreed_interval() {
    with_server_offering(QUICK_HEART_BEATS, |address| {
        let (mut silent, _) = Client::connecting(address, Some("60,0"));
        let started = Instant::now();
        let mut chunk = [0; 64];
        let closed = loop {
            match silent.stream.read(&mut chunk) {
                Ok(0) | Err(_) => break started.elapsed(),
                Ok(_) => {}
            }
        };
        assert!(
            closed >= Duration::from_millis(200) && closed < Duration::from_secs(4),
            "{closed:?}"
        );
    });
}

#[test]
fn a_client_sending_heart_beats_stays_connected() {
    with_server_offering(QUICK_HEART_BEATS, |address| {
        let (mut talking, _) = Client::connecting(address, Some("60,0"));
        for _ in 0..12 {
            talking.stream.write_all(b"\n").unwrap();
            std::thread::sleep(Duration::from_millis(50));
        }
        talking.receipt(
            Frame::new("SEND")
                .with("destination", "/topic/pulse")
                .with_body(b"alive"),
            "still-here",
        );
        let (mut free, _) = Client::connecting(address, Some("0,0"));
        std::thread::sleep(Duration::from_millis(500));
        free.receipt(
            Frame::new("SEND")
                .with("destination", "/topic/pulse")
                .with_body(b"no limit"),
            "no-limit",
        );
    });
}
