//! A small MQTT 3.1.1 subscriber: `CONNECT` with an optional user and password, `SUBSCRIBE` at
//! QoS 1, `PUBLISH` received at QoS 0 and 1 (acknowledged), and `PINGREQ` while idle.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const KEEP_ALIVE_SECONDS: u16 = 30;
const READ_WAIT: Duration = Duration::from_millis(500);
const MOST_PACKET: usize = 16 << 20;

/// A broker address, `mqtt://[user:password@]host[:port]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broker {
    /// `host:port`, port 1883 when not given.
    pub address: String,
    /// The user name and password, when given.
    pub login: Option<(String, String)>,
}

impl Broker {
    /// Reads `mqtt://[user:password@]host[:port]`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let rest = text.strip_prefix("mqtt://").ok_or_else(|| format!("{text} is not an mqtt:// URL"))?;
        let rest = rest.trim_end_matches('/');
        let (login, host) = match rest.rsplit_once('@') {
            Some((login, host)) => {
                let (user, password) = login.split_once(':').ok_or_else(|| format!("{text}: write user:password@"))?;
                (Some((user.to_owned(), password.to_owned())), host)
            }
            None => (None, rest),
        };
        if host.is_empty() || host.contains('/') {
            return Err(format!("{text} must be mqtt://[user:password@]host[:port]"));
        }
        let address = if host.contains(':') { host.to_owned() } else { format!("{host}:1883") };
        Ok(Self { address, login })
    }
}

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
    out.extend_from_slice(&u16::try_from(bytes.len()).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(&bytes[..bytes.len().min(usize::from(u16::MAX))]);
}

fn packet(first: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![first];
    remaining_length(&mut out, body.len());
    out.extend_from_slice(body);
    out
}

/// A message a subscription delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    /// Its topic.
    pub topic: String,
    /// Its payload.
    pub payload: Vec<u8>,
}

/// A connected MQTT session.
#[derive(Debug)]
pub struct Subscriber {
    stream: TcpStream,
    buffered: Vec<u8>,
    last_sent: Instant,
}

impl Subscriber {
    /// Connects to `broker` as `client_id` and subscribes to `filter` at QoS 1.
    pub fn connect(broker: &Broker, client_id: &str, filter: &str) -> Result<Self, String> {
        let stream = TcpStream::connect(&broker.address).map_err(|error| format!("cannot reach {}: {error}", broker.address))?;
        stream.set_read_timeout(Some(READ_WAIT)).map_err(|error| error.to_string())?;
        let mut subscriber = Self { stream, buffered: Vec::new(), last_sent: Instant::now() };
        let mut body = Vec::new();
        put_string(&mut body, b"MQTT");
        body.push(4);
        body.push(match &broker.login {
            Some(_) => 0xC2,
            None => 0x02,
        });
        body.extend_from_slice(&KEEP_ALIVE_SECONDS.to_be_bytes());
        put_string(&mut body, client_id.as_bytes());
        if let Some((user, password)) = &broker.login {
            put_string(&mut body, user.as_bytes());
            put_string(&mut body, password.as_bytes());
        }
        subscriber.send(&packet(0x10, &body))?;
        let connected = subscriber.wait_for(2)?;
        if connected.get(1) != Some(&0) {
            return Err(format!("the broker refused the connection (code {:?})", connected.get(1)));
        }
        let mut body = 1u16.to_be_bytes().to_vec();
        put_string(&mut body, filter.as_bytes());
        body.push(1);
        subscriber.send(&packet(0x82, &body))?;
        let granted = subscriber.wait_for(9)?;
        if granted.last() == Some(&0x80) {
            return Err(format!("the broker refused the subscription to {filter}"));
        }
        Ok(subscriber)
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.last_sent = Instant::now();
        self.stream.write_all(bytes).map_err(|error| error.to_string())
    }

    fn wait_for(&mut self, kind: u8) -> Result<Vec<u8>, String> {
        let started = Instant::now();
        loop {
            if let Some((first, body)) = self.next_packet()? {
                if first >> 4 == kind {
                    return Ok(body);
                }
                continue;
            }
            if started.elapsed() > Duration::from_secs(10) {
                return Err("the broker did not answer".to_owned());
            }
        }
    }

    fn next_packet(&mut self) -> Result<Option<(u8, Vec<u8>)>, String> {
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
                if !complete && at > 4 {
                    return Err("a packet length runs past four bytes".to_owned());
                }
                if length > MOST_PACKET {
                    return Err("a packet larger than 16 MiB".to_owned());
                }
                if complete && self.buffered.len() >= at + length {
                    let first = self.buffered[0];
                    let body = self.buffered[at..at + length].to_vec();
                    self.buffered.drain(..at + length);
                    return Ok(Some((first, body)));
                }
            }
            let mut chunk = [0u8; 64 * 1024];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err("the broker closed the connection".to_owned()),
                Ok(read) => self.buffered.extend_from_slice(&chunk[..read]),
                Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return Ok(None),
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    /// The next delivered message, or `None` after waiting half a second; pings the broker
    /// while the connection is idle and acknowledges QoS 1 messages.
    pub fn receive(&mut self) -> Result<Option<Delivered>, String> {
        if self.last_sent.elapsed() > Duration::from_secs(u64::from(KEEP_ALIVE_SECONDS) / 2) {
            self.send(&[0xC0, 0x00])?;
        }
        let Some((first, body)) = self.next_packet()? else {
            return Ok(None);
        };
        if first >> 4 != 3 {
            return Ok(None);
        }
        let qos = (first >> 1) & 0x03;
        let topic_length = usize::from(u16::from_be_bytes([*body.first().ok_or("a PUBLISH without a topic")?, *body.get(1).ok_or("a PUBLISH without a topic")?]));
        let topic = String::from_utf8_lossy(body.get(2..2 + topic_length).ok_or("a PUBLISH topic runs past its packet")?).into_owned();
        let mut at = 2 + topic_length;
        if qos > 0 {
            let id = body.get(at..at + 2).ok_or("a PUBLISH without its packet id")?.to_vec();
            at += 2;
            if qos == 1 {
                self.send(&[0x40, 0x02, id[0], id[1]])?;
            } else {
                self.send(&[0x50, 0x02, id[0], id[1]])?;
            }
        }
        Ok(Some(Delivered { topic, payload: body.get(at..).unwrap_or_default().to_vec() }))
    }
}
