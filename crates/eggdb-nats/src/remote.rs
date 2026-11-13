//! A small NATS client the server uses to read streams of other NATS servers (D1060): it signs
//! in with the user and password of its address, makes requests through an inbox of its own,
//! and answers the remote server's `PING`s.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const REQUEST_WAIT: Duration = Duration::from_secs(5);
const MOST_PAYLOAD: usize = 8 << 20;
const INBOX: &str = "_INBOX.eggdb-source";

/// A connection to another NATS server.
pub(crate) struct RemoteClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next: u64,
}

/// An answer: its headers block (empty when none) and payload.
pub(crate) type Reply = (Vec<u8>, Vec<u8>);

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

impl RemoteClient {
    /// Connects to `address`, `user:password@host:port` or `host:port`.
    pub(crate) fn connect(address: &str) -> Result<Self, String> {
        let address = address.strip_prefix("nats://").unwrap_or(address);
        let (login, host) = match address.rsplit_once('@') {
            Some((login, host)) => (login.split_once(':'), host),
            None => (None, address),
        };
        let stream = TcpStream::connect(host).map_err(|error| format!("cannot reach {host}: {error}"))?;
        stream.set_read_timeout(Some(REQUEST_WAIT)).map_err(|error| error.to_string())?;
        let writer = stream.try_clone().map_err(|error| error.to_string())?;
        let mut client = Self { reader: BufReader::new(stream), writer, next: 0 };
        let info = client.line()?;
        if !info.starts_with("INFO ") {
            return Err(format!("{host} did not greet as a NATS server"));
        }
        let credentials = login
            .map(|(user, password)| format!(",\"user\":{},\"pass\":{}", json_string(user), json_string(password)))
            .unwrap_or_default();
        let connect = format!("CONNECT {{\"verbose\":false,\"pedantic\":false,\"headers\":true,\"no_responders\":true,\"name\":\"eggdb-source\"{credentials}}}\r\nSUB {INBOX}.> 1\r\nPING\r\n");
        client.writer.write_all(connect.as_bytes()).map_err(|error| error.to_string())?;
        loop {
            let line = client.line()?;
            if line == "PONG" {
                return Ok(client);
            }
            if line.starts_with("-ERR") {
                return Err(format!("{host} refused the connection: {line}"));
            }
        }
    }

    fn line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("the remote NATS server closed the connection".to_owned());
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }

    fn take(&mut self, count: usize) -> Result<Vec<u8>, String> {
        if count > MOST_PAYLOAD {
            return Err("a remote message larger than 8 MiB".to_owned());
        }
        let mut bytes = vec![0u8; count + 2];
        self.reader.read_exact(&mut bytes).map_err(|error| error.to_string())?;
        bytes.truncate(count);
        Ok(bytes)
    }

    /// Sends `payload` to `subject` and waits for the answer on this client's inbox.
    pub(crate) fn request(&mut self, subject: &str, payload: &[u8]) -> Result<Reply, String> {
        self.next += 1;
        let inbox = format!("{INBOX}.{}", self.next);
        let mut out = format!("PUB {subject} {inbox} {}\r\n", payload.len()).into_bytes();
        out.extend_from_slice(payload);
        out.extend_from_slice(b"\r\n");
        self.writer.write_all(&out).map_err(|error| error.to_string())?;
        loop {
            let line = self.line()?;
            let parts: Vec<&str> = line.split(' ').collect();
            match parts.as_slice() {
                ["PING"] => self.writer.write_all(b"PONG\r\n").map_err(|error| error.to_string())?,
                ["MSG", to, _, rest @ ..] => {
                    let length: usize = rest.last().and_then(|length| length.parse().ok()).ok_or("a malformed MSG line")?;
                    let payload = self.take(length)?;
                    if *to == inbox {
                        return Ok((Vec::new(), payload));
                    }
                }
                ["HMSG", to, _, rest @ ..] if rest.len() >= 2 => {
                    let header_length: usize = rest[rest.len() - 2].parse().map_err(|_| "a malformed HMSG line")?;
                    let total: usize = rest[rest.len() - 1].parse().map_err(|_| "a malformed HMSG line")?;
                    let whole = self.take(total)?;
                    if header_length > whole.len() {
                        return Err("a malformed HMSG".to_owned());
                    }
                    if *to == inbox {
                        let (headers, payload) = whole.split_at(header_length);
                        return Ok((headers.to_vec(), payload.to_vec()));
                    }
                }
                [first, ..] if first.starts_with("-ERR") => return Err(format!("the remote NATS server said {line}")),
                _ => {}
            }
        }
    }
}
