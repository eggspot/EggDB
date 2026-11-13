//! A small Kafka client of the comparison's own: `Metadata` v1 to create the topic, `Produce`
//! v3 with record batches (magic 2, no compression) and `Fetch` v4, enough to move records the
//! way a producer and a consumer do, against EggDB's Kafka listener and Apache Kafka alike.

use crate::measure::{Measured, timed};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const CLIENT: &str = "eggdb-broker-compare";

struct Connection {
    stream: TcpStream,
    correlation: i32,
}

fn put_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as i16).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
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

fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 };
        }
    }
    !crc
}

/// A record batch of `values`, magic 2, no compression.
fn batch(values: &[Vec<u8>]) -> Vec<u8> {
    let mut records = Vec::new();
    for (delta, value) in values.iter().enumerate() {
        let mut record = vec![0u8];
        varint(&mut record, 0);
        varint(&mut record, delta as i64);
        varint(&mut record, -1);
        varint(&mut record, value.len() as i64);
        record.extend_from_slice(value);
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

impl Connection {
    fn open(address: &str) -> Result<Self, String> {
        let stream = TcpStream::connect(address).map_err(|error| format!("cannot reach {address}: {error}"))?;
        stream.set_nodelay(true).map_err(|error| error.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(30))).map_err(|error| error.to_string())?;
        Ok(Self { stream, correlation: 0 })
    }

    fn call(&mut self, key: i16, version: i16, body: &[u8]) -> Result<Vec<u8>, String> {
        self.correlation += 1;
        let mut frame = Vec::with_capacity(body.len() + 32);
        frame.extend_from_slice(&key.to_be_bytes());
        frame.extend_from_slice(&version.to_be_bytes());
        frame.extend_from_slice(&self.correlation.to_be_bytes());
        put_string(&mut frame, CLIENT);
        frame.extend_from_slice(body);
        let mut out = (frame.len() as i32).to_be_bytes().to_vec();
        out.extend(frame);
        self.stream.write_all(&out).map_err(|error| error.to_string())?;
        let mut length = [0u8; 4];
        self.stream.read_exact(&mut length).map_err(|error| error.to_string())?;
        let length = usize::try_from(i32::from_be_bytes(length)).map_err(|_| "a negative response length")?;
        if length > 64 << 20 {
            return Err("a response longer than 64 MiB".to_owned());
        }
        let mut response = vec![0u8; length];
        self.stream.read_exact(&mut response).map_err(|error| error.to_string())?;
        Ok(response.split_off(4.min(response.len())))
    }

    fn ensure_topic(&mut self, topic: &str) -> Result<(), String> {
        let mut body = 1i32.to_be_bytes().to_vec();
        put_string(&mut body, topic);
        for _ in 0..20 {
            self.call(3, 1, &body)?;
            if self.produce(topic, &[b"warm-up".to_vec()]).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(format!("the topic {topic} never became writable"))
    }

    fn produce(&mut self, topic: &str, values: &[Vec<u8>]) -> Result<(), String> {
        let records = batch(values);
        let mut body = Vec::new();
        body.extend_from_slice(&(-1i16).to_be_bytes());
        body.extend_from_slice(&1i16.to_be_bytes());
        body.extend_from_slice(&30_000i32.to_be_bytes());
        body.extend_from_slice(&1i32.to_be_bytes());
        put_string(&mut body, topic);
        body.extend_from_slice(&1i32.to_be_bytes());
        body.extend_from_slice(&0i32.to_be_bytes());
        body.extend_from_slice(&(records.len() as i32).to_be_bytes());
        body.extend(records);
        let response = self.call(0, 3, &body)?;
        let at = 4 + 2 + topic.len() + 4 + 4;
        let error = response.get(at..at + 2).map(|bytes| i16::from_be_bytes([bytes[0], bytes[1]])).ok_or("a short produce response")?;
        if error != 0 { Err(format!("produce failed with Kafka error {error}")) } else { Ok(()) }
    }

    /// Fetches from `offset`; returns how many records arrived and the offset after them.
    fn fetch(&mut self, topic: &str, offset: i64) -> Result<(u64, i64), String> {
        let mut body = Vec::new();
        body.extend_from_slice(&(-1i32).to_be_bytes());
        body.extend_from_slice(&500i32.to_be_bytes());
        body.extend_from_slice(&1i32.to_be_bytes());
        body.extend_from_slice(&(4i32 << 20).to_be_bytes());
        body.push(0);
        body.extend_from_slice(&1i32.to_be_bytes());
        put_string(&mut body, topic);
        body.extend_from_slice(&1i32.to_be_bytes());
        body.extend_from_slice(&0i32.to_be_bytes());
        body.extend_from_slice(&offset.to_be_bytes());
        body.extend_from_slice(&(4i32 << 20).to_be_bytes());
        let response = self.call(1, 4, &body)?;
        let short = || "a short fetch response".to_owned();
        let mut at = 4 + 4 + 2 + topic.len() + 4 + 4 + 2 + 8 + 8;
        let aborted = i32::from_be_bytes(response.get(at..at + 4).ok_or_else(short)?.try_into().map_err(|_| short())?);
        at += 4 + 16 * usize::try_from(aborted.max(0)).unwrap_or(0);
        let length = i32::from_be_bytes(response.get(at..at + 4).ok_or_else(short)?.try_into().map_err(|_| short())?);
        at += 4;
        let records = response.get(at..at + usize::try_from(length.max(0)).unwrap_or(0)).ok_or_else(short)?;
        let (mut count, mut next, mut cursor) = (0u64, offset, 0usize);
        while let Some(header) = records.get(cursor..cursor + 61) {
            let base = i64::from_be_bytes(header[..8].try_into().map_err(|_| short())?);
            let batch_length = usize::try_from(i32::from_be_bytes(header[8..12].try_into().map_err(|_| short())?)).unwrap_or(0);
            if records.len() < cursor + 12 + batch_length {
                break;
            }
            let last_delta = i32::from_be_bytes(header[23..27].try_into().map_err(|_| short())?);
            let in_batch = i32::from_be_bytes(header[57..61].try_into().map_err(|_| short())?);
            if base + i64::from(last_delta) >= offset {
                count += u64::try_from(in_batch.max(0)).unwrap_or(0);
            }
            next = next.max(base + i64::from(last_delta) + 1);
            cursor += 12 + batch_length;
        }
        Ok((count, next))
    }
}

fn values(count: usize, size: usize) -> Vec<Vec<u8>> {
    (0..count).map(|index| format!("{index:0size$}").into_bytes()).collect()
}

/// Produces `batches` batches of `batch_size` 100-byte records, timing each round trip.
pub(crate) fn produce(address: &str, topic: &str, batches: usize, batch_size: usize) -> Result<Measured, String> {
    let mut connection = Connection::open(address)?;
    connection.ensure_topic(topic)?;
    let payload = values(batch_size, 100);
    timed(|latencies| {
        for _ in 0..batches {
            let started = Instant::now();
            connection.produce(topic, &payload)?;
            latencies.push(started.elapsed().as_micros() as u64);
        }
        Ok((batches * batch_size) as u64)
    })
}

/// Fetches the topic from its start until `wanted` records arrived or it has no more, timing
/// each fetch, as a consumer catching up does.
pub(crate) fn consume(address: &str, topic: &str, wanted: u64) -> Result<Measured, String> {
    let mut connection = Connection::open(address)?;
    timed(|latencies| {
        let (mut moved, mut offset) = (0u64, 0i64);
        while moved < wanted {
            let started = Instant::now();
            let (count, next) = connection.fetch(topic, offset)?;
            latencies.push(started.elapsed().as_micros() as u64);
            if count == 0 || next <= offset {
                break;
            }
            moved += count;
            offset = next;
        }
        Ok(moved)
    })
}
