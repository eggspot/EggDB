//! A small HTTP/1.1 server written from RFC 9110 and RFC 9112. Connections persist (HTTP/1.1
//! by default, HTTP/1.0 with `Connection: keep-alive`) until `Connection: close`, an idle
//! timeout or a number of requests, and pipelined requests are answered in order. Request
//! bodies come with `Content-Length` or `Transfer-Encoding: chunked`, at most 1 MiB unless a
//! listener sets its own limit (with `100 Continue` for clients that wait for it); framing that
//! could be read two ways is refused, as RFC 9112 asks, so requests cannot be smuggled. A
//! handler answers a whole [`Response`] or a [`StreamedResponse`] sent in chunks as it is
//! produced. Every length and wait is bounded so a hostile client cannot hold a thread or
//! memory for long; that includes reading a response, which may block the server for 5 seconds
//! plus one second for each 64 KiB already sent. A listener given [`Limits`] holds every body
//! over 64 KiB, the request's (a chunked one as it arrives) and then the response's, against the
//! server's one memory budget (D284, D307), and answers with its own refusal when the budget is
//! spent. An HTTP/1.1 request without exactly one `Host` header is refused with `400`, as RFC
//! 9112 asks. A listener can decode request bodies sent with `Content-Encoding: gzip`,
//! `x-gzip` or `deflate` (stacked too) before its handler sees them, bounded by its body limit
//! and the memory budget (D343). A minimal client posts to plain `http://` URLs, for webhooks.

mod client;
mod connection;
mod content_coding;
mod rate_limit;
mod reader;
mod socket;
mod streamed;

pub use client::{ClientResponse, HttpTarget, host_header, parse_http_url, post, request};
pub use rate_limit::{RateLimiter, too_many_requests_response};
pub use streamed::{BodyProducer, BodyWriter, Reply, STREAM_CHUNK_BYTES, StreamedResponse};

use connection::ConnectionHeader;
use eggdb_platform::connection_limits::{MemoryBudget, Reservation};
use reader::{PlainSource, RequestReader};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 100;
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const MAX_CONNECTIONS: usize = 16;
const KEEP_ALIVE_BELOW_CONNECTIONS: usize = MAX_CONNECTIONS - MAX_CONNECTIONS / 4;
/// How long a persistent connection waits for its next request before the server closes it.
pub const KEEP_ALIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// The most requests one connection is served before the server closes it.
pub const MAX_REQUESTS_PER_CONNECTION: usize = 100;
/// The longest request body [`serve`] reads: 1 MiB.
pub const MAX_BODY_BYTES: usize = 1 << 20;

/// A request's line and headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The method, such as `GET`.
    pub method: String,
    /// The request target's path, without its query.
    pub path: String,
    /// The query after `?`, if any.
    pub query: Option<String>,
    /// The header fields, names in lower case, in order.
    pub headers: Vec<(String, String)>,
    /// The body, empty when the request has none.
    pub body: Vec<u8>,
}

impl Request {
    /// The first value of header `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A response to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The status code.
    pub status: u16,
    /// The `Content-Type` of the body.
    pub content_type: String,
    /// The body.
    pub body: Vec<u8>,
    /// More header fields to send, such as a session identifier.
    pub headers: Vec<(String, String)>,
}

impl Response {
    /// A `200 OK` with `body` of `content_type`.
    pub fn ok(content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: content_type.to_owned(),
            body: body.into(),
            headers: Vec::new(),
        }
    }

    /// A plain-text response with `status`.
    pub fn text(status: u16, text: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8".to_owned(),
            body: format!("{text}\n").into_bytes(),
            headers: Vec::new(),
        }
    }

    /// The same response with header `name: value` added; values with line breaks are dropped.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        if !value.contains(['\r', '\n']) && !name.contains(['\r', '\n', ':']) {
            self.headers.push((name.to_owned(), value.to_owned()));
        }
        self
    }
}

