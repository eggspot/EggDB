use crate::Response;
use std::fmt;
use std::io::{self, Write};

/// The bytes a streamed body gathers before it sends them as one chunk.
pub const STREAM_CHUNK_BYTES: usize = 16 * 1024;

/// Where a [`StreamedResponse`] writes its body: bytes are gathered into chunks of
/// [`STREAM_CHUNK_BYTES`] and sent as each fills, with HTTP/1.1's chunked transfer coding (or
/// plain, ended by closing the connection, to an HTTP/1.0 client), so a body of any length
/// holds one chunk in memory at a time.
pub struct BodyWriter<'w> {
    out: &'w mut dyn Write,
    chunked: bool,
    pending: Vec<u8>,
    sent: u64,
}

impl fmt::Debug for BodyWriter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BodyWriter")
            .field("chunked", &self.chunked)
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl<'w> BodyWriter<'w> {
    pub(crate) fn new(out: &'w mut dyn Write, chunked: bool) -> Self {
        Self {
            out,
            chunked,
            pending: Vec::with_capacity(STREAM_CHUNK_BYTES),
            sent: 0,
        }
    }

    fn send_pending(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if self.chunked {
            let mut framed = format!("{:x}\r\n", self.pending.len()).into_bytes();
            framed.reserve(self.pending.len() + 2);
            framed.extend_from_slice(&self.pending);
            framed.extend_from_slice(b"\r\n");
            self.out.write_all(&framed)?;
        } else {
            self.out.write_all(&self.pending)?;
        }
        self.sent = self.sent.saturating_add(self.pending.len() as u64);
        self.pending.clear();
        Ok(())
    }

    pub(crate) fn finish(mut self) -> io::Result<u64> {
        self.send_pending()?;
        if self.chunked {
            self.out.write_all(b"0\r\n\r\n")?;
        }
        self.out.flush()?;
        Ok(self.sent)
    }
}

impl Write for BodyWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = STREAM_CHUNK_BYTES.saturating_sub(self.pending.len());
        let taken = bytes.len().min(room.max(1));
        self.pending
            .extend_from_slice(bytes.get(..taken).unwrap_or_default());
        if self.pending.len() >= STREAM_CHUNK_BYTES {
            self.send_pending()?;
        }
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send_pending()?;
        self.out.flush()
    }
}

/// What writes a [`StreamedResponse`]'s body; an error after the head is sent cuts the body
/// short and closes the connection, so the client sees an incomplete response.
pub type BodyProducer<'a> = Box<dyn FnOnce(&mut BodyWriter<'_>) -> io::Result<()> + 'a>;

/// A response whose body's length is not known when its head is sent, such as rows a query
/// produces: the server sends it with `Transfer-Encoding: chunked` to HTTP/1.1 clients and
/// keeps the connection, and to HTTP/1.0 clients ends it by closing the connection.
pub struct StreamedResponse<'a> {
    /// The status code.
    pub status: u16,
    /// The `Content-Type` of the body.
    pub content_type: String,
    /// More header fields to send; `Content-Length`, `Transfer-Encoding` and `Connection` are
    /// the server's and are left out.
    pub headers: Vec<(String, String)>,
    /// Writes the body; not called for a `HEAD` request.
    pub body: BodyProducer<'a>,
    /// The bytes the body's source keeps in memory until the body is sent, such as a query's
    /// rows; the server holds them against its memory budget while it sends the body, and
    /// answers its over-budget response instead when they do not fit.
    pub held_bytes: usize,
    /// The body's length when it is known before it is written, such as a stored object's:
    /// the server then sends `Content-Length` instead of the chunked coding, and closes the
    /// connection when the body writes a different number of bytes.
    pub content_length: Option<u64>,
}

impl fmt::Debug for StreamedResponse<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamedResponse")
            .field("status", &self.status)
            .field("content_type", &self.content_type)
            .field("headers", &self.headers)
            .field("held_bytes", &self.held_bytes)
            .field("content_length", &self.content_length)
            .finish_non_exhaustive()
    }
}

impl<'a> StreamedResponse<'a> {
    /// A response with `status` whose body of `content_type` `body` writes as it is sent.
    pub fn new(
        status: u16,
        content_type: &str,
        body: impl FnOnce(&mut BodyWriter<'_>) -> io::Result<()> + 'a,
    ) -> Self {
        Self {
            status,
            content_type: content_type.to_owned(),
            headers: Vec::new(),
            body: Box::new(body),
            held_bytes: 0,
            content_length: None,
        }
    }

    /// The same response, sent with `Content-Length: length`.
    pub fn with_content_length(mut self, length: u64) -> Self {
        self.content_length = Some(length);
        self
    }

    /// The same response, holding `bytes` against the server's memory budget while it is sent.
    pub fn with_held_bytes(mut self, bytes: usize) -> Self {
        self.held_bytes = bytes;
        self
    }

    /// The same response with header `name: value` added; values with line breaks are dropped.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        if !value.contains(['\r', '\n']) && !name.contains(['\r', '\n', ':']) {
            self.headers.push((name.to_owned(), value.to_owned()));
        }
        self
    }

    /// The whole response in memory: the body written into one buffer, or a `500` when
    /// writing it fails.
    pub fn collected(self) -> Response {
        let mut body = Vec::new();
        let written = {
            let mut writer = BodyWriter::new(&mut body, false);
            (self.body)(&mut writer).and_then(|()| writer.finish().map(|_| ()))
        };
        match written {
            Ok(()) => Response {
                status: self.status,
                content_type: self.content_type,
                body,
                headers: self.headers,
            },
            Err(error) => {
                Response::text(500, &format!("the response could not be written: {error}"))
            }
        }
    }
}

/// What a handler answers: a whole [`Response`], or a [`StreamedResponse`] written as it is
/// produced.
#[derive(Debug)]
pub enum Reply<'a> {
    /// A response whose body is complete, sent with `Content-Length`.
    Whole(Response),
    /// A response whose body is written in chunks as it is produced.
    Streamed(StreamedResponse<'a>),
}

impl Reply<'_> {
    /// The reply as one whole [`Response`], collecting a streamed body into memory.
    pub fn into_response(self) -> Response {
        match self {
            Self::Whole(response) => response,
            Self::Streamed(streamed) => streamed.collected(),
        }
    }
}

impl From<Response> for Reply<'_> {
    fn from(response: Response) -> Self {
        Self::Whole(response)
    }
}

impl<'a> From<StreamedResponse<'a>> for Reply<'a> {
    fn from(streamed: StreamedResponse<'a>) -> Self {
        Self::Streamed(streamed)
    }
}
