use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_HANDSHAKE_BYTES: usize = 8 * 1024;
const HANDSHAKE_WAIT: Duration = Duration::from_secs(10);
const MAX_FRAME_BYTES: u64 = 32 << 20;
const OPCODE_CONTINUATION: u8 = 0x0;
const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xA;
const FINAL_FRAGMENT: u8 = 0x80;
const MASKED: u8 = 0x80;
const MQTT_SUBPROTOCOLS: [&str; 2] = ["mqtt", "mqttv3.1"];

/// Whether a connection's first byte starts an HTTP request rather than an MQTT `CONNECT`.
pub(crate) fn starts_http(stream: &TcpStream) -> bool {
    let mut first = [0u8; 1];
    let deadline = Instant::now() + HANDSHAKE_WAIT;
    while Instant::now() < deadline {
        match stream.peek(&mut first) {
            Ok(1) => return first[0] == b'G',
            Ok(_) => return false,
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(_) => return false,
        }
    }
    false
}

fn header<'h>(request: &'h str, name: &str) -> Option<&'h str> {
    request.lines().skip(1).find_map(|line| {
        let (field, value) = line.split_once(':')?;
        field.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// The `Sec-WebSocket-Accept` value for a client's `Sec-WebSocket-Key` (RFC 6455, 4.2.2).
pub(crate) fn accept_key(key: &str) -> String {
    eggdb_crypto::base64_encode(&eggdb_crypto::sha1(format!("{key}{ACCEPT_GUID}").as_bytes()))
}

/// Reads the client's opening handshake and answers it: `101 Switching Protocols` with the
/// `mqtt` subprotocol when the request is a WebSocket upgrade, `400` otherwise.
pub(crate) fn handshake(stream: &mut TcpStream) -> io::Result<()> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 1024];
    let started = Instant::now();
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        if request.len() > MAX_HANDSHAKE_BYTES || started.elapsed() > HANDSHAKE_WAIT {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "the WebSocket handshake is too long or too slow"));
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => request.extend_from_slice(&chunk[..read]),
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(error) => return Err(error),
        }
    }
    let text = String::from_utf8_lossy(&request);
    let upgrade = header(&text, "upgrade").is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let key = header(&text, "sec-websocket-key");
    let (true, Some(key)) = (upgrade && text.starts_with("GET "), key) else {
        stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a WebSocket upgrade"));
    };
    let protocol = header(&text, "sec-websocket-protocol").and_then(|offered| {
        offered
            .split(',')
            .map(str::trim)
            .find(|name| MQTT_SUBPROTOCOLS.iter().any(|known| known.eq_ignore_ascii_case(name)))
    });
    let mut response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n",
        accept_key(key)
    );
    if let Some(protocol) = protocol {
        response.push_str(&format!("Sec-WebSocket-Protocol: {protocol}\r\n"));
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes())
}

fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(FINAL_FRAGMENT | opcode);
    match payload.len() {
        length @ 0..=125 => out.push(length as u8),
        length @ 126..=65_535 => {
            out.push(126);
            out.extend_from_slice(&(length as u16).to_be_bytes());
        }
        length => {
            out.push(127);
            out.extend_from_slice(&(length as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

/// Writes each `write` as one binary WebSocket frame on the shared stream.
pub(crate) struct FrameWriter(pub(crate) Arc<Mutex<TcpStream>>);

impl Write for FrameWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write_all(&frame(OPCODE_BINARY, bytes))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).flush()
    }
}

/// Reads the payloads of a client's binary frames as one byte stream: unmasks them, answers
/// pings, and ends at a close frame.
pub(crate) struct FrameReader {
    pub(crate) stream: TcpStream,
    pub(crate) writer: Arc<Mutex<TcpStream>>,
    raw: Vec<u8>,
    ready: Vec<u8>,
    closed: bool,
}

impl FrameReader {
    pub(crate) fn new(stream: TcpStream, writer: Arc<Mutex<TcpStream>>) -> Self {
        Self {
            stream,
            writer,
            raw: Vec::new(),
            ready: Vec::new(),
            closed: false,
        }
    }

    fn take_frames(&mut self) -> io::Result<()> {
        loop {
            if self.raw.len() < 2 {
                return Ok(());
            }
            let (first, second) = (self.raw[0], self.raw[1]);
            let opcode = first & 0x0F;
            if second & MASKED == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "a client frame is not masked"));
            }
            let (length, at) = match second & 0x7F {
                126 => match self.raw.get(2..4) {
                    Some(bytes) => (u64::from(u16::from_be_bytes([bytes[0], bytes[1]])), 4),
                    None => return Ok(()),
                },
                127 => match self.raw.get(2..10) {
                    Some(bytes) => {
                        let mut array = [0u8; 8];
                        array.copy_from_slice(bytes);
                        (u64::from_be_bytes(array), 10)
                    }
                    None => return Ok(()),
                },
                short => (u64::from(short), 2),
            };
            if length > MAX_FRAME_BYTES {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "a WebSocket frame is too large"));
            }
            let length = length as usize;
            let Some(mask) = self.raw.get(at..at + 4).map(|mask| [mask[0], mask[1], mask[2], mask[3]]) else {
                return Ok(());
            };
            let start = at + 4;
            if self.raw.len() < start + length {
                return Ok(());
            }
            let payload: Vec<u8> = self.raw[start..start + length]
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4])
                .collect();
            self.raw.drain(..start + length);
            match opcode {
                OPCODE_BINARY | OPCODE_CONTINUATION => self.ready.extend_from_slice(&payload),
                OPCODE_PING => {
                    self.writer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .write_all(&frame(OPCODE_PONG, &payload))?;
                }
                OPCODE_PONG => {}
                OPCODE_CLOSE => {
                    let _ = self
                        .writer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .write_all(&frame(OPCODE_CLOSE, payload.get(..2).unwrap_or_default()));
                    self.closed = true;
                    return Ok(());
                }
                _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "MQTT over WebSocket takes binary frames only")),
            }
        }
    }
}

impl Read for FrameReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if !self.ready.is_empty() {
                let count = buffer.len().min(self.ready.len());
                buffer[..count].copy_from_slice(&self.ready[..count]);
                self.ready.drain(..count);
                return Ok(count);
            }
            if self.closed {
                return Ok(0);
            }
            let mut chunk = [0u8; 16 * 1024];
            let read = self.stream.read(&mut chunk)?;
            if read == 0 {
                return Ok(0);
            }
            self.raw.extend_from_slice(&chunk[..read]);
            self.take_frames()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_matches_rfc_6455() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kZGzZo2BhXjhb4=");
    }

    #[test]
    fn frames_carry_their_length() {
        assert_eq!(frame(OPCODE_BINARY, &[1, 2]), vec![0x82, 2, 1, 2]);
        assert_eq!(frame(OPCODE_BINARY, &[0; 200])[..4], [0x82, 126, 0, 200]);
    }
}
