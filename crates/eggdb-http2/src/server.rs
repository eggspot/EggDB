use crate::frame::{ACK, END_HEADERS, END_STREAM, Frame, FrameKind, Http2Error, MAX_FRAME_PAYLOAD};
use crate::hpack::{HeaderDecoder, encode_headers};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};

const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const DEFAULT_WINDOW: i64 = 65_535;
const RECEIVE_WINDOW: u32 = 16 << 20;
const MOST_STREAMS: u32 = 100;
const MOST_REQUEST_BYTES: usize = 64 << 20;
const MOST_HEADER_BLOCK_BYTES: usize = 256 * 1024;
const MOST_WINDOW: i64 = (1 << 31) - 1;
const SETTINGS_INITIAL_WINDOW_SIZE: u16 = 0x4;
const SETTINGS_MAX_FRAME_SIZE: u16 = 0x5;
const SETTINGS_MAX_CONCURRENT_STREAMS: u16 = 0x3;

/// A request: its header fields (pseudo-headers such as `:path` included) and its body.
#[derive(Debug, Clone, Default)]
pub struct Http2Request {
    /// The header fields, in order.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Http2Request {
    /// The first value of header `name`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }

    /// The `:path` pseudo-header.
    pub fn path(&self) -> &str {
        self.header(":path").unwrap_or("/")
    }
}

/// A response: header fields (`:status` first), the body in chunks, and trailers.
#[derive(Debug, Clone, Default)]
pub struct Http2Response {
    /// The header fields.
    pub headers: Vec<(String, String)>,
    /// The body, sent as one or more `DATA` frames per chunk.
    pub body: Vec<Vec<u8>>,
    /// The trailer fields, sent after the body; none ends the stream with the last `DATA`.
    pub trailers: Vec<(String, String)>,
}

/// What answers the requests of a connection.
pub trait Handler {
    /// The response to `request`.
    fn handle(&self, request: &Http2Request) -> Http2Response;
}

#[derive(Default)]
struct OpenStream {
    block: Vec<u8>,
    ends_with_headers: bool,
    headers: Option<Vec<(String, String)>>,
    body: Vec<u8>,
}

struct Connection<'s, S: Read + Write> {
    stream: &'s mut S,
    decoder: HeaderDecoder,
    open: HashMap<u32, OpenStream>,
    ready: VecDeque<(u32, Http2Request)>,
    continuing: Option<u32>,
    send_window: i64,
    stream_windows: HashMap<u32, i64>,
    initial_window: i64,
    max_frame: usize,
    closed: bool,
}

fn settings_payload(pairs: &[(u16, u32)]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(pairs.len() * 6);
    for (id, value) in pairs {
        payload.extend_from_slice(&id.to_be_bytes());
        payload.extend_from_slice(&value.to_be_bytes());
    }
    payload
}