/// Why a request could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// The connection closed or failed before the request's head ended.
    Closed,
    /// The head is longer than the server reads, or has too many headers.
    TooLarge,
    /// The body is longer than the listener reads.
    BodyTooLarge,
    /// The head is not HTTP/1.x.
    Malformed,
    /// The body is longer than the server's memory budget has left.
    OverMemoryBudget,
    /// The body is sent with a transfer coding other than `chunked`.
    UnsupportedTransferCoding,
    /// An HTTP/1.1 request has no `Host` header, or a request has more than one.
    InvalidHost,
    /// The body is sent with a content coding the listener does not decode.
    UnsupportedContentCoding,
    /// The body does not decode as its `Content-Encoding` says.
    CorruptContentCoding,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Closed => "the connection closed before the request ended",
            Self::TooLarge => "the request head is too large",
            Self::BodyTooLarge => "the request body is too large",
            Self::Malformed => "the request is not HTTP/1.1",
            Self::OverMemoryBudget => "the server's connections already hold their memory budget",
            Self::UnsupportedTransferCoding => "the request's transfer coding is not supported",
            Self::InvalidHost => "the request does not have exactly one Host header",
            Self::UnsupportedContentCoding => "the request's content coding is not supported",
            Self::CorruptContentCoding => "the request body does not decode as its content coding",
        })
    }
}

impl std::error::Error for HttpError {}

fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

/// Decodes `%XX` escapes in one URL path segment or query value; `None` when an escape is not
/// two hex digits or the decoded bytes are not UTF-8. A `+` stays a `+`.
pub fn percent_decoded(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        if byte != b'%' {
            decoded.push(byte);
            at += 1;
            continue;
        }
        let high = bytes.get(at + 1).copied().and_then(hex_digit)?;
        let low = bytes.get(at + 2).copied().and_then(hex_digit)?;
        decoded.push(high << 4 | low);
        at += 3;
    }
    String::from_utf8(decoded).ok()
}

fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|digit| u8::try_from(digit).ok())
}

/// Reads a request from `stream`: its head, at most 16 KiB, and a body of at most 1 MiB given
/// by `Content-Length` or sent with `Transfer-Encoding: chunked`.
pub fn read_request(stream: &mut impl Read) -> Result<Request, HttpError> {
    read_request_limited(stream, MAX_BODY_BYTES, &mut || {})
}

/// Reads a request as [`read_request`] does, with a body of at most `max_body_bytes`, calling
/// `on_continue` before reading the body of a request that asks for `100 Continue`.
pub fn read_request_limited(
    stream: &mut impl Read,
    max_body_bytes: usize,
    on_continue: &mut dyn FnMut(),
) -> Result<Request, HttpError> {
    let unshared = MemoryBudget::default();
    let mut held = unshared.empty_reservation();
    read_request_holding(stream, max_body_bytes, &mut held, on_continue)
}

/// Reads a request as [`read_request_limited`] does, holding a body over 64 KiB in `held`
/// before it is read (and before `100 Continue` is sent), so a declared length the budget
/// cannot take costs nothing: that request fails with [`HttpError::OverMemoryBudget`]. A
/// chunked body is held as its chunks arrive, each chunk's size before its bytes are read.
pub fn read_request_holding(
    stream: &mut impl Read,
    max_body_bytes: usize,
    held: &mut Reservation<'_>,
    on_continue: &mut dyn FnMut(),
) -> Result<Request, HttpError> {
    RequestReader::new(PlainSource(stream))
        .next_request(max_body_bytes, held, on_continue)
        .map(|(request, _)| request)
}

/// Writes `response` to `stream`, without its body for a `HEAD` request, and asks the client to
/// close the connection. A `Content-Length` among the response's headers replaces the body's
/// length, so a `HEAD` answer can state the length of a body it does not carry.
pub fn write_response(
    stream: &mut impl Write,
    response: &Response,
    head_only: bool,
) -> io::Result<()> {
    connection::write_whole(stream, response, head_only, ConnectionHeader::Close)
}

