//! A small client of the CQL native protocol v4 of the comparison's own: `STARTUP` with the
//! password authenticator when the server asks for it, `QUERY`, `PREPARE`, `EXECUTE` and
//! `BATCH`, with many requests in flight on one connection through their stream ids.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const VERSION_REQUEST: u8 = 0x04;
const ERROR: u8 = 0x00;
const STARTUP: u8 = 0x01;
const READY: u8 = 0x02;
const AUTHENTICATE: u8 = 0x03;
const QUERY: u8 = 0x07;
const RESULT: u8 = 0x08;
const PREPARE: u8 = 0x09;
const EXECUTE: u8 = 0x0a;
const BATCH: u8 = 0x0d;
const AUTH_RESPONSE: u8 = 0x0f;
const AUTH_SUCCESS: u8 = 0x10;
const CONSISTENCY_ONE: u16 = 0x0001;
const VALUES_FLAG: u8 = 0x01;
const ROWS: i32 = 0x0002;
const PREPARED: i32 = 0x0004;
const GLOBAL_TABLES_SPEC: i32 = 0x0001;
const HAS_MORE_PAGES: i32 = 0x0002;
const NO_METADATA: i32 = 0x0004;
const MOST_FRAME: usize = 256 << 20;

/// One connection to a CQL server.
pub(crate) struct Client {
    stream: TcpStream,
    buffered: Vec<u8>,
}

/// A server's answer: its opcode and body.
pub(crate) struct Response {
    pub(crate) stream: i16,
    pub(crate) opcode: u8,
    pub(crate) body: Vec<u8>,
}