impl<S: Read + Write> Connection<'_, S> {
    fn send(&mut self, frame: Frame) -> Result<(), Http2Error> {
        frame.write(&mut *self.stream)
    }

    fn window_update(&mut self, stream: u32, increment: usize) -> Result<(), Http2Error> {
        if increment == 0 {
            return Ok(());
        }
        let increment = u32::try_from(increment).unwrap_or(u32::MAX >> 1);
        self.send(Frame::new(FrameKind::WindowUpdate, 0, stream, increment.to_be_bytes().to_vec()))
    }

    fn finish_headers(&mut self, stream: u32, end_stream: bool) -> Result<(), Http2Error> {
        let open = self.open.entry(stream).or_default();
        let block = std::mem::take(&mut open.block);
        let fields = self.decoder.decode(&block)?;
        let open = self.open.entry(stream).or_default();
        if open.headers.is_none() {
            open.headers = Some(fields);
        }
        if end_stream {
            self.complete(stream);
        }
        Ok(())
    }

    fn complete(&mut self, stream: u32) {
        if let Some(open) = self.open.remove(&stream) {
            self.ready.push_back((
                stream,
                Http2Request {
                    headers: open.headers.unwrap_or_default(),
                    body: open.body,
                },
            ));
        }
    }

    fn apply_settings(&mut self, payload: &[u8]) -> Result<(), Http2Error> {
        if payload.len() % 6 != 0 {
            return Err(Http2Error::frame_size("SETTINGS not a multiple of 6 bytes"));
        }
        for pair in payload.chunks(6) {
            let id = u16::from_be_bytes([pair[0], pair[1]]);
            let value = u32::from_be_bytes([pair[2], pair[3], pair[4], pair[5]]);
            match id {
                SETTINGS_INITIAL_WINDOW_SIZE => {
                    let value = i64::from(value);
                    if value > MOST_WINDOW {
                        return Err(Http2Error::flow_control("an initial window above 2^31 - 1"));
                    }
                    let delta = value - self.initial_window;
                    for window in self.stream_windows.values_mut() {
                        *window += delta;
                    }
                    self.initial_window = value;
                }
                SETTINGS_MAX_FRAME_SIZE => {
                    if !(16_384..=16_777_215).contains(&value) {
                        return Err(Http2Error::protocol("a maximum frame size out of range"));
                    }
                    self.max_frame = (value as usize).min(MAX_FRAME_PAYLOAD);
                }
                _ => {}
            }
        }
        self.send(Frame::new(FrameKind::Settings, ACK, 0, Vec::new()))
    }

    fn receive(&mut self) -> Result<(), Http2Error> {
        let frame = Frame::read(&mut *self.stream, MAX_FRAME_PAYLOAD)?;
        if let Some(expected) = self.continuing
            && (frame.kind != FrameKind::Continuation || frame.stream != expected)
        {
            return Err(Http2Error::protocol("a header block interrupted"));
        }
        match frame.kind {
            FrameKind::Headers | FrameKind::Continuation => {
                if frame.stream == 0 || frame.stream % 2 == 0 {
                    return Err(Http2Error::protocol("a request on an even or zero stream"));
                }
                if frame.kind == FrameKind::Headers && !self.open.contains_key(&frame.stream) {
                    if self.open.len() >= MOST_STREAMS as usize {
                        return Err(Http2Error::protocol("more streams than announced"));
                    }
                    self.stream_windows.insert(frame.stream, self.initial_window);
                }
                let content = frame.content()?.to_vec();
                let open = self.open.entry(frame.stream).or_default();
                if open.block.len() + content.len() > MOST_HEADER_BLOCK_BYTES {
                    return Err(Http2Error::protocol("a header block larger than 256 KiB"));
                }
                open.block.extend_from_slice(&content);
                if frame.kind == FrameKind::Headers {
                    open.ends_with_headers = frame.flags & END_STREAM != 0;
                }
                let end_stream = open.ends_with_headers;
                if frame.flags & END_HEADERS != 0 {
                    self.continuing = None;
                    self.finish_headers(frame.stream, end_stream)?;
                } else {
                    self.continuing = Some(frame.stream);
                }
            }
            FrameKind::Data => {
                let content = frame.content()?.to_vec();
                let received = frame.payload.len();
                let Some(open) = self.open.get_mut(&frame.stream) else {
                    return self.window_update(0, received);
                };
                if open.body.len() + content.len() > MOST_REQUEST_BYTES {
                    return Err(Http2Error::protocol("a request body larger than 64 MiB"));
                }
                open.body.extend_from_slice(&content);
                self.window_update(0, received)?;
                if frame.flags & END_STREAM != 0 {
                    self.complete(frame.stream);
                } else {
                    self.window_update(frame.stream, received)?;
                }
            }
            FrameKind::Settings if frame.flags & ACK == 0 => self.apply_settings(&frame.payload)?,
            FrameKind::Ping if frame.flags & ACK == 0 => {
                if frame.payload.len() != 8 {
                    return Err(Http2Error::frame_size("a PING that is not 8 bytes"));
                }
                self.send(Frame::new(FrameKind::Ping, ACK, 0, frame.payload))?;
            }
            FrameKind::WindowUpdate => {
                let bytes: [u8; 4] = frame
                    .payload
                    .as_slice()
                    .try_into()
                    .map_err(|_| Http2Error::frame_size("a WINDOW_UPDATE that is not 4 bytes"))?;
                let increment = i64::from(u32::from_be_bytes(bytes) & 0x7FFF_FFFF);
                let window = if frame.stream == 0 {
                    &mut self.send_window
                } else {
                    self.stream_windows.entry(frame.stream).or_insert(DEFAULT_WINDOW)
                };
                *window += increment;
                if *window > MOST_WINDOW {
                    return Err(Http2Error::flow_control("a window above 2^31 - 1"));
                }
            }
            FrameKind::ResetStream => {
                self.open.remove(&frame.stream);
                self.ready.retain(|(stream, _)| *stream != frame.stream);
            }
            FrameKind::GoAway => self.closed = true,
            FrameKind::PushPromise => return Err(Http2Error::protocol("a client sent PUSH_PROMISE")),
            _ => {}
        }
        Ok(())
    }

    fn send_headers(&mut self, stream: u32, fields: &[(String, String)], end_stream: bool) -> Result<(), Http2Error> {
        let borrowed: Vec<(&str, &str)> = fields.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
        let block = encode_headers(&borrowed);
        let mut chunks = block.chunks(self.max_frame).peekable();
        let mut first = true;
        if block.is_empty() {
            let flags = END_HEADERS | if end_stream { END_STREAM } else { 0 };
            return self.send(Frame::new(FrameKind::Headers, flags, stream, Vec::new()));
        }
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            let mut flags = if last { END_HEADERS } else { 0 };
            if first && end_stream {
                flags |= END_STREAM;
            }
            let kind = if first { FrameKind::Headers } else { FrameKind::Continuation };
            self.send(Frame::new(kind, flags, stream, chunk.to_vec()))?;
            first = false;
        }
        Ok(())
    }

    fn send_data(&mut self, stream: u32, mut data: &[u8], end_stream: bool) -> Result<(), Http2Error> {
        loop {
            let stream_window = self.stream_windows.get(&stream).copied().unwrap_or(self.initial_window);
            let allowed = self.send_window.min(stream_window).max(0) as usize;
            if allowed == 0 && !data.is_empty() {
                self.receive()?;
                if self.closed {
                    return Err(Http2Error::protocol("the client went away while a response was sent"));
                }
                continue;
            }
            let size = data.len().min(allowed).min(self.max_frame);
            let (chunk, rest) = data.split_at(size);
            let last = rest.is_empty();
            let flags = if last && end_stream { END_STREAM } else { 0 };
            self.send(Frame::new(FrameKind::Data, flags, stream, chunk.to_vec()))?;
            self.send_window -= size as i64;
            if let Some(window) = self.stream_windows.get_mut(&stream) {
                *window -= size as i64;
            }
            data = rest;
            if last {
                return Ok(());
            }
        }
    }

    fn respond(&mut self, stream: u32, response: Http2Response) -> Result<(), Http2Error> {
        let has_trailers = !response.trailers.is_empty();
        let body: Vec<Vec<u8>> = response.body.into_iter().filter(|chunk| !chunk.is_empty()).collect();
        self.send_headers(stream, &response.headers, body.is_empty() && !has_trailers)?;
        let count = body.len();
        for (index, chunk) in body.iter().enumerate() {
            self.send_data(stream, chunk, index + 1 == count && !has_trailers)?;
        }
        if has_trailers {
            self.send_headers(stream, &response.trailers, true)?;
        }
        self.stream_windows.remove(&stream);
        self.stream.flush()?;
        Ok(())
    }
}

