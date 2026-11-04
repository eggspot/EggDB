use crate::codec::{FRAME_BODY, FRAME_HEADER, FRAME_METHOD, Out, frame, take_frame};
use crate::server::{Broker, ServerConfig, encode_message, serve_stream, topic_matches};
use eggdb_keyspace::MemoryKeyspace;

fn config() -> ServerConfig {
    ServerConfig {
        user: "eggdb".to_owned(),
        password_sha256: eggdb_crypto::sha256(b"secret"),
        max_connections: 4,
        memory_budget: std::sync::Arc::default(),
    }
}

fn method(channel: u16, class: u16, id: u16, body: impl FnOnce(&mut Out)) -> Vec<u8> {
    let mut out = Out::method(class, id);
    body(&mut out);
    frame(FRAME_METHOD, channel, &out.bytes)
}

fn login(password: &str) -> Vec<u8> {
    let mut input = b"AMQP\x00\x00\x09\x01".to_vec();
    input.extend(method(0, 10, 11, |out| {
        let mut response = vec![0];
        response.extend_from_slice(b"eggdb");
        response.push(0);
        response.extend_from_slice(password.as_bytes());
        out.empty_table()
            .shortstr("PLAIN")
            .longstr(&response)
            .shortstr("en_US");
    }));
    input.extend(method(0, 10, 31, |out| {
        out.short(2047).long(131_072).short(0);
    }));
    input.extend(method(0, 10, 40, |out| {
        out.shortstr("/").shortstr("").octet(0);
    }));
    input.extend(method(1, 20, 10, |out| {
        out.shortstr("");
    }));
    input
}

fn publish(channel: u16, exchange: &str, routing_key: &str, body: &[u8]) -> Vec<u8> {
    let mut input = method(channel, 60, 40, |out| {
        out.short(0)
            .shortstr(exchange)
            .shortstr(routing_key)
            .octet(0);
    });
    let mut header = Out::default();
    header
        .short(60)
        .short(0)
        .longlong(body.len() as u64)
        .short(0);
    input.extend(frame(FRAME_HEADER, channel, &header.bytes));
    input.extend(frame(FRAME_BODY, channel, body));
    input
}

fn declare(channel: u16, queue: &str) -> Vec<u8> {
    method(channel, 50, 10, |out| {
        out.short(0).shortstr(queue).octet(0).empty_table();
    })
}

fn methods(output: &[u8]) -> Vec<(u8, u16, u16, Vec<u8>)> {
    let mut buffer = output.to_vec();
    let mut out = Vec::new();
    while let Some(found) = take_frame(&mut buffer).unwrap() {
        let (class, id) = if found.kind == FRAME_METHOD {
            (
                u16::from_be_bytes([found.payload[0], found.payload[1]]),
                u16::from_be_bytes([found.payload[2], found.payload[3]]),
            )
        } else {
            (0, 0)
        };
        out.push((found.kind, class, id, found.payload));
    }
    out
}

fn run(keyspace: &MemoryKeyspace, input: &[u8]) -> Vec<(u8, u16, u16, Vec<u8>)> {
    let broker = Broker::new(keyspace);
    let mut output = Vec::new();
    serve_stream(&broker, &config(), input, &mut output, &|| false).unwrap();
    methods(&output)
}

#[test]
fn a_client_logs_in_publishes_and_gets_a_message() {
    let keyspace = MemoryKeyspace::default();
    let mut input = login("secret");
    input.extend(declare(1, "tasks"));
    input.extend(publish(1, "", "tasks", b"hello"));
    input.extend(method(1, 60, 70, |out| {
        out.short(0).shortstr("tasks").octet(0);
    }));
    input.extend(method(1, 60, 80, |out| {
        out.longlong(1).octet(0);
    }));
    input.extend(method(1, 60, 70, |out| {
        out.short(0).shortstr("tasks").octet(0);
    }));
    let frames = run(&keyspace, &input);
    let shapes: Vec<(u8, u16, u16)> = frames
        .iter()
        .map(|(kind, class, id, _)| (*kind, *class, *id))
        .collect();
    assert_eq!(
        shapes,
        [
            (1, 10, 10),
            (1, 10, 30),
            (1, 10, 41),
            (1, 20, 11),
            (1, 50, 11),
            (1, 60, 71),
            (2, 0, 0),
            (3, 0, 0),
            (1, 60, 72)
        ]
    );
    assert_eq!(frames[7].3, b"hello");
    let start = &frames[0].3;
    let announced = start
        .windows(b"publisher_confirms".len())
        .position(|window| window == b"publisher_confirms");
    let flag_at = announced.map(|at| at + b"publisher_confirms".len());
    assert_eq!(
        flag_at.and_then(|at| start.get(at..at + 2)),
        Some(&[b't', 1][..])
    );
}

