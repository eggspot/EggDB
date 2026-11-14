use crate::streamed::Upgrade;
use crate::{Reply, Request, Response};
use std::io::{self, Read, Write};

const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MOST_MESSAGE_BYTES: usize = 16 << 20;
const READ_CHUNK_BYTES: usize = 16 * 1024;
const TEXT: u8 = 0x1;
const BINARY: u8 = 0x2;
const CONTINUATION: u8 = 0x0;
const CLOSE: u8 = 0x8;
const PING: u8 = 0x9;
const PONG: u8 = 0xA;

/// What a client sent on a WebSocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketMessage {
    /// A text message.
    Text(String),
    /// A binary message.
    Binary(Vec<u8>),
    /// The client closed the WebSocket, with the status code it gave.
    Closed(Option<u16>),
}

/// The server's end of a WebSocket (RFC 6455) after the upgrade: it reads the client's
/// masked frames, answers pings, reassembles fragmented messages, and writes unmasked frames.
pub struct WebSocket<'c> {
    reader: &'c mut dyn Read,
    writer: &'c mut dyn Write,
    buffered: Vec<u8>,
    fragments: Option<(u8, Vec<u8>)>,
    protocol: Option<String>,
    closed: bool,
}

impl std::fmt::Debug for WebSocket<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocket")
            .field("buffered", &self.buffered.len())
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

fn protocol_error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

/// `Sec-WebSocket-Accept` for a client's `Sec-WebSocket-Key`.
pub fn websocket_accept_key(key: &str) -> String {
    let mut input = key.trim().as_bytes().to_vec();
    input.extend_from_slice(ACCEPT_GUID.as_bytes());
    eggdb_crypto::base64_encode(&eggdb_crypto::sha1(&input))
}

/// Whether `request` asks to switch to a WebSocket.
pub fn wants_websocket(request: &Request) -> bool {
    request.method == "GET"
        && request
            .header("upgrade")
            .is_some_and(|upgrade| upgrade.eq_ignore_ascii_case("websocket"))
}

/// The reply that switches `request`'s connection to a WebSocket speaking the first of
/// `protocols` the client offered in `Sec-WebSocket-Protocol` (or none when `protocols` is
/// empty), and then runs `session` on it until it returns. A request that is not a valid
/// WebSocket opening handshake, or that offers none of `protocols`, gets a `400` or `426`.
pub fn websocket_reply<'a>(
    request: &Request,
    protocols: &[&str],
    session: impl FnOnce(&mut WebSocket<'_>) -> io::Result<()> + 'a,
) -> Reply<'a> {
    if !wants_websocket(request) {
        return Response::text(426, "this resource takes a WebSocket: send Upgrade: websocket").into();
    }
    if request.header("sec-websocket-version").map(str::trim) != Some("13") {
        let mut response = Response::text(426, "WebSocket version 13 is required");
        response.headers.push(("Sec-WebSocket-Version".to_owned(), "13".to_owned()));
        return response.into();
    }
    let Some(key) = request.header("sec-websocket-key") else {
        return Response::text(400, "the WebSocket handshake has no Sec-WebSocket-Key").into();
    };
    let offered: Vec<&str> = request
        .header("sec-websocket-protocol")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|protocol| !protocol.is_empty())
        .collect();
    let chosen = protocols
        .iter()
        .find(|protocol| offered.iter().any(|offer| offer.eq_ignore_ascii_case(protocol)))
        .map(|protocol| (*protocol).to_owned());
    if !protocols.is_empty() && chosen.is_none() {
        return Response::text(
            400,
            &format!("offer one of these WebSocket subprotocols: {}", protocols.join(", ")),
        )
        .into();
    }
    let mut upgrade = Upgrade::new("websocket", move |reader, writer| {
        let mut socket = WebSocket {
            reader,
            writer,
            buffered: Vec::new(),
            fragments: None,
            protocol: chosen,
            closed: false,
        };
        let outcome = session(&mut socket);
        let _ = socket.close(1000);
        outcome
    })
    .with_header("Sec-WebSocket-Accept", &websocket_accept_key(key));
    if let Some(protocol) = protocols
        .iter()
        .find(|protocol| offered.iter().any(|offer| offer.eq_ignore_ascii_case(protocol)))
    {
        upgrade = upgrade.with_header("Sec-WebSocket-Protocol", protocol);
    }
    Reply::Upgraded(upgrade)
}