/// What a listener bounds: the longest body it reads, the memory budget its connections share
/// with the server's other listeners, and the answer it gives when that budget is spent.
#[derive(Debug, Clone, Copy)]
pub struct Limits<'b> {
    /// The longest request body read; a longer one is answered `413`.
    pub max_body_bytes: usize,
    /// The bytes every connection of the server holds together.
    pub memory_budget: &'b MemoryBudget,
    /// What a request whose body, or whose response's body, the budget cannot take is
    /// answered with, in the words of the protocol the listener speaks.
    pub over_memory_budget: fn() -> Response,
    /// How long a persistent connection waits for its next request.
    pub keep_alive_idle_timeout: Duration,
    /// The most requests one connection is served; the last answer says `Connection: close`.
    pub max_requests_per_connection: usize,
    /// Which requests have their body's `Content-Encoding` (`gzip`, `x-gzip`, `deflate`,
    /// `identity`, stacked) decoded before the handler sees them; the handler then gets the
    /// decoded body with its `Content-Length` and no `Content-Encoding`. A decoded body past
    /// `max_body_bytes` is answered `413`, one the budget cannot take with
    /// `over_memory_budget()`, a damaged one `400` and another coding `415`. None by default.
    pub decode_content_encoding: fn(&Request) -> bool,
    /// The requests a second each client address may send; a request over it is answered
    /// `429` with `Retry-After` without reaching the handler. None by default.
    pub rate_limit: Option<&'b RateLimiter>,
    /// The certificate and key every connection is secured with before its first request
    /// (HTTPS); plain HTTP when `None`, the default.
    pub tls: Option<&'b eggdb_tls::ServerConfig>,
}

fn no_request(_: &Request) -> bool {
    false
}

fn every_request(_: &Request) -> bool {
    true
}

impl<'b> Limits<'b> {
    /// Limits of `max_body_bytes` per body under `memory_budget`, answering `503` when the
    /// budget is spent.
    pub fn new(max_body_bytes: usize, memory_budget: &'b MemoryBudget) -> Self {
        Self {
            max_body_bytes,
            memory_budget,
            over_memory_budget: over_memory_budget_response,
            keep_alive_idle_timeout: KEEP_ALIVE_IDLE_TIMEOUT,
            max_requests_per_connection: MAX_REQUESTS_PER_CONNECTION,
            decode_content_encoding: no_request,
            rate_limit: None,
            tls: None,
        }
    }

    /// These limits, securing every connection with `config` (HTTPS) when one is given.
    pub fn with_tls(self, config: Option<&'b eggdb_tls::ServerConfig>) -> Self {
        Self { tls: config, ..self }
    }

    /// These limits, answering `429` to a client address over `limiter`'s rate.
    pub fn with_rate_limit(self, limiter: Option<&'b RateLimiter>) -> Self {
        Self {
            rate_limit: limiter,
            ..self
        }
    }

    /// These limits, decoding every request body's `Content-Encoding` before the handler sees
    /// it.
    pub fn decoding_content_encoding(self) -> Self {
        self.decoding_content_encoding_of(every_request)
    }

    /// These limits, decoding the `Content-Encoding` of the request bodies `wanted` picks
    /// before the handler sees them.
    pub fn decoding_content_encoding_of(self, wanted: fn(&Request) -> bool) -> Self {
        Self {
            decode_content_encoding: wanted,
            ..self
        }
    }

    /// These limits, closing a persistent connection after `idle_timeout` without a request or
    /// after `max_requests` requests.
    pub fn with_keep_alive(self, idle_timeout: Duration, max_requests: usize) -> Self {
        Self {
            keep_alive_idle_timeout: idle_timeout,
            max_requests_per_connection: max_requests,
            ..self
        }
    }

    /// These limits, answering `over_memory_budget()` when the budget is spent.
    pub fn answering_over_budget_with(self, over_memory_budget: fn() -> Response) -> Self {
        Self {
            over_memory_budget,
            ..self
        }
    }
}

/// The plain answer to a request the server's memory budget cannot take: `503`, with
/// `Retry-After`, so clients back off and try again.
pub fn over_memory_budget_response() -> Response {
    Response::text(
        503,
        "the server's connections already hold their memory budget",
    )
    .with_header("Retry-After", "1")
}

