//! A small NATS client of the comparison's own: `CONNECT` with an optional user and password,
//! `PUB`, `SUB` and `MSG`, `PING` and `PONG`, and JetStream's stream creation and acknowledged
//! publishes, against EggDB's `--nats-listen` and nats-server alike.

use crate::measure::{Measured, timed};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

struct Connection {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Connection {
    fn open(address: &str, login: Option<(&str, &str)>) -> Result<Self, String> {
        let stream = TcpStream::connect(address).map_err(|error| format!("cannot reach {address}: {error}"))?;
        stream.set_nodelay(true).map_err(|error| error.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(30))).map_err(|error| error.to_string())?;
        let writer = stream.try_clone().map_err(|error| error.to_string())?;
        let mut connection = Self { reader: BufReader::with_capacity(256 * 1024, stream), writer };
        let info = connection.line()?;
        if !info.starts_with("INFO ") {
            return Err(format!("{address} did not greet as a NATS server"));
        }
        let credentials = login.map(|(user, password)| format!(",\"user\":\"{user}\",\"pass\":\"{password}\"")).unwrap_or_default();
        let connect = format!("CONNECT {{\"verbose\":false,\"pedantic\":false,\"headers\":true,\"name\":\"eggdb-broker-compare\"{credentials}}}\r\n");
        connection.send(connect.as_bytes())?;
        connection.ping()?;
        Ok(connection)
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.writer.write_all(bytes).map_err(|error| error.to_string())
    }

    fn line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        if self.reader.read_line(&mut line).map_err(|error| error.to_string())? == 0 {
            return Err("the server closed the connection".to_owned());
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }

    fn ping(&mut self) -> Result<(), String> {
        self.send(b"PING\r\n")?;
        loop {
            match self.next()? {
                Event::Pong => return Ok(()),
                Event::Error(line) => return Err(format!("the server refused: {line}")),
                _ => {}
            }
        }
    }

    fn next(&mut self) -> Result<Event, String> {
        loop {
            let line = self.line()?;
            let parts: Vec<&str> = line.split(' ').collect();
            match parts.as_slice() {
                ["PING"] => self.send(b"PONG\r\n")?,
                ["PONG"] => return Ok(Event::Pong),
                ["+OK"] | ["INFO", ..] => {}
                ["MSG", subject, _, rest @ ..] => {
                    let subject = (*subject).to_owned();
                    let length: usize = rest.last().and_then(|length| length.parse().ok()).ok_or("a malformed MSG line")?;
                    let payload = self.take(length)?;
                    return Ok(Event::Message(subject, payload));
                }
                ["HMSG", subject, _, rest @ ..] if rest.len() >= 2 => {
                    let subject = (*subject).to_owned();
                    let total: usize = rest[rest.len() - 1].parse().map_err(|_| "a malformed HMSG line")?;
                    let header_length: usize = rest[rest.len() - 2].parse().map_err(|_| "a malformed HMSG line")?;
                    let whole = self.take(total)?;
                    return Ok(Event::Message(subject, whole.get(header_length..).unwrap_or_default().to_vec()));
                }
                [first, ..] if first.starts_with("-ERR") => return Ok(Event::Error(line)),
                _ => return Err(format!("an unexpected line from the server: {line}")),
            }
        }
    }

    fn take(&mut self, length: usize) -> Result<Vec<u8>, String> {
        let mut bytes = vec![0u8; length + 2];
        self.reader.read_exact(&mut bytes).map_err(|error| error.to_string())?;
        bytes.truncate(length);
        Ok(bytes)
    }
}

enum Event {
    Pong,
    Message(String, Vec<u8>),
    Error(String),
}

fn publish_frame(out: &mut Vec<u8>, subject: &str, reply: Option<&str>, payload: &[u8]) {
    match reply {
        Some(reply) => out.extend_from_slice(format!("PUB {subject} {reply} {}\r\n", payload.len()).as_bytes()),
        None => out.extend_from_slice(format!("PUB {subject} {}\r\n", payload.len()).as_bytes()),
    }
    out.extend_from_slice(payload);
    out.extend_from_slice(b"\r\n");
}

