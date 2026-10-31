use super::value::{Value, decode_all, decode_first, encode};
use super::{AMQP_HEADER, SASL_HEADER};
use crate::server::{Broker, ServerConfig, serve_stream};
use eggdb_keyspace::MemoryKeyspace;
use std::collections::VecDeque;
use std::io::Read;

fn config() -> ServerConfig {
    ServerConfig { user: "eggdb".to_owned(), password_sha256: eggdb_crypto::sha256(b"secret"), max_connections: 4 }
}

struct Chunks(VecDeque<Vec<u8>>);

impl Read for Chunks {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let Some(mut chunk) = self.0.pop_front() else {
            return Ok(0);
        };
        let count = chunk.len().min(buffer.len());
        buffer[..count].copy_from_slice(&chunk[..count]);
        if count < chunk.len() {
            self.0.push_front(chunk.split_off(count));
        }
        Ok(count)
    }
}

fn frame(kind: u8, channel: u16, code: u64, fields: Vec<Value>, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    encode(&Value::described(code, fields), &mut body);
    body.extend_from_slice(payload);
    let mut out = u32::try_from(body.len() + 8).unwrap().to_be_bytes().to_vec();
    out.extend_from_slice(&[2, kind, 0, 0]);
    out[6..8].copy_from_slice(&channel.to_be_bytes());
    out.extend_from_slice(&body);
    out
}

fn sasl(password: &str) -> Vec<u8> {
    let mut out = SASL_HEADER.to_vec();
    let mut response = b"\0eggdb\0".to_vec();
    response.extend_from_slice(password.as_bytes());
    out.extend(frame(1, 0, 0x41, vec![Value::Symbol("PLAIN".to_owned()), Value::Binary(response)], &[]));
    out
}

fn terminus(code: u64, address: &str) -> Value {
    Value::described(code, vec![Value::String(address.to_owned())])
}

fn data(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode(&Value::Described(Box::new(Value::Ulong(0x77)), Box::new(Value::String(String::from_utf8(body.to_vec()).unwrap()))), &mut out);
    out
}

fn performatives(output: &[u8]) -> Vec<(u64, Value, Vec<u8>)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < output.len() {
        if output[at..].starts_with(b"AMQP") {
            at += 8;
            continue;
        }
        let size = u32::from_be_bytes(output[at..at + 4].try_into().unwrap()) as usize;
        let body = &output[at + 8..at + size];
        at += size;
        if body.is_empty() {
            continue;
        }
        let (value, used) = decode_first(body).unwrap();
        out.push((value.descriptor().unwrap(), value, body[used..].to_vec()));
    }
    out
}

fn run(keyspace: &MemoryKeyspace, chunks: Vec<Vec<u8>>) -> Vec<(u64, Value, Vec<u8>)> {
    let broker = Broker::new(keyspace);
    let mut output = Vec::new();
    serve_stream(&broker, &config(), Chunks(chunks.into()), &mut output, &|| false).unwrap();
    performatives(&output)
}

#[test]
fn values_round_trip_and_hostile_input_is_refused() {
    let value = Value::described(
        0x12,
        vec![
            Value::String("link".to_owned()),
            Value::Uint(7),
            Value::Bool(true),
            Value::Map(vec![(Value::Symbol("k".to_owned()), Value::Ulong(u64::MAX))]),
            Value::Array(vec![Value::Symbol("a".to_owned()), Value::Symbol("b".to_owned())]),
            Value::Binary(vec![0; 300]),
            Value::Signed(-5),
        ],
    );
    let mut bytes = Vec::new();
    encode(&value, &mut bytes);
    assert_eq!(decode_all(&bytes).unwrap(), [value]);
    assert_eq!(decode_all(&[0xc0, 0x03, 0x02, 0x52, 0x05, 0xa3]).ok(), None);
    assert_eq!(decode_all(&[0xc0, 0x04, 0x02, 0x52, 0x05, 0x43]).unwrap(), [Value::List(vec![Value::Uint(5), Value::Uint(0)])]);
    assert_eq!(decode_all(&[0xa3, 0x02, b'o', b'k', 0x53, 0x09]).unwrap(), [Value::Symbol("ok".to_owned()), Value::Ulong(9)]);
    assert!(decode_all(&[0xd0, 0, 0, 0, 4, 0xff, 0xff, 0xff, 0xff]).is_err());
    assert!(decode_all(&[0xc1, 0x02, 0x01, 0x40]).is_err());
    assert!(decode_all(&[0xff]).is_err());
    assert!(decode_all(&[0x00; 200]).is_err());
}