impl WebSocket<'_> {
    /// The subprotocol the handshake chose.
    pub fn protocol(&self) -> Option<&str> {
        self.protocol.as_deref()
    }

    fn frame_header(opcode: u8, length: usize) -> Vec<u8> {
        let mut head = vec![0x80 | opcode];
        if length < 126 {
            head.push(length as u8);
        } else if length <= usize::from(u16::MAX) {
            head.push(126);
            head.extend_from_slice(&(length as u16).to_be_bytes());
        } else {
            head.push(127);
            head.extend_from_slice(&(length as u64).to_be_bytes());
        }
        head
    }

    fn send(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut frame = Self::frame_header(opcode, payload.len());
        frame.extend_from_slice(payload);
        self.writer.write_all(&frame)?;
        self.writer.flush()
    }

    /// Sends a text message.
    pub fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.send(TEXT, text.as_bytes())
    }

    /// Sends a binary message.
    pub fn send_binary(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.send(BINARY, bytes)
    }

    /// Sends a ping, which a live client answers with a pong.
    pub fn ping(&mut self) -> io::Result<()> {
        self.send(PING, b"")
    }

    /// Sends a close frame with `code`, once: a later close sends nothing.
    pub fn close(&mut self, code: u16) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.send(CLOSE, &code.to_be_bytes())
    }

    fn parsed_frame(&mut self) -> io::Result<Option<Frame>> {
        let bytes = &self.buffered;
        if bytes.len() < 2 {
            return Ok(None);
        }
        let fin = bytes[0] & 0x80 != 0;
        if bytes[0] & 0x70 != 0 {
            return Err(protocol_error("a WebSocket frame sets reserved bits"));
        }
        let opcode = bytes[0] & 0x0F;
        if bytes[1] & 0x80 == 0 {
            return Err(protocol_error("a client's WebSocket frame is not masked"));
        }
        let (length, mut at) = match bytes[1] & 0x7F {
            126 => match bytes.get(2..4) {
                Some(raw) => (usize::from(u16::from_be_bytes([raw[0], raw[1]])), 4),
                None => return Ok(None),
            },
            127 => match bytes.get(2..10).and_then(|raw| <[u8; 8]>::try_from(raw).ok()) {
                Some(raw) => (
                    usize::try_from(u64::from_be_bytes(raw))
                        .map_err(|_| protocol_error("a WebSocket frame is too long"))?,
                    10,
                ),
                None => return Ok(None),
            },
            short => (usize::from(short), 2),
        };
        if length > MOST_MESSAGE_BYTES {
            return Err(protocol_error("a WebSocket frame is too long"));
        }
        if opcode >= CLOSE && (length > 125 || !fin) {
            return Err(protocol_error("a WebSocket control frame is too long or fragmented"));
        }
        let Some(mask) = bytes.get(at..at + 4).and_then(|raw| <[u8; 4]>::try_from(raw).ok()) else {
            return Ok(None);
        };
        at += 4;
        let Some(masked) = bytes.get(at..at + length) else {
            return Ok(None);
        };
        let payload: Vec<u8> = masked
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4])
            .collect();
        self.buffered.drain(..at + length);
        Ok(Some(Frame { fin, opcode, payload }))
    }

    fn message_from(&mut self, frame: Frame) -> io::Result<Option<WebSocketMessage>> {
        match frame.opcode {
            PING => {
                self.send(PONG, &frame.payload)?;
                Ok(None)
            }
            PONG => Ok(None),
            CLOSE => {
                let code = frame
                    .payload
                    .get(..2)
                    .map(|raw| u16::from_be_bytes([raw[0], raw[1]]));
                Ok(Some(WebSocketMessage::Closed(code)))
            }
            TEXT | BINARY => {
                if self.fragments.is_some() {
                    return Err(protocol_error("a new WebSocket message began inside a fragmented one"));
                }
                if frame.fin {
                    return Self::whole(frame.opcode, frame.payload).map(Some);
                }
                self.fragments = Some((frame.opcode, frame.payload));
                Ok(None)
            }
            CONTINUATION => {
                let Some((opcode, mut gathered)) = self.fragments.take() else {
                    return Err(protocol_error("a WebSocket continuation frame has no message to continue"));
                };
                if gathered.len() + frame.payload.len() > MOST_MESSAGE_BYTES {
                    return Err(protocol_error("a WebSocket message is too long"));
                }
                gathered.extend_from_slice(&frame.payload);
                if frame.fin {
                    return Self::whole(opcode, gathered).map(Some);
                }
                self.fragments = Some((opcode, gathered));
                Ok(None)
            }
            _ => Err(protocol_error("a WebSocket frame has an unknown opcode")),
        }
    }

    fn whole(opcode: u8, payload: Vec<u8>) -> io::Result<WebSocketMessage> {
        if opcode == TEXT {
            String::from_utf8(payload)
                .map(WebSocketMessage::Text)
                .map_err(|_| protocol_error("a WebSocket text message is not UTF-8"))
        } else {
            Ok(WebSocketMessage::Binary(payload))
        }
    }

    /// The next whole message the client sent, if one has arrived: `Ok(None)` when none is
    /// ready yet (the connection's read timed out), so a session can do other work between
    /// messages. A connection the client dropped reads as `Closed(None)`.
    pub fn poll_message(&mut self) -> io::Result<Option<WebSocketMessage>> {
        loop {
            while let Some(frame) = self.parsed_frame()? {
                if let Some(message) = self.message_from(frame)? {
                    return Ok(Some(message));
                }
            }
            let mut chunk = [0u8; READ_CHUNK_BYTES];
            match self.reader.read(&mut chunk) {
                Ok(0) => return Ok(Some(WebSocketMessage::Closed(None))),
                Ok(count) => {
                    if self.buffered.len() + count > MOST_MESSAGE_BYTES + 14 {
                        return Err(protocol_error("a WebSocket frame is too long"));
                    }
                    self.buffered.extend_from_slice(&chunk[..count]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WebSocket, WebSocketMessage, websocket_accept_key};
    use std::io::Cursor;

    fn masked(opcode_byte: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let mut frame = vec![opcode_byte, 0x80 | payload.len() as u8];
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(at, byte)| byte ^ mask[at % 4]));
        frame
    }

    #[test]
    fn the_accept_key_follows_rfc_6455() {
        assert_eq!(
            websocket_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRp1zmrE="
        );
    }

    #[test]
    fn fragmented_messages_pings_and_close_are_read_as_rfc_6455_says() {
        let mut input = masked(0x01, b"Hel");
        input.extend(masked(0x89, b"!"));
        input.extend(masked(0x80, b"lo"));
        input.extend(masked(0x88, &1000u16.to_be_bytes()));
        let mut reader = Cursor::new(input);
        let mut written = Vec::new();
        let mut socket = WebSocket {
            reader: &mut reader,
            writer: &mut written,
            buffered: Vec::new(),
            fragments: None,
            protocol: None,
            closed: false,
        };
        assert_eq!(
            socket.poll_message().unwrap(),
            Some(WebSocketMessage::Text("Hello".to_owned()))
        );
        assert_eq!(
            socket.poll_message().unwrap(),
            Some(WebSocketMessage::Closed(Some(1000)))
        );
        assert_eq!(written, vec![0x8A, 0x01, b'!']);
    }

    #[test]
    fn unmasked_and_oversized_control_frames_are_errors() {
        let mut reader = Cursor::new(vec![0x81, 0x01, b'x']);
        let mut written = Vec::new();
        let mut socket = WebSocket {
            reader: &mut reader,
            writer: &mut written,
            buffered: Vec::new(),
            fragments: None,
            protocol: None,
            closed: false,
        };
        assert!(socket.poll_message().is_err());
    }
}