/// Publishes `count` 100-byte core messages in windows of `window`, each window followed by a
/// `PING` that the server answers once it has taken them, while a subscriber on another
/// connection receives them; returns the publishing and the receiving side's measurements.
pub(crate) fn publish_and_receive(address: &str, login: Option<(&str, &str)>, subject: &str, count: u64, window: u64) -> Result<(Measured, Measured), String> {
    let mut subscriber = Connection::open(address, login)?;
    subscriber.send(format!("SUB {subject} 1\r\n").as_bytes())?;
    subscriber.ping()?;
    let mut publisher = Connection::open(address, login)?;
    let payload = vec![b'm'; 100];
    std::thread::scope(|scope| {
        let receiver = scope.spawn(move || {
            timed(|latencies| {
                let mut received = 0u64;
                let mut started = Instant::now();
                while received < count {
                    if let Event::Message(..) = subscriber.next()? {
                        received += 1;
                        if received % window == 0 {
                            latencies.push(started.elapsed().as_micros() as u64);
                            started = Instant::now();
                        }
                    }
                }
                Ok(received)
            })
        });
        let published = timed(|latencies| {
            let mut sent = 0u64;
            while sent < count {
                let started = Instant::now();
                let batch = window.min(count - sent);
                let mut out = Vec::with_capacity(batch as usize * 128);
                for _ in 0..batch {
                    publish_frame(&mut out, subject, None, &payload);
                }
                publisher.send(&out)?;
                publisher.ping()?;
                sent += batch;
                latencies.push(started.elapsed().as_micros() as u64);
            }
            Ok(count)
        });
        let received = receiver.join().map_err(|_| "the subscriber stopped unexpectedly".to_owned())?;
        Ok((published?, received?))
    })
}

/// Creates a JetStream stream named `stream` on `subject`, then publishes `count` 100-byte
/// messages to it, `window` awaiting their acknowledgement at most, timing each window.
pub(crate) fn jetstream_publish(address: &str, login: Option<(&str, &str)>, stream: &str, subject: &str, count: u64, window: u64) -> Result<Measured, String> {
    let mut connection = Connection::open(address, login)?;
    connection.send(b"SUB _INBOX.compare.> 1\r\n")?;
    let mut create = Vec::new();
    let config = format!("{{\"name\":\"{stream}\",\"subjects\":[\"{subject}\"],\"storage\":\"file\",\"retention\":\"limits\"}}");
    publish_frame(&mut create, &format!("$JS.API.STREAM.CREATE.{stream}"), Some("_INBOX.compare.create"), config.as_bytes());
    connection.send(&create)?;
    loop {
        match connection.next()? {
            Event::Message(_, answer) if String::from_utf8_lossy(&answer).contains("\"error\"") => {
                return Err(format!("the stream was not created: {}", String::from_utf8_lossy(&answer)));
            }
            Event::Message(..) => break,
            Event::Error(line) => return Err(format!("the server refused: {line}")),
            Event::Pong => {}
        }
    }
    let payload = vec![b'j'; 100];
    timed(|latencies| {
        let mut sent = 0u64;
        while sent < count {
            let started = Instant::now();
            let batch = window.min(count - sent);
            let mut out = Vec::with_capacity(batch as usize * 160);
            for offset in 0..batch {
                publish_frame(&mut out, subject, Some(&format!("_INBOX.compare.{}", sent + offset)), &payload);
            }
            connection.send(&out)?;
            let mut acknowledged = 0;
            while acknowledged < batch {
                match connection.next()? {
                    Event::Message(_, answer) if String::from_utf8_lossy(&answer).contains("\"error\"") => {
                        return Err(format!("a publish was refused: {}", String::from_utf8_lossy(&answer)));
                    }
                    Event::Message(..) => acknowledged += 1,
                    Event::Error(line) => return Err(format!("the server refused: {line}")),
                    Event::Pong => {}
                }
            }
            sent += batch;
            latencies.push(started.elapsed().as_micros() as u64);
        }
        Ok(count)
    })
}
