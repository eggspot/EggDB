use std::io::{self, Read, Write};

/// The largest frame payload EggDB accepts and sends: HTTP/2's default `SETTINGS_MAX_FRAME_SIZE`.
pub const MAX_FRAME_PAYLOAD: usize = 16_384;
const HEADER_BYTES: usize = 9;

/// The frame types of RFC 9113 section 6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// `DATA`.
    Data,
    /// `HEADERS`.
    Headers,
    /// `PRIORITY`.
    Priority,
    /// `RST_STREAM`.
    ResetStream,
    /// `SETTINGS`.
    Settings,
    /// `PUSH_PROMISE`.
    PushPromise,
    /// `PING`.
    Ping,
    /// `GOAWAY`.
    GoAway,
    /// `WINDOW_UPDATE`.
    WindowUpdate,
    /// `CONTINUATION`.
    Continuation,
    /// A type this implementation ignores, as RFC 9113 says to.
    Unknown(u8),
}

impl FrameKind {
    fn of(code: u8) -> Self {
        match code {
            0 => Self::Data,
            1 => Self::Headers,
            2 => Self::Priority,
            3 => Self::ResetStream,
            4 => Self::Settings,
            5 => Self::PushPromise,
            6 => Self::Ping,
            7 => Self::GoAway,
            8 => Self::WindowUpdate,
            9 => Self::Continuation,
            other => Self::Unknown(other),
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Data => 0,
            Self::Headers => 1,
            Self::Priority => 2,
            Self::ResetStream => 3,
            Self::Settings => 4,
            Self::PushPromise => 5,
            Self::Ping => 6,
            Self::GoAway => 7,
            Self::WindowUpdate => 8,
            Self::Continuation => 9,
            Self::Unknown(code) => code,
        }
    }
}

/// `END_STREAM` on `DATA` and `HEADERS`.
pub const END_STREAM: u8 = 0x1;
/// `ACK` on `SETTINGS` and `PING`.
pub const ACK: u8 = 0x1;
/// `END_HEADERS` on `HEADERS` and `CONTINUATION`.
pub const END_HEADERS: u8 = 0x4;
/// `PADDED` on `DATA` and `HEADERS`.
pub const PADDED: u8 = 0x8;
/// `PRIORITY` on `HEADERS`.
pub const PRIORITY: u8 = 0x20;

/// Why a connection failed, with the HTTP/2 error code to send in `GOAWAY`.
#[derive(Debug)]
pub struct Http2Error {
    /// The error code of RFC 9113 section 7.
    pub code: u32,
    /// What went wrong.
    pub message: String,
}

impl Http2Error {
    /// A `PROTOCOL_ERROR`.
    pub fn protocol(message: impl Into<String>) -> Self {
        Self { code: 0x1, message: message.into() }
    }

    /// A `COMPRESSION_ERROR`.
    pub fn compression(message: impl Into<String>) -> Self {
        Self { code: 0x9, message: message.into() }
    }

    /// A `FRAME_SIZE_ERROR`.
    pub fn frame_size(message: impl Into<String>) -> Self {
        Self { code: 0x6, message: message.into() }
    }

    /// A `FLOW_CONTROL_ERROR`.
    pub fn flow_control(message: impl Into<String>) -> Self {
        Self { code: 0x3, message: message.into() }
    }
}

impl From<io::Error> for Http2Error {
    fn from(error: io::Error) -> Self {
        Self { code: 0x2, message: error.to_string() }
    }
}

impl std::fmt::Display for Http2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP/2 error {}: {}", self.code, self.message)
    }
}

/// One frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Its type.
    pub kind: FrameKind,
    /// Its flags.
    pub flags: u8,
    /// Its stream, 0 for the connection.
    pub stream: u32,
    /// Its payload.
    pub payload: Vec<u8>,
}

impl Frame {
    /// A frame of `kind` on `stream`.
    pub fn new(kind: FrameKind, flags: u8, stream: u32, payload: Vec<u8>) -> Self {
        Self { kind, flags, stream, payload }
    }

    /// Reads one frame, refusing payloads longer than `max_payload`.
    pub fn read(reader: &mut impl Read, max_payload: usize) -> Result<Self, Http2Error> {
        let mut header = [0u8; HEADER_BYTES];
        reader.read_exact(&mut header)?;
        let length = usize::from(header[0]) << 16 | usize::from(header[1]) << 8 | usize::from(header[2]);
        if length > max_payload {
            return Err(Http2Error::frame_size(format!("a frame of {length} bytes")));
        }
        let stream = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) & 0x7FFF_FFFF;
        let mut payload = vec![0u8; length];
        reader.read_exact(&mut payload)?;
        Ok(Self { kind: FrameKind::of(header[3]), flags: header[4], stream, payload })
    }

    /// Writes the frame.
    pub fn write(&self, writer: &mut impl Write) -> Result<(), Http2Error> {
        let length = self.payload.len();
        if length > MAX_FRAME_PAYLOAD {
            return Err(Http2Error::frame_size("a frame to send is too long"));
        }
        let mut header = [0u8; HEADER_BYTES];
        header[0] = (length >> 16) as u8;
        header[1] = (length >> 8) as u8;
        header[2] = length as u8;
        header[3] = self.kind.code();
        header[4] = self.flags;
        header[5..].copy_from_slice(&(self.stream & 0x7FFF_FFFF).to_be_bytes());
        writer.write_all(&header)?;
        writer.write_all(&self.payload)?;
        Ok(())
    }

    /// The payload without padding (and, on `HEADERS`, without the priority fields).
    pub fn content(&self) -> Result<&[u8], Http2Error> {
        let mut payload = self.payload.as_slice();
        let mut padding = 0usize;
        if self.flags & PADDED != 0 && matches!(self.kind, FrameKind::Data | FrameKind::Headers) {
            let (&length, rest) = payload.split_first().ok_or_else(|| Http2Error::protocol("a padded frame without its pad length"))?;
            padding = usize::from(length);
            payload = rest;
        }
        if self.kind == FrameKind::Headers && self.flags & PRIORITY != 0 {
            payload = payload.get(5..).ok_or_else(|| Http2Error::protocol("HEADERS too short for its priority"))?;
        }
        let end = payload.len().checked_sub(padding).ok_or_else(|| Http2Error::protocol("padding longer than the frame"))?;
        Ok(&payload[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_strip_padding() {
        let frame = Frame::new(FrameKind::Data, PADDED, 3, vec![2, b'h', b'i', 0, 0]);
        let mut bytes = Vec::new();
        frame.write(&mut bytes).unwrap();
        let read = Frame::read(&mut bytes.as_slice(), MAX_FRAME_PAYLOAD).unwrap();
        assert_eq!(read, frame);
        assert_eq!(read.content().unwrap(), b"hi");
        let short = Frame::new(FrameKind::Data, PADDED, 3, vec![9, 1]);
        assert!(short.content().is_err());
    }
}
