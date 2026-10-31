use crate::codec::{FRAME_BODY, FRAME_HEADER, FRAME_METHOD, Out, frame, take_frame};
use crate::server::{Broker, ServerConfig, serve_stream, topic_matches};
use eggdb_keyspace::MemoryKeyspace;

fn config() -> ServerConfig {
    ServerConfig { user: "eggdb".to_owned(), password_sha256: eggdb_crypto::sha256(b"secret"), max_connections: 4 }
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
        out.empty_table().shortstr("PLAIN").longstr(&response).shortstr("en_US");
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
        out.short(0).shortstr(exchange).shortstr(routing_key).octet(0);
    });
    let mut header = Out::default();
    header.short(60).short(0).longlong(body.len() as u64).short(0);
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
            (u16::from_be_bytes([found.payload[0], found.payload[1]]), u16::from_be_bytes([found.payload[2], found.payload[3]]))
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
    let shapes: Vec<(u8, u16, u16)> = frames.iter().map(|(kind, class, id, _)| (*kind, *class, *id)).collect();
    assert_eq!(
        shapes,
        [(1, 10, 10), (1, 10, 30), (1, 10, 41), (1, 20, 11), (1, 50, 11), (1, 60, 71), (2, 0, 0), (3, 0, 0), (1, 60, 72)]
    );
    assert_eq!(frames[7].3, b"hello");
}

#[test]
fn consumers_receive_pushed_messages_through_exchanges_and_unacked_ones_return() {
    let keyspace = MemoryKeyspace::default();
    let mut input = login("secret");
    input.extend(declare(1, "audit"));
    input.extend(method(1, 40, 10, |out| {
        out.short(0).shortstr("logs").shortstr("topic").octet(0).empty_table();
    }));
    input.extend(method(1, 50, 20, |out| {
        out.short(0).shortstr("audit").shortstr("logs").shortstr("app.*.error").octet(0).empty_table();
    }));
    input.extend(publish(1, "logs", "app.db.error", b"disk full"));
    input.extend(publish(1, "logs", "app.db.info", b"ignored"));
    input.extend(method(1, 60, 20, |out| {
        out.short(0).shortstr("audit").shortstr("c1").octet(0).empty_table();
    }));
    let frames = run(&keyspace, &input);
    assert!(frames.iter().any(|(kind, class, id, _)| (*kind, *class, *id) == (1, 60, 60)));
    assert!(frames.iter().any(|(kind, _, _, payload)| *kind == 3 && payload == b"disk full"));
    assert!(!frames.iter().any(|(kind, _, _, payload)| *kind == 3 && payload == b"ignored"));
    let mut again = login("secret");
    again.extend(method(1, 60, 70, |out| {
        out.short(0).shortstr("audit").octet(0);
    }));
    let frames = run(&keyspace, &again);
    assert!(frames.iter().any(|(kind, _, _, payload)| *kind == 3 && payload == b"disk full"), "an unacknowledged message comes back");
}

#[test]
fn wrong_passwords_and_bad_frames_close_the_connection() {
    let keyspace = MemoryKeyspace::default();
    let frames = run(&keyspace, &login("wrong"));
    assert_eq!(frames.last().map(|(_, class, id, _)| (*class, *id)), Some((10, 50)));
    let mut input = login("secret");
    input.extend([1, 0, 1, 0xff, 0xff, 0xff, 0xff]);
    let frames = run(&keyspace, &input);
    assert_eq!(frames.last().map(|(_, class, id, _)| (*class, *id)), Some((10, 50)));
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
        let _ = serve_stream(&broker, &config(), damaged.as_slice(), &mut output, &|| false);
    }
}

#[test]
fn topic_patterns_match_as_the_specification_says() {
    assert!(topic_matches("a.*.c", "a.b.c"));
    assert!(!topic_matches("a.*.c", "a.c"));
    assert!(topic_matches("a.#", "a"));
    assert!(topic_matches("#.c", "a.b.c"));
    assert!(!topic_matches("a.b", "a.b.c"));
}
