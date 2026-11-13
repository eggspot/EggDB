use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_HANDSHAKE_BYTES: usize = 8 * 1024;
const HANDSHAKE_WAIT: Duration = Duration::from_secs(10);
pub(crate) const MAX_MESSAGE_BYTES: usize = 16 << 20;
const OPCODE_CONTINUATION: u8 = 0x0;
pub(crate) const OPCODE_TEXT: u8 = 0x1;
pub(crate) const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xA;
const FINAL_FRAGMENT: u8 = 0x80;
const MASKED: u8 = 0x80;

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

/// Reads the client's opening handshake and answers `101 Switching Protocols`, or `400` when
/// the request is not a WebSocket upgrade.
pub(crate) fn handshake(stream: &mut (impl Read + Write)) -> io::Result<()> {
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
    let (true, Some(key)) = (upgrade && text.starts_with("GET "), header(&text, "sec-websocket-key")) else {
        stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a WebSocket upgrade"));
    };
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(key)
    );
    stream.write_all(response.as_bytes())
}

/// One frame from the server: never masked, never fragmented.
pub(crate) fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
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

/// A whole message from the client: its opcode (text or binary) and its unmasked payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Message {
    pub(crate) opcode: u8,
    pub(crate) payload: Vec<u8>,
}

fn read_exact_waiting(stream: &mut impl Read, buffer: &mut [u8], keep_waiting: &mut dyn FnMut() -> bool) -> io::Result<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => filled += read,
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                if !keep_waiting() {
                    return Err(io::ErrorKind::Interrupted.into());
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Reads frames until a whole text or binary message arrives, answering pings on `writer`;
/// `None` when the client closes the connection. Unmasked client frames, control frames over
/// 125 bytes and messages over [`MAX_MESSAGE_BYTES`] fail.
pub(crate) fn read_message(
    stream: &mut impl Read,
    writer: &mut impl Write,
    keep_waiting: &mut dyn FnMut() -> bool,
) -> io::Result<Option<Message>> {
    let mut message: Option<Message> = None;
    loop {
        let mut head = [0u8; 2];
        read_exact_waiting(stream, &mut head, keep_waiting)?;
        let last = head[0] & FINAL_FRAGMENT != 0;
        let opcode = head[0] & 0x0F;
        if head[1] & MASKED == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a client frame is not masked"));
        }
        let length = match head[1] & 0x7F {
            126 => {
                let mut bytes = [0u8; 2];
                read_exact_waiting(stream, &mut bytes, keep_waiting)?;
                u64::from(u16::from_be_bytes(bytes))
            }
            127 => {
                let mut bytes = [0u8; 8];
                read_exact_waiting(stream, &mut bytes, keep_waiting)?;
                u64::from_be_bytes(bytes)
            }
            length => u64::from(length),
        };
        let held = message.as_ref().map_or(0, |message| message.payload.len());
        let length = usize::try_from(length)
            .ok()
            .filter(|length| held.saturating_add(*length) <= MAX_MESSAGE_BYTES)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the message is too large"))?;
        let control = opcode & 0x8 != 0;
        if control && (length > 125 || !last) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a control frame is too long or fragmented"));
        }
        let mut mask = [0u8; 4];
        read_exact_waiting(stream, &mut mask, keep_waiting)?;
        let mut payload = vec![0u8; length];
        read_exact_waiting(stream, &mut payload, keep_waiting)?;
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
        match opcode {
            OPCODE_CLOSE => {
                let _ = writer.write_all(&frame(OPCODE_CLOSE, payload.get(..2).unwrap_or_default()));
                return Ok(None);
            }
            OPCODE_PING => writer.write_all(&frame(OPCODE_PONG, &payload))?,
            OPCODE_PONG => {}
            OPCODE_TEXT | OPCODE_BINARY if message.is_none() => {
                message = Some(Message { opcode, payload });
            }
            OPCODE_CONTINUATION => match &mut message {
                Some(message) => message.payload.extend_from_slice(&payload),
                None => return Err(io::Error::new(io::ErrorKind::InvalidData, "a continuation frame starts no message")),
            },
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "an unknown or out-of-order WebSocket frame")),
        }
        if last && !control && let Some(done) = message.take() {
            return Ok(Some(done));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_accept_key_and_a_masked_fragmented_message_follow_rfc_6455() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kxGzzhZRbK+xOo=");
        let mask = [1u8, 2, 3, 4];
        let masked = |text: &[u8]| text.iter().enumerate().map(|(index, byte)| byte ^ mask[index % 4]).collect::<Vec<u8>>();
        let mut wire = vec![OPCODE_TEXT, MASKED | 3];
        wire.extend_from_slice(&mask);
        wire.extend_from_slice(&masked(b"Hel"));
        wire.extend_from_slice(&[FINAL_FRAGMENT | OPCODE_CONTINUATION, MASKED | 2]);
        wire.extend_from_slice(&mask);
        wire.extend_from_slice(&masked(b"lo"));
        let mut reader = io::Cursor::new(wire);
        let mut written = Vec::new();
        let message = read_message(&mut reader, &mut written, &mut || true).unwrap().unwrap();
        assert_eq!(message, Message { opcode: OPCODE_TEXT, payload: b"Hello".to_vec() });
    }
}