/// Serves requests arriving on `listener` with `handler`, a thread per connection and at most
/// sixteen at once, until `stop` turns true. Connections persist and take pipelined requests
/// until the client closes, asks to, waits [`KEEP_ALIVE_IDLE_TIMEOUT`] or sends
/// [`MAX_REQUESTS_PER_CONNECTION`]; once twelve connections are open, each answer closes its
/// connection, so idle clients cannot keep new ones out.
pub fn serve(
    listener: &TcpListener,
    handler: &(dyn Fn(&Request) -> Response + Sync),
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    serve_with_body_limit(listener, handler, stop, MAX_BODY_BYTES)
}

/// Serves requests as [`serve`] does, accepting bodies of at most `max_body_bytes`, under a
/// memory budget of its own.
pub fn serve_with_body_limit(
    listener: &TcpListener,
    handler: &(dyn Fn(&Request) -> Response + Sync),
    stop: &(dyn Fn() -> bool + Sync),
    max_body_bytes: usize,
) -> io::Result<()> {
    let unshared = MemoryBudget::default();
    serve_with_limits(
        listener,
        handler,
        stop,
        &Limits::new(max_body_bytes, &unshared),
    )
}

/// Serves requests as [`serve`] does, within `limits`: bodies of at most
/// `limits.max_body_bytes`, and every body over 64 KiB held against `limits.memory_budget`
/// while its request is read and answered.
pub fn serve_with_limits(
    listener: &TcpListener,
    handler: &(dyn Fn(&Request) -> Response + Sync),
    stop: &(dyn Fn() -> bool + Sync),
    limits: &Limits<'_>,
) -> io::Result<()> {
    let whole = |request: &Request| -> Reply<'static> { Reply::Whole(handler(request)) };
    serve_replies_with_limits(listener, &whole, stop, limits)
}

