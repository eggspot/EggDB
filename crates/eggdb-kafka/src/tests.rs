use crate::server::{ServerConfig, answer};
use crate::wire::{Reader, Writer, crc32c};
use eggdb_keyspace::MemoryKeyspace;

fn config() -> ServerConfig {
    ServerConfig { advertised: "127.0.0.1:9092".parse().unwrap(), max_connections: 4 }
}

fn varint(out: &mut Vec<u8>, value: i64) {
    let mut zigzag = ((value << 1) ^ (value >> 63)) as u64;
    loop {
        let byte = (zigzag & 0x7f) as u8;
        zigzag >>= 7;
        if zigzag == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn batch(values: &[&str]) -> Vec<u8> {
    let mut records = Vec::new();
    for (delta, value) in values.iter().enumerate() {
        let mut record = vec![0u8];
        varint(&mut record, 0);
        varint(&mut record, delta as i64);
        varint(&mut record, -1);
        varint(&mut record, value.len() as i64);
        record.extend_from_slice(value.as_bytes());
        varint(&mut record, 0);
        varint(&mut records, record.len() as i64);
        records.extend(record);
    }
    let mut tail = Vec::new();
    tail.extend_from_slice(&0i16.to_be_bytes());
    tail.extend_from_slice(&(values.len() as i32 - 1).to_be_bytes());
    tail.extend_from_slice(&0i64.to_be_bytes());
    tail.extend_from_slice(&0i64.to_be_bytes());
    tail.extend_from_slice(&(-1i64).to_be_bytes());
    tail.extend_from_slice(&(-1i16).to_be_bytes());
    tail.extend_from_slice(&(-1i32).to_be_bytes());
    tail.extend_from_slice(&(values.len() as i32).to_be_bytes());
    tail.extend(records);
    let mut out = Vec::new();
    out.extend_from_slice(&0i64.to_be_bytes());
    out.extend_from_slice(&((4 + 1 + 4 + tail.len()) as i32).to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.push(2);
    out.extend_from_slice(&crc32c(&tail).to_be_bytes());
    out.extend(tail);
    out
}

fn request(key: i16, version: i16, body: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut writer = Writer::default();
    writer.i16(key).i16(version).i32(7).string(Some("test"));
    body(&mut writer);
    writer.out
}

fn produce(topic: &str, records: &[u8]) -> Vec<u8> {
    request(0, 3, |writer| {
        writer.string(None).i16(1).i32(1000).count(1).string(Some(topic)).count(1).i32(0).bytes(records);
    })
}

fn response(keyspace: &MemoryKeyspace, frame: &[u8]) -> Vec<u8> {
    let framed = answer(keyspace, &config(), frame).expect("an answer");
    assert_eq!(i32::from_be_bytes(framed[..4].try_into().unwrap()) as usize, framed.len() - 4);
    assert_eq!(i32::from_be_bytes(framed[4..8].try_into().unwrap()), 7);
    framed[8..].to_vec()
}

#[test]
fn api_versions_lists_what_is_supported_and_refuses_newer_versions() {
    let keyspace = MemoryKeyspace::default();
    let body = response(&keyspace, &request(18, 0, |_| {}));
    assert_eq!(i16::from_be_bytes([body[0], body[1]]), 0);
    assert_eq!(i32::from_be_bytes(body[2..6].try_into().unwrap()), 5);
    let newer = response(&keyspace, &request(18, 3, |_| {}));
    assert_eq!(i16::from_be_bytes([newer[0], newer[1]]), 35);
    assert!(answer(&keyspace, &config(), &request(1, 99, |_| {})).is_none());
}

#[test]
fn produced_records_are_fetched_from_their_offsets() {
    let keyspace = MemoryKeyspace::default();
    for (values, base) in [(&["a", "b"][..], 0i64), (&["c"][..], 2)] {
        let body = response(&keyspace, &produce("events", &batch(values)));
        let mut reader = Reader::new(&body);
        assert_eq!(reader.i32().unwrap(), 1);
        assert_eq!(reader.string().unwrap().as_deref(), Some("events"));
        assert_eq!(reader.i32().unwrap(), 1);
        assert_eq!((reader.i32().unwrap(), reader.i16().unwrap(), reader.i64().unwrap()), (0, 0, base));
    }
    let fetched = response(&keyspace, &request(1, 4, |writer| {
        writer.i32(-1).i32(100).i32(1).i32(1 << 20).i8(0).count(1).string(Some("events")).count(1).i32(0).i64(2).i32(1 << 20);
    }));
    let mut reader = Reader::new(&fetched);
    let _throttle = reader.i32().unwrap();
    reader.i32().unwrap();
    reader.string().unwrap();
    reader.i32().unwrap();
    assert_eq!((reader.i32().unwrap(), reader.i16().unwrap(), reader.i64().unwrap()), (0, 0, 3));
    reader.i64().unwrap();
    reader.i32().unwrap();
    let records = reader.bytes().unwrap().unwrap();
    assert_eq!(i64::from_be_bytes(records[..8].try_into().unwrap()), 2);
    assert!(records.windows(1).any(|window| window == b"c"));
    let offsets = response(&keyspace, &request(2, 1, |writer| {
        writer.i32(-1).count(1).string(Some("events")).count(2).i32(0).i64(-2).i32(0).i64(-1);
    }));
    let mut reader = Reader::new(&offsets);
    reader.i32().unwrap();
    reader.string().unwrap();
    reader.i32().unwrap();
    assert_eq!((reader.i32().unwrap(), reader.i16().unwrap(), reader.i64().unwrap(), reader.i64().unwrap()), (0, 0, -1, 0));
    assert_eq!((reader.i32().unwrap(), reader.i16().unwrap(), reader.i64().unwrap(), reader.i64().unwrap()), (0, 0, -1, 3));
}

#[test]
fn metadata_lists_and_creates_topics() {
    let keyspace = MemoryKeyspace::default();
    let body = response(&keyspace, &request(3, 1, |writer| {
        writer.count(2).string(Some("orders")).string(Some("bad topic!"));
    }));
    let mut reader = Reader::new(&body);
    assert_eq!(reader.i32().unwrap(), 1);
    assert_eq!(reader.i32().unwrap(), 0);
    assert_eq!(reader.string().unwrap().as_deref(), Some("127.0.0.1"));
    assert_eq!(reader.i32().unwrap(), 9092);
    reader.string().unwrap();
    reader.i32().unwrap();
    assert_eq!(reader.i32().unwrap(), 2);
    assert_eq!((reader.i16().unwrap(), reader.string().unwrap().as_deref().map(str::to_owned)), (0, Some("orders".to_owned())));
}

#[test]
fn corrupt_batches_and_hostile_frames_are_refused() {
    let keyspace = MemoryKeyspace::default();
    let mut damaged = batch(&["x"]);
    let last = damaged.len() - 1;
    damaged[last] ^= 1;
    let body = response(&keyspace, &produce("t", &damaged));
    let mut reader = Reader::new(&body);
    reader.i32().unwrap();
    reader.string().unwrap();
    reader.i32().unwrap();
    assert_eq!((reader.i32().unwrap(), reader.i16().unwrap()), (0, 2));
    let valid = produce("t", &batch(&["y"]));
    for cut in 0..valid.len() {
        let _ = answer(&keyspace, &config(), &valid[..cut]);
    }
    let mut huge = request(3, 1, |writer| {
        writer.i32(i32::MAX);
    });
    huge.extend_from_slice(&[0; 8]);
    assert!(answer(&keyspace, &config(), &huge).is_none());
}