#[test]
fn messages_flow_in_and_out_of_a_queue_with_settlement() {
    let keyspace = MemoryKeyspace::default();
    let mut connect = AMQP_HEADER.to_vec();
    connect.extend(frame(0, 0, 0x10, vec![Value::String("client".to_owned()), Value::Null, Value::Uint(1 << 16)], &[]));
    connect.extend(frame(0, 0, 0x11, vec![Value::Null, Value::Uint(0), Value::Uint(100), Value::Uint(100)], &[]));
    connect.extend(frame(0, 0, 0x12, vec![Value::String("out".to_owned()), Value::Uint(0), Value::Bool(false), Value::Ubyte(2), Value::Ubyte(0), Value::Null, terminus(0x29, "orders"), Value::Null, Value::Null, Value::Uint(0)], &[]));
    connect.extend(frame(0, 0, 0x14, vec![Value::Uint(0), Value::Uint(0), Value::Binary(b"t0".to_vec()), Value::Uint(0), Value::Bool(false), Value::Bool(true)], &data(b"hel")[..4]));
    connect.extend(frame(0, 0, 0x14, vec![Value::Uint(0), Value::Null, Value::Null, Value::Null, Value::Null, Value::Bool(false)], &data(b"hel")[4..]));
    let receive = [
        frame(0, 0, 0x12, vec![Value::String("in".to_owned()), Value::Uint(1), Value::Bool(true), Value::Ubyte(0), Value::Ubyte(0), terminus(0x28, "orders"), Value::Null], &[]),
        frame(0, 0, 0x13, vec![Value::Uint(1), Value::Uint(100), Value::Uint(1), Value::Uint(100), Value::Uint(1), Value::Uint(0), Value::Uint(10)], &[]),
    ]
    .concat();
    let settle = [
        frame(0, 0, 0x15, vec![Value::Bool(true), Value::Uint(0), Value::Null, Value::Bool(true), Value::described(0x24, Vec::new())], &[]),
        frame(0, 0, 0x18, Vec::new(), &[]),
    ]
    .concat();
    let replies = run(&keyspace, vec![sasl("secret"), connect, receive, settle]);
    let codes: Vec<u64> = replies.iter().map(|(code, _, _)| *code).collect();
    assert_eq!(codes, [0x40, 0x44, 0x10, 0x11, 0x12, 0x13, 0x15, 0x12, 0x14, 0x18], "{replies:?}");
    assert_eq!(replies[1].1.field(0), &Value::Ubyte(0));
    assert_eq!(replies[6].1.field(4).descriptor(), Some(0x24));
    let (_, transfer, payload) = &replies[8];
    assert_eq!(transfer.field(1), &Value::Uint(0));
    let sections = decode_all(payload).unwrap();
    assert_eq!(sections[1].field(0), &Value::String("orders-1".to_owned()));
    assert_eq!(sections[2], Value::Described(Box::new(Value::Ulong(0x75)), Box::new(Value::Binary(b"hel".to_vec()))));
    let queues = eggdb_queues::Queues::new(&keyspace);
    assert_eq!(queues.depth("orders", u64::MAX).unwrap(), eggdb_queues::Depth::default());
}

#[test]
fn unsettled_deliveries_return_to_the_queue_when_the_client_goes() {
    let keyspace = MemoryKeyspace::default();
    eggdb_queues::Queues::new(&keyspace).declare("jobs").unwrap();
    eggdb_queues::Queues::new(&keyspace).send("jobs", b"work", 0, 0).unwrap();
    let mut connect = AMQP_HEADER.to_vec();
    connect.extend(frame(0, 0, 0x10, vec![Value::String("client".to_owned())], &[]));
    connect.extend(frame(0, 0, 0x11, vec![Value::Null, Value::Uint(0), Value::Uint(100), Value::Uint(100)], &[]));
    connect.extend(frame(0, 0, 0x12, vec![Value::String("in".to_owned()), Value::Uint(0), Value::Bool(true), Value::Ubyte(0), Value::Ubyte(0), terminus(0x28, "/queues/jobs"), Value::Null], &[]));
    connect.extend(frame(0, 0, 0x13, vec![Value::Uint(0), Value::Uint(100), Value::Uint(0), Value::Uint(100), Value::Uint(0), Value::Uint(0), Value::Uint(5)], &[]));
    let replies = run(&keyspace, vec![sasl("secret"), connect]);
    assert!(replies.iter().any(|(code, _, _)| *code == 0x14));
    let now = u64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()).unwrap();
    assert_eq!(eggdb_queues::Queues::new(&keyspace).depth("jobs", now).unwrap(), eggdb_queues::Depth { ready: 1, waiting: 0 });
}

#[test]
fn a_wrong_password_ends_the_sasl_exchange() {
    let keyspace = MemoryKeyspace::default();
    let replies = run(&keyspace, vec![sasl("wrong"), AMQP_HEADER.to_vec()]);
    assert_eq!(replies.iter().map(|(code, _, _)| *code).collect::<Vec<_>>(), [0x40, 0x44]);
    assert_eq!(replies[1].1.field(0), &Value::Ubyte(1));
}