/// Serves requests as [`serve_with_limits`] does with a handler that answers a [`Reply`]: a
/// [`StreamedResponse`] is sent in chunks as it is produced, holding one chunk at a time.
pub fn serve_replies_with_limits<'h>(
    listener: &TcpListener,
    handler: &(dyn Fn(&Request) -> Reply<'h> + Sync),
    stop: &(dyn Fn() -> bool + Sync),
    limits: &Limits<'_>,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let active = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        while !stop() {
            let (stream, peer) = match eggdb_platform::accept_tcp(listener) {
                Ok(accepted) => accepted,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err() {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                active.fetch_sub(1, Ordering::AcqRel);
                let mut stream = stream;
                let _ = write_response(&mut stream, &Response::text(503, "busy"), false);
                continue;
            }
            let active = &active;
            let spawned = std::thread::Builder::new()
                .name("eggdb-http".to_owned())
                .spawn_scoped(scope, move || {
                    connection::serve_connection(
                        stream,
                        peer.ip(),
                        handler,
                        limits,
                        stop,
                        active,
                    );
                    active.fetch_sub(1, Ordering::AcqRel);
                });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod content_coding_tests;

#[cfg(test)]
mod keep_alive_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    #[test]
    fn a_listener_with_tls_serves_https() {
        use eggdb_tls::self_signed::{SelfSignedCertificateRequest, generate_self_signed_certificate};
        use eggdb_tls::x509::SubjectAlternativeName;
        use std::io::{Read, Write};
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs() as i64);
        let request = SelfSignedCertificateRequest {
            common_name: "localhost".to_owned(),
            subject_alternative_names: vec![SubjectAlternativeName::DnsName("localhost".to_owned())],
            not_before_unix_seconds: now - 86_400,
            not_after_unix_seconds: now + 86_400,
        };
        let (certificate, key) = generate_self_signed_certificate(&request, &mut eggdb_platform::secure_random::fill).unwrap();
        let config = eggdb_tls::ServerConfig::new(vec![certificate], key).unwrap();
        let budget = MemoryBudget::new(1 << 24);
        let limits = Limits::new(1 << 20, &budget).with_tls(Some(&config));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stopped = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| serve_with_limits(&listener, &|_| Response::text(200, "secured"), &|| stopped.load(Ordering::Acquire), &limits));
            let client = eggdb_tls::ClientConfig::new(eggdb_tls::ServerVerification::EncryptionOnly, "localhost");
            let mut secured = eggdb_tls::TlsStream::connect(TcpStream::connect(address).unwrap(), &client).unwrap();
            secured.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
            let mut answer = Vec::new();
            let _ = secured.read_to_end(&mut answer);
            let answer = String::from_utf8_lossy(&answer);
            assert!(answer.starts_with("HTTP/1.1 200") && answer.ends_with("secured"), "{answer}");
            let mut plain = TcpStream::connect(address).unwrap();
            plain.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
            let mut refused = Vec::new();
            let _ = plain.read_to_end(&mut refused);
            assert!(!String::from_utf8_lossy(&refused).contains("secured"), "a plain request gets no answer");
            stopped.store(true, Ordering::Release);
        });
    }

    #[test]
    fn a_body_limit_is_kept_and_a_waiting_client_is_told_to_continue() {
        let mut continued = 0;
        let request = read_request_limited(
            &mut &b"PUT /b/k HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 2000000\r\n\r\n"[..],
            1,
            &mut || continued += 1,
        );
        assert_eq!((request, continued), (Err(HttpError::BodyTooLarge), 0));
        let body = vec![7u8; 2_000_000];
        let mut bytes =
            b"PUT /b/k HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 2000000\r\n\r\n"
                .to_vec();
        let head = bytes.len();
        bytes.extend_from_slice(&body);
        let mut reader = SplitReader {
            bytes: &bytes,
            head,
        };
        let request = read_request_limited(&mut reader, 4_000_000, &mut || continued += 1).unwrap();
        assert_eq!((request.body.len(), continued), (2_000_000, 1));
    }

    struct SplitReader<'b> {
        bytes: &'b [u8],
        head: usize,
    }

    impl Read for SplitReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let limit = if self.head > 0 {
                self.head
            } else {
                self.bytes.len()
            };
            let count = buffer.len().min(limit).min(self.bytes.len());
            buffer[..count].copy_from_slice(&self.bytes[..count]);
            self.bytes = &self.bytes[count..];
            self.head = self.head.saturating_sub(count);
            Ok(count)
        }
    }

    #[test]
    fn a_request_head_parses_and_hostile_ones_are_refused() {
        let request = read_request(
            &mut &b"GET /metrics?x=1 HTTP/1.1\r\nHost: a\r\nAccept: text/plain\r\n\r\nignored"[..],
        )
        .unwrap();
        assert_eq!(
            (request.method.as_str(), request.path.as_str()),
            ("GET", "/metrics")
        );
        assert_eq!(request.query.as_deref(), Some("x=1"));
        assert_eq!(request.header("accept"), Some("text/plain"));
        for (bytes, expected) in [
            (&b"GET / HTTP/1.1\r\nHost: x\r\n"[..], HttpError::Closed),
            (b"GET /\r\n\r\n", HttpError::Malformed),
            (b"GET x HTTP/1.1\r\nHost: x\r\n\r\n", HttpError::Malformed),
            (b"get / HTTP/1.1\r\nHost: x\r\n\r\n", HttpError::Malformed),
            (
                b"GET / HTTP/1.1\r\nHost: x\r\nbad header\r\n\r\n",
                HttpError::Malformed,
            ),
            (
                b"GET / HTTP/1.1\r\nHost: x\r\n\xff: x\r\n\r\n",
                HttpError::Malformed,
            ),
        ] {
            assert_eq!(read_request(&mut &bytes[..]), Err(expected), "{bytes:?}");
        }
        let posted = read_request(
            &mut &b"POST /x HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\n\r\nabcdef"[..],
        )
        .unwrap();
        assert_eq!(posted.body, b"abc");
        for hostile in [
            &b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: x\r\n\r\n"[..],
            b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nContent-Length: 3\r\n\r\n",
        ] {
            assert_eq!(read_request(&mut &hostile[..]), Err(HttpError::Malformed));
        }
        assert_eq!(
            read_request(
                &mut &b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 99999999\r\n\r\n"[..]
            ),
            Err(HttpError::BodyTooLarge)
        );
        let endless = vec![b'a'; MAX_HEAD_BYTES + 10];
        assert_eq!(read_request(&mut &endless[..]), Err(HttpError::TooLarge));
        let mut many = b"GET / HTTP/1.1\r\nHost: x\r\n".to_vec();
        for index in 0..=MAX_HEADERS {
            many.extend_from_slice(format!("h{index}: v\r\n").as_bytes());
        }
        many.extend_from_slice(b"\r\n");
        assert_eq!(read_request(&mut &many[..]), Err(HttpError::TooLarge));
    }

    #[test]
    fn an_http_1_1_request_needs_exactly_one_host_header() {
        for (bytes, expected) in [
            (&b"GET / HTTP/1.1\r\n\r\n"[..], Err(HttpError::InvalidHost)),
            (
                b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n",
                Err(HttpError::InvalidHost),
            ),
            (
                b"GET / HTTP/1.0\r\nHost: a\r\nhost: a\r\n\r\n",
                Err(HttpError::InvalidHost),
            ),
            (
                b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\nhi",
                Err(HttpError::InvalidHost),
            ),
            (b"GET /old HTTP/1.0\r\n\r\n", Ok("/old")),
            (b"GET /empty HTTP/1.1\r\nhOsT:\r\n\r\n", Ok("/empty")),
            (b"GET /one HTTP/1.1\r\nHost: db:9200\r\n\r\n", Ok("/one")),
        ] {
            assert_eq!(
                read_request(&mut &bytes[..]).map(|request| request.path),
                expected.map(str::to_owned),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stopped = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                serve(
                    &listener,
                    &|request| Response::ok("text/plain", request.path.clone()),
                    &|| stopped.load(Ordering::Acquire),
                )
                .unwrap();
            });
            for request in [
                &b"GET /none HTTP/1.1\r\n\r\nGET /after HTTP/1.1\r\nHost: x\r\n\r\n"[..],
                b"GET /two HTTP/1.1\r\nHost: x\r\nHost: y\r\n\r\n",
            ] {
                let mut client = TcpStream::connect(address).unwrap();
                client.write_all(request).unwrap();
                let mut answer = String::new();
                client.read_to_string(&mut answer).unwrap();
                assert!(answer.starts_with("HTTP/1.1 400 "), "{answer}");
                assert!(answer.contains("Connection: close\r\n"), "{answer}");
                assert!(answer.ends_with("exactly one Host header\n"), "{answer}");
            }
            stopped.store(true, Ordering::Release);
        });
    }

    #[test]
    fn responses_carry_their_length_and_head_leaves_the_body_out() {
        let mut out = Vec::new();
        write_response(&mut out, &Response::ok("text/plain", "hi"), false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(text.ends_with("\r\n\r\nhi"));
        let mut head = Vec::new();
        write_response(&mut head, &Response::ok("text/plain", "hi"), true).unwrap();
        assert!(String::from_utf8(head).unwrap().ends_with("\r\n\r\n"));
    }

    #[test]
    fn a_declared_body_past_the_memory_budget_is_refused_before_it_is_read() {
        let budget = MemoryBudget::new(100_000);
        let mut held = budget.empty_reservation();
        let mut continued = 0;
        let refused = read_request_holding(
            &mut &b"PUT /b/k HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 200000\r\n\r\n"[..],
            1 << 20,
            &mut held,
            &mut || continued += 1,
        );
        assert_eq!(
            (refused, continued, budget.used_bytes()),
            (Err(HttpError::OverMemoryBudget), 0, 0)
        );
        let mut bytes = b"PUT /b/k HTTP/1.1\r\nHost: x\r\nContent-Length: 90000\r\n\r\n".to_vec();
        bytes.extend_from_slice(&[1u8; 90_000]);
        let request = read_request_holding(&mut &bytes[..], 1 << 20, &mut held, &mut || {});
        assert_eq!(request.map(|request| request.body.len()), Ok(90_000));
        assert_eq!(budget.used_bytes(), 90_000);
        drop(held);
        assert_eq!(budget.used_bytes(), 0);
    }

    fn exchange(address: std::net::SocketAddr, request: &[u8]) -> String {
        let mut client = TcpStream::connect(address).unwrap();
        let line_end = request.windows(2).position(|pair| pair == b"\r\n").unwrap() + 2;
        client.write_all(&request[..line_end]).unwrap();
        client.write_all(b"Connection: close\r\n").unwrap();
        client.write_all(&request[line_end..]).unwrap();
        let mut answer = Vec::new();
        let _ = client.read_to_end(&mut answer);
        String::from_utf8_lossy(&answer).into_owned()
    }

    #[test]
    fn requests_and_responses_past_the_memory_budget_get_the_listeners_refusal() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stopped = std::sync::atomic::AtomicBool::new(false);
        let budget = MemoryBudget::new(150_000);
        let limits = Limits::new(1 << 20, &budget)
            .answering_over_budget_with(|| Response::text(429, "slow down"));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                serve_with_limits(
                    &listener,
                    &|request| match request.path.as_str() {
                        "/large" => Response::ok("text/plain", vec![b'x'; 200_000]),
                        _ => Response::ok("text/plain", format!("{} bytes", request.body.len())),
                    },
                    &|| stopped.load(Ordering::Acquire),
                    &limits,
                )
                .unwrap();
            });
            let refused = exchange(
                address,
                b"POST /echo HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 300000\r\n\r\n",
            );
            assert!(refused.starts_with("HTTP/1.1 429 "), "{refused}");
            assert!(refused.ends_with("slow down\n"), "{refused}");
            let large = exchange(address, b"GET /large HTTP/1.1\r\nHost: x\r\n\r\n");
            assert!(large.starts_with("HTTP/1.1 429 "), "{large}");
            let mut fitting =
                b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 100000\r\n\r\n".to_vec();
            fitting.extend_from_slice(&[7u8; 100_000]);
            let served = exchange(address, &fitting);
            assert!(served.ends_with("100000 bytes"), "{served}");
            let small = exchange(address, b"GET /small HTTP/1.1\r\nHost: x\r\n\r\n");
            assert!(small.ends_with("0 bytes"), "{small}");
            stopped.store(true, Ordering::Release);
        });
        assert_eq!(budget.used_bytes(), 0);
    }

    #[test]
    fn the_plain_refusal_asks_clients_to_retry() {
        let response = over_memory_budget_response();
        assert_eq!(response.status, 503);
        assert!(
            response
                .headers
                .iter()
                .any(|(name, value)| name == "Retry-After" && value == "1")
        );
    }

    #[test]
    fn a_server_answers_over_tcp_and_stops() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stopped = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                serve(
                    &listener,
                    &|request| {
                        let body = String::from_utf8_lossy(&request.body);
                        let shown = if body.is_empty() {
                            request.path.clone()
                        } else {
                            format!("{} {body}", request.path)
                        };
                        Response::ok("text/plain", shown)
                    },
                    &|| stopped.load(Ordering::Acquire),
                )
                .unwrap();
            });
            let mut client = TcpStream::connect(address).unwrap();
            client
                .write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                .unwrap();
            let mut answer = String::new();
            client.read_to_string(&mut answer).unwrap();
            assert!(answer.ends_with("/hello"), "{answer}");
            let mut posting = TcpStream::connect(address).unwrap();
            posting
                .write_all(b"POST /echo HTTP/1.0\r\nContent-Length: 5\r\n\r\nhello")
                .unwrap();
            let mut echoed = String::new();
            posting.read_to_string(&mut echoed).unwrap();
            assert!(echoed.ends_with("/echo hello"), "{echoed}");
            stopped.store(true, Ordering::Release);
        });
    }
}