#[test]
fn consumers_receive_pushed_messages_through_exchanges_and_unacked_ones_return() {
    let keyspace = MemoryKeyspace::default();
    let mut input = login("secret");
    input.extend(declare(1, "audit"));
    input.extend(method(1, 40, 10, |out| {
        out.short(0)
            .shortstr("logs")
            .shortstr("topic")
            .octet(0)
            .empty_table();
    }));
    input.extend(method(1, 50, 20, |out| {
        out.short(0)
            .shortstr("audit")
            .shortstr("logs")
            .shortstr("app.*.error")
            .octet(0)
            .empty_table();
    }));
    input.extend(publish(1, "logs", "app.db.error", b"disk full"));
    input.extend(publish(1, "logs", "app.db.info", b"ignored"));
    input.extend(method(1, 60, 20, |out| {
        out.short(0)
            .shortstr("audit")
            .shortstr("c1")
            .octet(0)
            .empty_table();
    }));
    let frames = run(&keyspace, &input);
    assert!(
        frames
            .iter()
            .any(|(kind, class, id, _)| (*kind, *class, *id) == (1, 60, 60))
    );
    assert!(
        frames
            .iter()
            .any(|(kind, _, _, payload)| *kind == 3 && payload == b"disk full")
    );
    assert!(
        !frames
            .iter()
            .any(|(kind, _, _, payload)| *kind == 3 && payload == b"ignored")
    );
    let mut again = login("secret");
    again.extend(method(1, 60, 70, |out| {
        out.short(0).shortstr("audit").octet(0);
    }));
    let frames = run(&keyspace, &again);
    assert!(
        frames
            .iter()
            .any(|(kind, _, _, payload)| *kind == 3 && payload == b"disk full"),
        "an unacknowledged message comes back"
    );
}

#[test]
fn wrong_passwords_and_bad_frames_close_the_connection() {
    let keyspace = MemoryKeyspace::default();
    let frames = run(&keyspace, &login("wrong"));
    assert_eq!(
        frames.last().map(|(_, class, id, _)| (*class, *id)),
        Some((10, 50))
    );
    let mut input = login("secret");
    input.extend([1, 0, 1, 0xff, 0xff, 0xff, 0xff]);
    let frames = run(&keyspace, &input);
    assert_eq!(
        frames.last().map(|(_, class, id, _)| (*class, *id)),
        Some((10, 50))
    );
    let valid = {
        let mut input = login("secret");
        input.extend(declare(1, "q"));
        input.extend(publish(1, "", "q", b"x"));
        input
    };
    for at in 8..valid.len() {
        let mut damaged = valid.clone();
        damaged[at] ^= 0x5a;
        let broker = Broker::new(&keyspace);
        let mut output = Vec::new();
        let _ = serve_stream(&broker, &config(), damaged.as_slice(), &mut output, &|| {
            false
        });
    }
}

#[test]
fn topic_patterns_match_as_the_specification_says() {
    assert!(topic_matches("a.*.c", "a.b.c"));
    assert!(!topic_matches("a.*.c", "a.c"));
    assert!(topic_matches("a.#", "a"));
    assert!(topic_matches("#.c", "a.b.c"));
    assert!(!topic_matches("a.b", "a.b.c"));
    assert!(topic_matches("#", ""));
    assert!(topic_matches("a.#.#.c", "a.c"));
    let hostile = format!("{}z", "#.".repeat(60));
    let key = vec!["a"; 120].join(".");
    assert!(!topic_matches(&hostile, &key));
    assert!(topic_matches(&hostile, &format!("{key}.z")));
}