/// Serves one HTTP/2 connection (prior knowledge, or after TLS with ALPN `h2`) until the
/// client goes away, answering each complete request with `handler`, one at a time.
pub fn serve_connection<S: Read + Write>(stream: &mut S, handler: &dyn Handler) -> Result<(), Http2Error> {
    let mut preface = [0u8; 24];
    stream.read_exact(&mut preface)?;
    if &preface != PREFACE {
        return Err(Http2Error::protocol("the connection did not start with HTTP/2's preface"));
    }
    let mut connection = Connection {
        stream,
        decoder: HeaderDecoder::default(),
        open: HashMap::new(),
        ready: VecDeque::new(),
        continuing: None,
        send_window: DEFAULT_WINDOW,
        stream_windows: HashMap::new(),
        initial_window: DEFAULT_WINDOW,
        max_frame: MAX_FRAME_PAYLOAD,
        closed: false,
    };
    let settings = settings_payload(&[
        (SETTINGS_MAX_CONCURRENT_STREAMS, MOST_STREAMS),
        (SETTINGS_INITIAL_WINDOW_SIZE, RECEIVE_WINDOW),
    ]);
    connection.send(Frame::new(FrameKind::Settings, 0, 0, settings))?;
    connection.window_update(0, (RECEIVE_WINDOW as usize).saturating_sub(DEFAULT_WINDOW as usize))?;
    connection.stream.flush()?;
    loop {
        while let Some((stream_id, request)) = connection.ready.pop_front() {
            let response = handler.handle(&request);
            connection.respond(stream_id, response)?;
        }
        if connection.closed {
            return Ok(());
        }
        match connection.receive() {
            Ok(()) => {}
            Err(error) if error.code == 0x2 => return Ok(()),
            Err(error) => {
                let mut payload = 0u32.to_be_bytes().to_vec();
                payload.extend_from_slice(&error.code.to_be_bytes());
                let _ = connection.send(Frame::new(FrameKind::GoAway, 0, 0, payload));
                return Err(error);
            }
        }
        connection.stream.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Echo;

    impl Handler for Echo {
        fn handle(&self, request: &Http2Request) -> Http2Response {
            Http2Response {
                headers: vec![(":status".to_owned(), "200".to_owned())],
                body: vec![request.body.clone()],
                trailers: vec![("grpc-status".to_owned(), "0".to_owned())],
            }
        }
    }

    struct Duplex {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for Duplex {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buffer)
        }
    }

    impl Write for Duplex {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_request_gets_headers_data_and_trailers() {
        let mut input = PREFACE.to_vec();
        Frame::new(FrameKind::Settings, 0, 0, Vec::new()).write(&mut input).unwrap();
        let block = encode_headers(&[(":method", "POST"), (":path", "/echo")]);
        Frame::new(FrameKind::Headers, END_HEADERS, 1, block).write(&mut input).unwrap();
        Frame::new(FrameKind::Data, END_STREAM, 1, b"hello".to_vec()).write(&mut input).unwrap();
        let mut duplex = Duplex { input: Cursor::new(input), output: Vec::new() };
        serve_connection(&mut duplex, &Echo).unwrap();
        let mut output = duplex.output.as_slice();
        let mut frames = Vec::new();
        while !output.is_empty() {
            frames.push(Frame::read(&mut output, MAX_FRAME_PAYLOAD).unwrap());
        }
        let data: Vec<&Frame> = frames.iter().filter(|frame| frame.kind == FrameKind::Data).collect();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].payload, b"hello");
        let last = frames.last().unwrap();
        assert_eq!((last.kind, last.flags & END_STREAM), (FrameKind::Headers, END_STREAM));
    }
}