fn long_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as i32).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn short_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u16).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn values(out: &mut Vec<u8>, values: &[Vec<u8>]) {
    out.extend_from_slice(&(values.len() as u16).to_be_bytes());
    for value in values {
        out.extend_from_slice(&(value.len() as i32).to_be_bytes());
        out.extend_from_slice(value);
    }
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn take(&mut self, count: usize) -> Result<&'b [u8], String> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or("a response is cut short")?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn int(&mut self) -> Result<i32, String> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().map_err(|_| "a response is cut short")?))
    }

    fn short(&mut self) -> Result<u16, String> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().map_err(|_| "a response is cut short")?))
    }

    fn string(&mut self) -> Result<String, String> {
        let length = usize::from(self.short()?);
        Ok(String::from_utf8_lossy(self.take(length)?).into_owned())
    }

    fn bytes(&mut self) -> Result<Option<&'b [u8]>, String> {
        let length = self.int()?;
        if length < 0 {
            return Ok(None);
        }
        self.take(length as usize).map(Some)
    }

    fn short_bytes(&mut self) -> Result<&'b [u8], String> {
        let length = usize::from(self.short()?);
        self.take(length)
    }

    fn option(&mut self, depth: usize) -> Result<(), String> {
        if depth > 16 {
            return Err("a column type nests too deeply".to_owned());
        }
        match self.short()? {
            0x0000 => {
                self.string()?;
            }
            0x0020 | 0x0022 => self.option(depth + 1)?,
            0x0021 => {
                self.option(depth + 1)?;
                self.option(depth + 1)?;
            }
            0x0030 => {
                self.string()?;
                self.string()?;
                for _ in 0..self.short()? {
                    self.string()?;
                    self.option(depth + 1)?;
                }
            }
            0x0031 => {
                for _ in 0..self.short()? {
                    self.option(depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl Response {
    fn error_text(&self) -> String {
        let mut reader = Reader { bytes: &self.body, at: 0 };
        let code = reader.int().unwrap_or(0);
        let message = reader.string().unwrap_or_default();
        format!("error 0x{code:04x}: {message}")
    }

    /// The prepared statement id of a `PREPARE`'s result.
    pub(crate) fn prepared_id(&self) -> Result<Vec<u8>, String> {
        self.check()?;
        let mut reader = Reader { bytes: &self.body, at: 0 };
        if reader.int()? != PREPARED {
            return Err("PREPARE did not answer a prepared statement".to_owned());
        }
        Ok(reader.short_bytes()?.to_vec())
    }

    /// How many rows a result holds; 0 for results without rows.
    pub(crate) fn row_count(&self) -> Result<u64, String> {
        self.check()?;
        let mut reader = Reader { bytes: &self.body, at: 0 };
        if reader.int()? != ROWS {
            return Ok(0);
        }
        let flags = reader.int()?;
        let columns = reader.int()?;
        if flags & HAS_MORE_PAGES != 0 {
            reader.bytes()?;
        }
        if flags & NO_METADATA == 0 {
            if flags & GLOBAL_TABLES_SPEC != 0 {
                reader.string()?;
                reader.string()?;
            }
            for _ in 0..columns.max(0) {
                if flags & GLOBAL_TABLES_SPEC == 0 {
                    reader.string()?;
                    reader.string()?;
                }
                reader.string()?;
                reader.option(0)?;
            }
        }
        Ok(reader.int()?.max(0) as u64)
    }

    /// Ok for anything but an `ERROR`.
    pub(crate) fn check(&self) -> Result<(), String> {
        if self.opcode == ERROR {
            return Err(self.error_text());
        }
        Ok(())
    }
}

impl Client {
    /// Connects and starts a session, signing in with `login` when the server asks.
    pub(crate) fn connect(address: &str, login: Option<(&str, &str)>) -> Result<Self, String> {
        let stream = TcpStream::connect(address).map_err(|error| format!("cannot reach {address}: {error}"))?;
        stream.set_nodelay(true).map_err(|error| error.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(60))).map_err(|error| error.to_string())?;
        let mut client = Self { stream, buffered: Vec::new() };
        let mut startup = 1u16.to_be_bytes().to_vec();
        short_string(&mut startup, "CQL_VERSION");
        short_string(&mut startup, "3.0.0");
        client.send(0, STARTUP, &startup)?;
        let answer = client.receive()?;
        match answer.opcode {
            READY => Ok(client),
            AUTHENTICATE => {
                let (user, password) = login.ok_or_else(|| format!("{address} asks for a password"))?;
                let mut token = vec![0u8];
                token.extend_from_slice(user.as_bytes());
                token.push(0);
                token.extend_from_slice(password.as_bytes());
                let mut body = (token.len() as i32).to_be_bytes().to_vec();
                body.extend_from_slice(&token);
                client.send(0, AUTH_RESPONSE, &body)?;
                let answer = client.receive()?;
                if answer.opcode != AUTH_SUCCESS {
                    return Err(format!("{address} refused the password: {}", answer.error_text()));
                }
                Ok(client)
            }
            _ => Err(format!("{address} did not start the session: {}", answer.error_text())),
        }
    }

    fn send(&mut self, stream: i16, opcode: u8, body: &[u8]) -> Result<(), String> {
        let mut frame = vec![VERSION_REQUEST, 0];
        frame.extend_from_slice(&stream.to_be_bytes());
        frame.push(opcode);
        frame.extend_from_slice(&(body.len() as i32).to_be_bytes());
        frame.extend_from_slice(body);
        self.stream.write_all(&frame).map_err(|error| error.to_string())
    }

    /// Reads the next response, whichever stream it answers.
    pub(crate) fn receive(&mut self) -> Result<Response, String> {
        loop {
            if self.buffered.len() >= 9 {
                let length = i32::from_be_bytes([self.buffered[5], self.buffered[6], self.buffered[7], self.buffered[8]]);
                let length = usize::try_from(length).ok().filter(|length| *length <= MOST_FRAME).ok_or("a response frame of a wrong size")?;
                if self.buffered.len() >= 9 + length {
                    let stream = i16::from_be_bytes([self.buffered[2], self.buffered[3]]);
                    let opcode = self.buffered[4];
                    let body = self.buffered[9..9 + length].to_vec();
                    self.buffered.drain(..9 + length);
                    return Ok(Response { stream, opcode, body });
                }
            }
            let mut chunk = [0u8; 64 * 1024];
            let read = self.stream.read(&mut chunk).map_err(|error| error.to_string())?;
            if read == 0 {
                return Err("the server closed the connection".to_owned());
            }
            self.buffered.extend_from_slice(&chunk[..read]);
        }
    }

    /// Runs `statement` and waits for its result.
    pub(crate) fn query(&mut self, statement: &str) -> Result<Response, String> {
        let mut body = Vec::new();
        long_string(&mut body, statement);
        body.extend_from_slice(&CONSISTENCY_ONE.to_be_bytes());
        body.push(0);
        self.send(0, QUERY, &body)?;
        let answer = self.receive()?;
        answer.check()?;
        Ok(answer)
    }

    /// Prepares `statement` and returns its id.
    pub(crate) fn prepare(&mut self, statement: &str) -> Result<Vec<u8>, String> {
        let mut body = Vec::new();
        long_string(&mut body, statement);
        self.send(0, PREPARE, &body)?;
        self.receive()?.prepared_id()
    }

    /// Sends an `EXECUTE` of the prepared statement `id` with `bound` on `stream`, without
    /// waiting for its result.
    pub(crate) fn execute(&mut self, stream: i16, id: &[u8], bound: &[Vec<u8>]) -> Result<(), String> {
        let mut body = (id.len() as u16).to_be_bytes().to_vec();
        body.extend_from_slice(id);
        body.extend_from_slice(&CONSISTENCY_ONE.to_be_bytes());
        body.push(VALUES_FLAG);
        values(&mut body, bound);
        self.send(stream, EXECUTE, &body)
    }

    /// Sends an unlogged `BATCH` of the prepared statement `id`, once per row of `rows`, on
    /// `stream`, without waiting for its result.
    pub(crate) fn batch(&mut self, stream: i16, id: &[u8], rows: &[Vec<Vec<u8>>]) -> Result<(), String> {
        let mut body = vec![1u8];
        body.extend_from_slice(&(rows.len() as u16).to_be_bytes());
        for row in rows {
            body.push(1);
            body.extend_from_slice(&(id.len() as u16).to_be_bytes());
            body.extend_from_slice(id);
            values(&mut body, row);
        }
        body.extend_from_slice(&CONSISTENCY_ONE.to_be_bytes());
        body.push(0);
        self.send(stream, BATCH, &body)
    }
}

/// Whether `opcode` is a result (`RESULT`), for callers that only count answers.
pub(crate) fn is_result(opcode: u8) -> bool {
    opcode == RESULT
}
