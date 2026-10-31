use crate::packstream::{Packed, Reader};
use crate::server::{ServerConfig, encode_request, serve_stream};
use eggdb_keyspace::MemoryKeyspace;

fn config() -> ServerConfig {
    ServerConfig { user: "eggdb".to_owned(), password_sha256: eggdb_crypto::sha256(b"secret"), max_connections: 4 }
}

fn text(value: &str) -> Packed {
    Packed::String(value.to_owned())
}

fn map(entries: Vec<(&str, Packed)>) -> Packed {
    Packed::Map(entries.into_iter().map(|(name, value)| (name.to_owned(), value)).collect())
}

fn hello(password: &str) -> Vec<u8> {
    encode_request(
        0x01,
        &[map(vec![("user_agent", text("test/1.0")), ("scheme", text("basic")), ("principal", text("eggdb")), ("credentials", text(password))])],
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
    input.extend(encode_request(0x10, &[text("CREATE (p:Person {name: $name}) RETURN p"), map(vec![("name", text("Ada"))]), map(vec![])]));
    input.extend(encode_request(0x3f, &[map(vec![("n", Packed::Integer(-1))])]));
    input.extend(encode_request(0x10, &[text("MATCH (p:Person) RETURN p.name AS name"), map(vec![]), map(vec![("mode", text("r"))])]));
    input.extend(encode_request(0x3f, &[map(vec![("n", Packed::Integer(1000))])]));
    input.extend(encode_request(0x02, &[]));
    let (version, messages) = session(input, &keyspace);
    assert_eq!(version, [0, 0, 0, 5]);
    let signatures: Vec<u8> = messages.iter().map(|message| match message {
        Packed::Struct(signature, _) => *signature,
        _ => 0,
    }).collect();
    assert_eq!(signatures, [0x70, 0x70, 0x71, 0x70, 0x70, 0x71, 0x70]);
    let Packed::Struct(_, record) = &messages[2] else { unreachable!() };
    let Packed::List(values) = &record[0] else { unreachable!() };
    assert!(matches!(&values[0], Packed::Struct(0x4e, fields) if fields.len() == 4));
    let Packed::Struct(_, record) = &messages[5] else { unreachable!() };
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
    assert!(matches!(&messages[0], Packed::Struct(0x7f, fields) if fields[0].get("code") == Some(&text("Neo.ClientError.Security.Unauthorized"))));
    let mut input = handshake([[0, 0, 0, 5], [0; 4], [0; 4], [0; 4]]);
    input.extend(hello("secret"));
    input.extend(encode_request(0x10, &[text("MATCH (n RETURN n"), map(vec![]), map(vec![])]));
    input.extend(encode_request(0x3f, &[map(vec![])]));
    input.extend(encode_request(0x0f, &[]));
    input.extend(encode_request(0x10, &[text("RETURN 1 AS one"), map(vec![]), map(vec![])]));
    let (_, messages) = session(input, &keyspace);
    let signatures: Vec<u8> = messages.iter().map(|message| match message {
        Packed::Struct(signature, _) => *signature,
        _ => 0,
    }).collect();
    assert_eq!(signatures, [0x70, 0x7f, 0x7e, 0x70, 0x70]);
}

#[test]
fn hostile_bytes_end_the_connection_without_panicking() {
    let keyspace = MemoryKeyspace::default();
    let mut valid = handshake([[0, 0, 0, 5], [0; 4], [0; 4], [0; 4]]);
    valid.extend(hello("secret"));
    valid.extend(encode_request(0x10, &[text("RETURN 1"), map(vec![]), map(vec![])]));
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
