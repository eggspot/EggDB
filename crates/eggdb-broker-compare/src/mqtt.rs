//! A small MQTT 3.1.1 client of the comparison's own: `CONNECT` with a user name and password,
//! `PUBLISH` at QoS 1 with its `PUBACK`, and `SUBSCRIBE` at QoS 0, against EggDB's
//! `--mqtt-listen`, Mosquitto and EMQX alike.

use crate::measure::{Measured, timed};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

fn remaining_length(out: &mut Vec<u8>, mut length: usize) {
    loop {
        let mut byte = (length % 128) as u8;
        length /= 128;
        if length > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if length == 0 {
            return;
        }
    }
}

fn put_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn packet(first: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![first];
    remaining_length(&mut out, body.len());
    out.extend_from_slice(body);
    out
}

struct Connection {
    stream: TcpStream,
    buffered: Vec<u8>,
}

impl Connection {
    fn open(address: &str, client: &str, login: Option<(&str, &str)>) -> Result<Self, String> {
        let stream = TcpStream::connect(address).map_err(|error| format!("cannot reach {address}: {error}"))?;
        stream.set_nodelay(true).map_err(|error| error.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(30))).map_err(|error| error.to_string())?;
        let mut connection = Self { stream, buffered: Vec::new() };
        let mut body = Vec::new();
        put_string(&mut body, b"MQTT");
        body.push(4);
        body.push(if login.is_some() { 0xC2 } else { 0x02 });
        body.extend_from_slice(&60u16.to_be_bytes());
        put_string(&mut body, client.as_bytes());
        if let Some((user, password)) = login {
            put_string(&mut body, user.as_bytes());
            put_string(&mut body, password.as_bytes());
        }
        connection.stream.write_all(&packet(0x10, &body)).map_err(|error| error.to_string())?;
        let (kind, reply) = connection.read_packet()?;
        if kind >> 4 != 2 || reply.get(1) != Some(&0) {
            return Err(format!("the broker refused the connection ({reply:?})"));
        }
        Ok(connection)
    }

    fn read_packet(&mut self) -> Result<(u8, Vec<u8>), String> {
        loop {
            if self.buffered.len() >= 2 {
                let mut length = 0usize;
                let mut multiplier = 1usize;
                let mut at = 1;
                let mut complete = false;
                while at < self.buffered.len() && at <= 4 {
                    let byte = self.buffered[at];
                    length += usize::from(byte & 0x7F) * multiplier;
                    multiplier *= 128;
                    at += 1;
                    if byte & 0x80 == 0 {
                        complete = true;
                        break;
                    }
                }
                if complete && self.buffered.len() >= at + length {
                    let kind = self.buffered[0];
                    let body = self.buffered[at..at + length].to_vec();
                    self.buffered.drain(..at + length);
                    return Ok((kind, body));
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

    fn publish(&mut self, topic: &str, id: u16, payload: &[u8]) -> Result<(), String> {
        let mut body = Vec::new();
        put_string(&mut body, topic.as_bytes());
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(payload);
        self.stream.write_all(&packet(0x32, &body)).map_err(|error| error.to_string())
    }

    fn subscribe(&mut self, topic: &str) -> Result<(), String> {
        let mut body = 1u16.to_be_bytes().to_vec();
        put_string(&mut body, topic.as_bytes());
        body.push(0);
        self.stream.write_all(&packet(0x82, &body)).map_err(|error| error.to_string())?;
        loop {
            let (kind, _) = self.read_packet()?;
            if kind >> 4 == 9 {
                return Ok(());
            }
        }
    }
}

/// Publishes `count` 100-byte messages at QoS 1, `window` unacknowledged at most, timing each
/// window, while a subscriber on another connection receives them; returns the publishing and
/// the receiving side's measurements.
pub(crate) fn publish_and_receive(address: &str, login: Option<(&str, &str)>, topic: &str, count: u16, window: u16) -> Result<(Measured, Measured), String> {
    let mut subscriber = Connection::open(address, "compare-sub", login)?;
    subscriber.subscribe(topic)?;
    let mut publisher = Connection::open(address, "compare-pub", login)?;
    let payload = vec![b'm'; 100];
    let receiving = std::thread::scope(|scope| {
        let receiver = scope.spawn(move || {
            timed(|latencies| {
                let mut received = 0u64;
                let mut started = Instant::now();
                while received < u64::from(count) {
                    let (kind, _) = subscriber.read_packet()?;
                    if kind >> 4 == 3 {
                        received += 1;
                        if received % u64::from(window) == 0 {
                            latencies.push(started.elapsed().as_micros() as u64);
                            started = Instant::now();
                        }
                    }
                }
                Ok(received)
            })
        });
        let published = timed(|latencies| {
            let mut sent = 0u16;
            while sent < count {
                let started = Instant::now();
                let batch = window.min(count - sent);
                for offset in 0..batch {
                    publisher.publish(topic, sent + offset + 1, &payload)?;
                }
                let mut acknowledged = 0;
                while acknowledged < batch {
                    let (kind, _) = publisher.read_packet()?;
                    if kind >> 4 == 4 {
                        acknowledged += 1;
                    }
                }
                sent += batch;
                latencies.push(started.elapsed().as_micros() as u64);
            }
            Ok(u64::from(count))
        });
        let received = receiver.join().map_err(|_| "the subscriber stopped unexpectedly".to_owned())?;
        Ok::<_, String>((published?, received?))
    })?;
    Ok(receiving)
}
