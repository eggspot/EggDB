use crate::packstream::{Packed, Reader};
use crate::server::{ServerConfig, encode_request, serve_stream};
use eggdb_keyspace::MemoryKeyspace;
use eggdb_platform::connection_limits::MemoryBudget;
use std::sync::Arc;

fn config() -> ServerConfig {
    ServerConfig {
        user: "eggdb".to_owned(),
        password_sha256: eggdb_crypto::sha256(b"secret"),
        max_connections: 4,
        memory_budget: std::sync::Arc::default(),
    }
}

fn text(value: &str) -> Packed {
    Packed::String(value.to_owned())
}

fn map(entries: Vec<(&str, Packed)>) -> Packed {
    Packed::Map(
        entries
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

fn hello(password: &str) -> Vec<u8> {
    encode_request(
        0x01,
        &[map(vec![
            ("user_agent", text("test/1.0")),
            ("scheme", text("basic")),
            ("principal", text("eggdb")),
            ("credentials", text(password)),
        ])],
    )
}

fn handshake(versions: [[u8; 4]; 4]) -> Vec<u8> {
    let mut out = vec![0x60, 0x60, 0xb0, 0x17];
    for version in versions {
        out.extend_from_slice(&version);
    }
    out
}

fn responses(output: &[u8]) -> (Vec<u8>, Vec<Packed>) {
    let version = output[..4].to_vec();
    let mut messages = Vec::new();
    let mut at = 4;
    let mut payload = Vec::new();
    while at + 2 <= output.len() {
        let size = usize::from(u16::from_be_bytes([output[at], output[at + 1]]));
        at += 2;
        if size == 0 {
            let mut reader = Reader::new(&payload);
            messages.push(reader.value(0).unwrap());
            payload.clear();
            continue;
        }
        payload.extend_from_slice(&output[at..at + size]);
        at += size;
    }
    (version, messages)
}

fn session(input: Vec<u8>, keyspace: &MemoryKeyspace) -> (Vec<u8>, Vec<Packed>) {
    let mut output = Vec::new();
    serve_stream(keyspace, &config(), 1, input.as_slice(), &mut output).unwrap();
    responses(&output)
}

#[test]
fn a_driver_session_creates_and_reads_nodes() {
    let keyspace = MemoryKeyspace::default();
    let mut input = handshake([[0, 4, 4, 5], [0, 0, 4, 4], [0; 4], [0; 4]]);
    input.extend(hello("secret"));
    input.extend(encode_request(
        0x10,
        &[
            text("CREATE (p:Person {name: $name}) RETURN p"),
            map(vec![("name", text("Ada"))]),
            map(vec![]),
        ],
    ));
    input.extend(encode_request(
        0x3f,
        &[map(vec![("n", Packed::Integer(-1))])],
    ));
    input.extend(encode_request(
        0x10,
        &[
            text("MATCH (p:Person) RETURN p.name AS name"),
            map(vec![]),
            map(vec![("mode", text("r"))]),
        ],
    ));
    input.extend(encode_request(
        0x3f,
        &[map(vec![("n", Packed::Integer(1000))])],
    ));
    input.extend(encode_request(0x02, &[]));
    let (version, messages) = session(input, &keyspace);
    assert_eq!(version, [0, 0, 0, 5]);
    let signatures: Vec<u8> = messages
        .iter()
        .map(|message| match message {
            Packed::Struct(signature, _) => *signature,
            _ => 0,
        })
        .collect();
    assert_eq!(signatures, [0x70, 0x70, 0x71, 0x70, 0x70, 0x71, 0x70]);
    let Packed::Struct(_, record) = &messages[2] else {
        unreachable!()
    };
    let Packed::List(values) = &record[0] else {
        unreachable!()
    };
    assert!(matches!(&values[0], Packed::Struct(0x4e, fields) if fields.len() == 4));
    let Packed::Struct(_, record) = &messages[5] else {
        unreachable!()
    };
    assert_eq!(record[0], Packed::List(vec![text("Ada")]));
}

#[test]
fn bolt_4_4_is_chosen_when_5_is_not_offered() {
    let keyspace = MemoryKeyspace::default();
    let (version, _) = session(handshake([[0, 2, 4, 4], [0; 4], [0; 4], [0; 4]]), &keyspace);
    assert_eq!(version, [0, 0, 4, 4]);
    let (version, _) = session(handshake([[0, 0, 3, 3], [0; 4], [0; 4], [0; 4]]), &keyspace);
    assert_eq!(version, [0, 0, 0, 0]);
}

#[test]
fn wrong_passwords_and_failed_queries_are_reported() {
    let keyspace = MemoryKeyspace::default();
    let mut input = handshake([[0, 0, 0, 5], [0; 4], [0; 4], [0; 4]]);
    input.extend(hello("wrong"));
    let (_, messages) = session(input, &keyspace);
    assert!(
        matches!(&messages[0], Packed::Struct(0x7f, fields) if fields[0].get("code") == Some(&text("Neo.ClientError.Security.Unauthorized")))
    );
    let mut input = handshake([[0, 0, 0, 5], [0; 4], [0; 4], [0; 4]]);
    input.extend(hello("secret"));
    input.extend(encode_request(
        0x10,
        &[text("MATCH (n RETURN n"), map(vec![]), map(vec![])],
    ));
    input.extend(encode_request(0x3f, &[map(vec![])]));
    input.extend(encode_request(0x0f, &[]));
    input.extend(encode_request(
        0x10,
        &[text("RETURN 1 AS one"), map(vec![]), map(vec![])],
    ));
    let (_, messages) = session(input, &keyspace);
    let signatures: Vec<u8> = messages
        .iter()
        .map(|message| match message {
            Packed::Struct(signature, _) => *signature,
            _ => 0,
        })
        .collect();
    assert_eq!(signatures, [0x70, 0x7f, 0x7e, 0x70, 0x70]);
}

#[test]
fn hostile_bytes_end_the_connection_without_panicking() {
    let keyspace = MemoryKeyspace::default();
    let mut valid = handshake([[0, 0, 0, 5], [0; 4], [0; 4], [0; 4]]);
    valid.extend(hello("secret"));
    valid.extend(encode_request(
        0x10,
        &[text("RETURN 1"), map(vec![]), map(vec![])],
    ));
    for cut in 0..valid.len() {
        let mut output = Vec::new();
        let _ = serve_stream(&keyspace, &config(), 1, &valid[..cut], &mut output);
    }
    for at in 20..valid.len() {
        let mut damaged = valid.clone();
        damaged[at] ^= 0x5a;
        let mut output = Vec::new();
        let _ = serve_stream(&keyspace, &config(), 1, damaged.as_slice(), &mut output);
    }
    let mut deep = handshake([[0, 0, 0, 5], [0; 4], [0; 4], [0; 4]]);
    let mut nested = vec![0xb1, 0x01];
    nested.extend(std::iter::repeat_n(0x91, 500));
    nested.push(0xc0);
    deep.extend_from_slice(&(nested.len() as u16).to_be_bytes());
    deep.extend(nested);
    deep.extend([0, 0]);
    let mut output = Vec::new();
    let _ = serve_stream(&keyspace, &config(), 1, deep.as_slice(), &mut output);
}

fn budgeted(limit_bytes: usize) -> (Arc<MemoryBudget>, ServerConfig) {
    let budget = Arc::new(MemoryBudget::new(limit_bytes));
    let config = ServerConfig {
        memory_budget: Arc::clone(&budget),
        ..config()
    };
    (budget, config)
}

fn signatures(messages: &[Packed]) -> Vec<u8> {
    messages
        .iter()
        .map(|message| match message {
            Packed::Struct(signature, _) => *signature,
            _ => 0,
        })
        .collect()
}

fn is_out_of_memory(message: &Packed) -> bool {
    matches!(message, Packed::Struct(0x7f, fields) if fields[0].get("code") == Some(&text("Neo.TransientError.General.MemoryPoolOutOfMemoryError")))
}

#[test]
fn a_message_past_the_memory_budget_ends_its_connection_before_it_is_read() {
    let keyspace = MemoryKeyspace::default();
    let (budget, config) = budgeted(200_000);
    let mut input = handshake([[0, 0, 0, 5], [0; 4], [0; 4], [0; 4]]);
    input.extend(hello("secret"));
    input.extend(encode_request(
        0x10,
        &[
            text("RETURN $x AS x"),
            map(vec![("x", text(&"a".repeat(300_000)))]),
            map(vec![]),
        ],
    ));
    input.extend(encode_request(0x3f, &[map(vec![])]));
    let mut output = Vec::new();
    serve_stream(&keyspace, &config, 1, input.as_slice(), &mut output).unwrap();
    let (_, messages) = responses(&output);
    assert_eq!(signatures(&messages), [0x70, 0x7f]);
    assert!(is_out_of_memory(&messages[1]));
    assert_eq!(budget.used_bytes(), 0);
}

#[test]
fn a_result_whose_rows_pass_the_memory_budget_fails_its_run_and_the_connection_goes_on_after_reset() {
    let keyspace = MemoryKeyspace::default();
    let (budget, config) = budgeted(200_000);
    let mut input = handshake([[0, 0, 0, 5], [0; 4], [0; 4], [0; 4]]);
    input.extend(hello("secret"));
    input.extend(encode_request(
        0x10,
        &[
            text("UNWIND range(1, 40) AS i RETURN $x AS x"),
            map(vec![("x", text(&"b".repeat(10_000)))]),
            map(vec![]),
        ],
    ));
    input.extend(encode_request(
        0x3f,
        &[map(vec![("n", Packed::Integer(-1))])],
    ));
    input.extend(encode_request(0x0f, &[]));
    input.extend(encode_request(
        0x10,
        &[text("RETURN 1 AS one"), map(vec![]), map(vec![])],
    ));
    input.extend(encode_request(
        0x3f,
        &[map(vec![("n", Packed::Integer(-1))])],
    ));
    input.extend(encode_request(0x02, &[]));
    let mut output = Vec::new();
    serve_stream(&keyspace, &config, 1, input.as_slice(), &mut output).unwrap();
    let (_, messages) = responses(&output);
    assert_eq!(
        signatures(&messages),
        [0x70, 0x7f, 0x7e, 0x70, 0x70, 0x71, 0x70]
    );
    assert!(is_out_of_memory(&messages[1]));
    assert_eq!(budget.used_bytes(), 0);
}

#[test]
fn rows_waiting_for_pull_hold_the_budget_until_they_are_pulled_or_discarded() {
    let keyspace = MemoryKeyspace::default();
    let (budget, config) = budgeted(400_000);
    let mut input = handshake([[0, 0, 0, 5], [0; 4], [0; 4], [0; 4]]);
    input.extend(hello("secret"));
    input.extend(encode_request(
        0x10,
        &[
            text("UNWIND range(1, 20) AS i RETURN $x AS x"),
            map(vec![("x", text(&"c".repeat(10_000)))]),
            map(vec![]),
        ],
    ));
    input.extend(encode_request(0x3f, &[map(vec![("n", Packed::Integer(5))])]));
    input.extend(encode_request(0x2f, &[map(vec![])]));
    input.extend(encode_request(0x02, &[]));
    let mut output = Vec::new();
    serve_stream(&keyspace, &config, 1, input.as_slice(), &mut output).unwrap();
    let (_, messages) = responses(&output);
    assert_eq!(
        signatures(&messages),
        [0x70, 0x70, 0x71, 0x71, 0x71, 0x71, 0x71, 0x70, 0x70]
    );
    assert_eq!(budget.used_bytes(), 0);
}

#[test]
fn a_named_path_reaches_the_driver_as_a_path_structure() {
    let keyspace = MemoryKeyspace::default();
    let mut input = handshake([[0, 0, 0, 5], [0; 4], [0; 4], [0; 4]]);
    input.extend(hello("secret"));
    input.extend(encode_request(
        0x10,
        &[
            text(
                "CREATE (:Person {name: 'Ada'})-[:KNOWS]->(:Person {name: 'Grace'})<-[:KNOWS]-(:Person {name: 'Linus'})",
            ),
            map(vec![]),
            map(vec![]),
        ],
    ));
    input.extend(encode_request(
        0x3f,
        &[map(vec![("n", Packed::Integer(-1))])],
    ));
    input.extend(encode_request(
        0x10,
        &[
            text(
                "MATCH (ada {name: 'Ada'}), (linus {name: 'Linus'}) MATCH p = shortestPath((ada)-[*]-(linus)) RETURN p",
            ),
            map(vec![]),
            map(vec![("mode", text("r"))]),
        ],
    ));
    input.extend(encode_request(
        0x3f,
        &[map(vec![("n", Packed::Integer(-1))])],
    ));
    input.extend(encode_request(0x02, &[]));
    let (_, messages) = session(input, &keyspace);
    let record = messages
        .iter()
        .find_map(|message| match message {
            Packed::Struct(0x71, fields) => Some(fields),
            _ => None,
        })
        .expect("a RECORD message");
    let Packed::List(values) = &record[0] else {
        panic!("{record:?}");
    };
    let Packed::Struct(0x50, fields) = &values[0] else {
        panic!("{values:?}");
    };
    let [
        Packed::List(nodes),
        Packed::List(relationships),
        Packed::List(indices),
    ] = fields.as_slice()
    else {
        panic!("{fields:?}");
    };
    assert_eq!(nodes.len(), 3);
    assert!(
        nodes
            .iter()
            .all(|node| matches!(node, Packed::Struct(0x4e, fields) if fields.len() == 4))
    );
    assert_eq!(relationships.len(), 2);
    assert!(relationships.iter().all(
        |relationship| matches!(relationship, Packed::Struct(0x72, fields) if fields.len() == 4)
    ));
    assert_eq!(
        indices,
        &vec![
            Packed::Integer(1),
            Packed::Integer(1),
            Packed::Integer(-2),
            Packed::Integer(2)
        ]
    );
}