#[test]
fn a_message_body_past_the_memory_budget_closes_its_connection_and_others_go_on() {
    let keyspace = MemoryKeyspace::default();
    let broker = Broker::new(&keyspace);
    let budget = std::sync::Arc::new(eggdb_platform::connection_limits::MemoryBudget::new(
        300_000,
    ));
    let config = ServerConfig {
        memory_budget: std::sync::Arc::clone(&budget),
        ..config()
    };
    let mut greedy = login("secret");
    greedy.extend(declare(1, "jobs"));
    greedy.extend(method(1, 60, 40, |out| {
        out.short(0).shortstr("").shortstr("jobs").octet(0);
    }));
    let mut header = Out::default();
    header.short(60).short(0).longlong(400_000).short(0);
    greedy.extend(frame(FRAME_HEADER, 1, &header.bytes));
    let mut output = Vec::new();
    serve_stream(&broker, &config, greedy.as_slice(), &mut output, &|| false).unwrap();
    let answers = methods(&output);
    let close = answers
        .iter()
        .find(|(kind, class, id, _)| *kind == FRAME_METHOD && (*class, *id) == (10, 50))
        .expect("a Connection.Close");
    assert_eq!(u16::from_be_bytes([close.3[4], close.3[5]]), 506);
    assert_eq!(budget.used_bytes(), 0);
    let mut fitting = login("secret");
    fitting.extend(declare(1, "jobs"));
    fitting.extend(publish(1, "", "jobs", &vec![b'j'; 100_000]));
    let mut output = Vec::new();
    serve_stream(&broker, &config, fitting.as_slice(), &mut output, &|| false).unwrap();
    assert!(
        methods(&output)
            .iter()
            .all(|(_, class, id, _)| (*class, *id) != (10, 50))
    );
    assert_eq!(
        eggdb_queues::Queues::new(&keyspace)
            .depth("jobs", u64::MAX)
            .unwrap()
            .ready,
        1
    );
    assert_eq!(budget.used_bytes(), 0);
}

struct PolledInput<'a> {
    input: &'a [u8],
    idle_polls: usize,
}

impl std::io::Read for PolledInput<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if !self.input.is_empty() {
            return self.input.read(buffer);
        }
        if self.idle_polls == 0 {
            return Ok(0);
        }
        self.idle_polls -= 1;
        Err(std::io::ErrorKind::WouldBlock.into())
    }
}

