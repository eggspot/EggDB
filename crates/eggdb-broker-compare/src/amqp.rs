//! A small AMQP 0-9-1 client of the comparison's own: the connection handshake with PLAIN,
//! one channel, a queue, publishes in confirm mode and a consumer that acknowledges, enough to
//! move messages as RabbitMQ clients do, against EggDB's `--amqp-listen` and RabbitMQ alike.

use crate::measure::{Measured, timed};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const METHOD: u8 = 1;
const HEADER: u8 = 2;
const BODY: u8 = 3;
const HEARTBEAT: u8 = 8;
const FRAME_END: u8 = 0xCE;

struct Frame {
    kind: u8,
    channel: u16,
    payload: Vec<u8>,
}

fn method_of(payload: &[u8]) -> (u16, u16) {
    (
        u16::from_be_bytes([payload.first().copied().unwrap_or(0), payload.get(1).copied().unwrap_or(0)]),
        u16::from_be_bytes([payload.get(2).copied().unwrap_or(0), payload.get(3).copied().unwrap_or(0)]),
    )
}

fn shortstr(out: &mut Vec<u8>, text: &str) {
    out.push(text.len().min(255) as u8);
    out.extend_from_slice(&text.as_bytes()[..text.len().min(255)]);
}

fn longstr(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn method(class: u16, id: u16, arguments: &[u8]) -> Vec<u8> {
    let mut out = class.to_be_bytes().to_vec();
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(arguments);
    out
}

struct Connection {
    stream: TcpStream,
    buffered: Vec<u8>,
}

impl Connection {
    fn open(address: &str, user: &str, password: &str) -> Result<Self, String> {
        let stream = TcpStream::connect(address).map_err(|error| format!("cannot reach {address}: {error}"))?;
        stream.set_nodelay(true).map_err(|error| error.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(30))).map_err(|error| error.to_string())?;
        let mut connection = Self { stream, buffered: Vec::new() };
        connection.stream.write_all(b"AMQP\x00\x00\x09\x01").map_err(|error| error.to_string())?;
        connection.expect(0, (10, 10))?;
        let mut start_ok = Vec::new();
        longstr(&mut start_ok, b"");
        shortstr(&mut start_ok, "PLAIN");
        longstr(&mut start_ok, format!("\0{user}\0{password}").as_bytes());
        shortstr(&mut start_ok, "en_US");
        connection.send(METHOD, 0, &method(10, 11, &start_ok))?;
        let tune = connection.expect(0, (10, 30))?;
        connection.send(METHOD, 0, &method(10, 31, tune.get(4..12).unwrap_or(&[0; 8])))?;
        let mut open = Vec::new();
        shortstr(&mut open, "/");
        shortstr(&mut open, "");
        open.push(0);
        connection.send(METHOD, 0, &method(10, 40, &open))?;
        connection.expect(0, (10, 41))?;
        connection.send(METHOD, 1, &method(20, 10, &[0]))?;
        connection.expect(1, (20, 11))?;
        Ok(connection)
    }

    fn send(&mut self, kind: u8, channel: u16, payload: &[u8]) -> Result<(), String> {
        let mut out = vec![kind];
        out.extend_from_slice(&channel.to_be_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out.push(FRAME_END);
        self.stream.write_all(&out).map_err(|error| error.to_string())
    }

    fn read_frame(&mut self) -> Result<Frame, String> {
        loop {
            if self.buffered.len() >= 7 {
                let length = u32::from_be_bytes(self.buffered[3..7].try_into().map_err(|_| "a bad frame")?) as usize;
                if length > 16 << 20 {
                    return Err("a frame larger than 16 MiB".to_owned());
                }
                if self.buffered.len() >= 8 + length {
                    let frame = Frame {
                        kind: self.buffered[0],
                        channel: u16::from_be_bytes([self.buffered[1], self.buffered[2]]),
                        payload: self.buffered[7..7 + length].to_vec(),
                    };
                    self.buffered.drain(..8 + length);
                    if frame.kind == HEARTBEAT {
                        continue;
                    }
                    return Ok(frame);
                }
            }
            let mut chunk = [0u8; 64 * 1024];
            let read = self.stream.read(&mut chunk).map_err(|error| error.to_string())?;
            if read == 0 {
                return Err("the broker closed the connection".to_owned());
            }
            self.buffered.extend_from_slice(&chunk[..read]);
        }
    }

    fn expect(&mut self, channel: u16, wanted: (u16, u16)) -> Result<Vec<u8>, String> {
        loop {
            let frame = self.read_frame()?;
            if frame.kind != METHOD || frame.channel != channel {
                continue;
            }
            let found = method_of(&frame.payload);
            if found == wanted {
                return Ok(frame.payload);
            }
            if matches!(found, (10, 50) | (20, 40)) {
                return Err(format!("the broker closed with {}", String::from_utf8_lossy(frame.payload.get(7..).unwrap_or_default())));
            }
        }
    }

    fn declare(&mut self, queue: &str) -> Result<(), String> {
        let mut arguments = 0u16.to_be_bytes().to_vec();
        shortstr(&mut arguments, queue);
        arguments.push(0b10);
        longstr(&mut arguments, b"");
        self.send(METHOD, 1, &method(50, 10, &arguments))?;
        self.expect(1, (50, 11)).map(drop)
    }

    fn publish(&mut self, queue: &str, body: &[u8]) -> Result<(), String> {
        let mut arguments = 0u16.to_be_bytes().to_vec();
        shortstr(&mut arguments, "");
        shortstr(&mut arguments, queue);
        arguments.push(0);
        self.send(METHOD, 1, &method(60, 40, &arguments))?;
        let mut header = 60u16.to_be_bytes().to_vec();
        header.extend_from_slice(&0u16.to_be_bytes());
        header.extend_from_slice(&(body.len() as u64).to_be_bytes());
        header.extend_from_slice(&0x1000u16.to_be_bytes());
        header.push(2);
        self.send(HEADER, 1, &header)?;
        self.send(BODY, 1, body)
    }
}

/// Publishes `batches` batches of `batch_size` persistent 100-byte messages in confirm mode,
/// waiting for the broker's confirmation of each batch, timing each batch.
pub(crate) fn publish(address: &str, user: &str, password: &str, queue: &str, batches: usize, batch_size: usize) -> Result<Measured, String> {
    let mut connection = Connection::open(address, user, password)?;
    connection.declare(queue)?;
    connection.send(METHOD, 1, &method(85, 10, &[0]))?;
    connection.expect(1, (85, 11))?;
    let body = vec![b'm'; 100];
    let mut confirmed = 0u64;
    timed(|latencies| {
        for _ in 0..batches {
            let started = Instant::now();
            for _ in 0..batch_size {
                connection.publish(queue, &body)?;
            }
            let target = confirmed + batch_size as u64;
            while confirmed < target {
                let acked = connection.expect(1, (60, 80))?;
                let tag = u64::from_be_bytes(acked.get(4..12).ok_or("a short confirmation")?.try_into().map_err(|_| "a short confirmation")?);
                let multiple = acked.get(12).is_some_and(|bits| bits & 1 == 1);
                confirmed = if multiple { tag } else { confirmed.max(tag) };
            }
            latencies.push(started.elapsed().as_micros() as u64);
        }
        Ok((batches * batch_size) as u64)
    })
}

/// Consumes `wanted` messages with a prefetch of 1,000, acknowledging every hundredth with
/// `multiple`, timing every hundred messages.
pub(crate) fn consume(address: &str, user: &str, password: &str, queue: &str, wanted: u64) -> Result<Measured, String> {
    let mut connection = Connection::open(address, user, password)?;
    let mut qos = 0u32.to_be_bytes().to_vec();
    qos.extend_from_slice(&1000u16.to_be_bytes());
    qos.push(0);
    connection.send(METHOD, 1, &method(60, 10, &qos))?;
    connection.expect(1, (60, 11))?;
    let mut consume = 0u16.to_be_bytes().to_vec();
    shortstr(&mut consume, queue);
    shortstr(&mut consume, "compare");
    consume.push(0);
    longstr(&mut consume, b"");
    connection.send(METHOD, 1, &method(60, 20, &consume))?;
    connection.expect(1, (60, 21))?;
    timed(|latencies| {
        let mut received = 0u64;
        let mut started = Instant::now();
        while received < wanted {
            let frame = connection.read_frame()?;
            if frame.kind != METHOD || method_of(&frame.payload) != (60, 60) {
                continue;
            }
            received += 1;
            let tag_at = 4 + 1 + usize::from(*frame.payload.get(4).unwrap_or(&0));
            let tag = frame.payload.get(tag_at..tag_at + 8).ok_or("a short delivery")?;
            if received % 100 == 0 || received == wanted {
                let mut ack = tag.to_vec();
                ack.push(1);
                connection.send(METHOD, 1, &method(60, 80, &ack))?;
                latencies.push(started.elapsed().as_micros() as u64);
                started = Instant::now();
            }
        }
        Ok(received)
    })
}