#[test]
fn a_small_budget_delivers_every_queued_message_to_every_consumer_without_redelivering() {
    let keyspace = MemoryKeyspace::default();
    let broker = Broker::new(&keyspace);
    let budget = std::sync::Arc::new(eggdb_platform::connection_limits::MemoryBudget::new(
        400_000,
    ));
    let config = ServerConfig {
        memory_budget: std::sync::Arc::clone(&budget),
        ..config()
    };
    let queues = eggdb_queues::Queues::new(&keyspace);
    let body = vec![b'm'; 100_000];
    for queue in ["first", "second"] {
        queues.declare(queue).unwrap();
        let stored = encode_message("", queue, &[], &body);
        for _ in 0..12 {
            queues.send(queue, &stored, 0, 0).unwrap();
        }
    }
    let mut input = login("secret");
    for (queue, tag) in [("first", "c1"), ("second", "c2")] {
        input.extend(method(1, 60, 20, |out| {
            out.short(0)
                .shortstr(queue)
                .shortstr(tag)
                .octet(2)
                .empty_table();
        }));
    }
    let reader = PolledInput {
        input: &input,
        idle_polls: 40,
    };
    let mut output = Vec::new();
    serve_stream(&broker, &config, reader, &mut output, &|| false).unwrap();
    let answers = methods(&output);
    let deliveries: Vec<&Vec<u8>> = answers
        .iter()
        .filter(|(kind, class, id, _)| *kind == FRAME_METHOD && (*class, *id) == (60, 60))
        .map(|(_, _, _, payload)| payload)
        .collect();
    for tag in [&b"c1"[..], b"c2"] {
        assert_eq!(
            deliveries
                .iter()
                .filter(|payload| &payload[5..7] == tag)
                .count(),
            12
        );
    }
    assert!(deliveries.iter().all(|payload| payload[15] == 0));
    assert!(
        answers
            .iter()
            .all(|(_, class, id, _)| (*class, *id) != (10, 50))
    );
    assert_eq!(queues.depth("first", u64::MAX).unwrap().ready, 0);
    assert_eq!(budget.used_bytes(), 0);
    queues.declare("huge").unwrap();
    queues
        .send(
            "huge",
            &encode_message("", "huge", &[], &vec![b'h'; 500_000]),
            0,
            0,
        )
        .unwrap();
    let mut greedy = login("secret");
    greedy.extend(method(1, 60, 20, |out| {
        out.short(0)
            .shortstr("huge")
            .shortstr("h")
            .octet(2)
            .empty_table();
    }));
    let reader = PolledInput {
        input: &greedy,
        idle_polls: 3,
    };
    let mut output = Vec::new();
    serve_stream(&broker, &config, reader, &mut output, &|| false).unwrap();
    let answers = methods(&output);
    let close = answers
        .iter()
        .find(|(kind, class, id, _)| *kind == FRAME_METHOD && (*class, *id) == (10, 50))
        .expect("a Connection.Close");
    assert_eq!(u16::from_be_bytes([close.3[4], close.3[5]]), 506);
    assert_eq!(queues.receive("huge", 1, 1, 1).unwrap()[0].receive_count, 1);
    assert_eq!(budget.used_bytes(), 0);
}

#[test]
fn a_body_another_protocol_stored_reaches_amqp_consumers_as_a_plain_message() {
    let keyspace = MemoryKeyspace::default();
    let broker = Broker::new(&keyspace);
    let queues = eggdb_queues::Queues::new(&keyspace);
    queues.declare("jobs").unwrap();
    queues.send("jobs", b"raw from sqs", 0, 0).unwrap();
    queues
        .send("jobs", &encode_message("", "jobs", &[], b"framed"), 0, 0)
        .unwrap();
    queues.send("jobs", b"", 0, 0).unwrap();
    let mut input = login("secret");
    input.extend(method(1, 60, 20, |out| {
        out.short(0)
            .shortstr("jobs")
            .shortstr("c1")
            .octet(2)
            .empty_table();
    }));
    let reader = PolledInput {
        input: &input,
        idle_polls: 5,
    };
    let mut output = Vec::new();
    serve_stream(&broker, &config(), reader, &mut output, &|| false).unwrap();
    let answers = methods(&output);
    let deliveries: Vec<&Vec<u8>> = answers
        .iter()
        .filter(|(kind, class, id, _)| *kind == FRAME_METHOD && (*class, *id) == (60, 60))
        .map(|(_, _, _, payload)| payload)
        .collect();
    assert_eq!(deliveries.len(), 3);
    assert_eq!(&deliveries[0][16..22], b"\x00\x04jobs");
    let bodies: Vec<&Vec<u8>> = answers
        .iter()
        .filter(|(kind, _, _, _)| *kind == FRAME_BODY)
        .map(|(_, _, _, payload)| payload)
        .collect();
    assert_eq!(bodies, [&b"raw from sqs".to_vec(), &b"framed".to_vec()]);
    assert_eq!(
        queues.depth("jobs", u64::MAX).unwrap(),
        eggdb_queues::Depth::default()
    );
    queues.send("jobs", b"raw again", 0, 0).unwrap();
    let mut get = login("secret");
    get.extend(method(1, 60, 70, |out| {
        out.short(0).shortstr("jobs").octet(1);
    }));
    let frames = run(&keyspace, &get);
    assert!(
        frames
            .iter()
            .any(|(kind, _, _, payload)| *kind == FRAME_BODY && payload == b"raw again")
    );
    assert_eq!(queues.depth("jobs", u64::MAX).unwrap().ready, 0);
}
