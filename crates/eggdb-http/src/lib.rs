//! A small HTTP/1.1 server written from RFC 9110 and RFC 9112: one request per connection,
//! bodies only with `Content-Length` and at most 1 MiB, with every length and wait bounded so a
//! hostile client cannot hold a thread or memory for long. A minimal client posts to plain
//! `http://` URLs, for webhooks.

mod client;

pub use client::{HttpTarget, parse_http_url, post};

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 100;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const MAX_CONNECTIONS: usize = 16;
const MAX_BODY_BYTES: usize = 1 << 20;

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
    /// The head is not HTTP/1.x.
    Malformed,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Closed => "the connection closed before the request ended",
            Self::TooLarge => "the request head is too large",
            Self::Malformed => "the request is not HTTP/1.1",
        })
    }
}

impl std::error::Error for HttpError {}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        401 => "Unauthorized",
        403 => "Forbidden",
        413 => "Content Too Large",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

fn head_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n").map(|at| at + 4)
}

/// Reads a request from `stream`: its head, at most 16 KiB, and a body of at most 1 MiB given
/// by `Content-Length` (chunked bodies are refused).
pub fn read_request(stream: &mut impl Read) -> Result<Request, HttpError> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    let end = loop {
        if let Some(end) = head_end(&head) {
            break end;
        }
        if head.len() >= MAX_HEAD_BYTES {
            return Err(HttpError::TooLarge);
        }
        let count = stream.read(&mut chunk).map_err(|_| HttpError::Closed)?;
        if count == 0 {
            return Err(HttpError::Closed);
        }
        head.extend_from_slice(&chunk[..count]);
    };
    let text = std::str::from_utf8(&head[..end]).map_err(|_| HttpError::Malformed)?;
    let mut lines = text.split("\r\n");
    let mut parts = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::Malformed);
    };
    if !version.starts_with("HTTP/1.")
        || method.is_empty()
        || !method.bytes().all(|byte| byte.is_ascii_uppercase())
        || !target.starts_with('/')
    {
        return Err(HttpError::Malformed);
    }
    let mut headers = Vec::new();
    for line in lines.filter(|line| !line.is_empty()) {
        if headers.len() >= MAX_HEADERS {
            return Err(HttpError::TooLarge);
        }
        let (name, value) = line.split_once(':').ok_or(HttpError::Malformed)?;
        if name.is_empty() || name.contains([' ', '\t']) {
            return Err(HttpError::Malformed);
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query.to_owned())),
        None => (target, None),
    };
    let mut request = Request {
        method: method.to_owned(),
        path: path.to_owned(),
        query,
        headers,
        body: Vec::new(),
    };
    if request.header("transfer-encoding").is_some() {
        return Err(HttpError::Malformed);
    }
    let length = match request.header("content-length") {
        None => 0,
        Some(text) => text.parse::<usize>().map_err(|_| HttpError::Malformed)?,
    };
    if length > MAX_BODY_BYTES {
        return Err(HttpError::TooLarge);
    }
    let mut body = head[end..].to_vec();
    if body.len() > length {
        body.truncate(length);
    }
    while body.len() < length {
        let count = stream.read(&mut chunk).map_err(|_| HttpError::Closed)?;
        if count == 0 {
            return Err(HttpError::Closed);
        }
        body.extend_from_slice(&chunk[..count.min(length - body.len())]);
    }
    request.body = body;
    Ok(request)
}

/// Writes `response` to `stream`, without its body for a `HEAD` request, and asks the client to
/// close the connection.
pub fn write_response(
    stream: &mut impl Write,
    response: &Response,
    head_only: bool,
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        response.status,
        reason(response.status),
        response.content_type,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if !head_only {
        stream.write_all(&response.body)?;
    }
    stream.flush()
}

fn serve_one(mut stream: TcpStream, handler: &(dyn Fn(&Request) -> Response + Sync)) {
    if stream.set_read_timeout(Some(READ_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(READ_TIMEOUT)).is_err()
    {
        return;
    }
    let response = match read_request(&mut stream) {
        Ok(request) => {
            let response = handler(&request);
            let _ = write_response(&mut stream, &response, request.method == "HEAD");
            return;
        }
        Err(HttpError::TooLarge) => Response::text(431, "the request is too large"),
        Err(HttpError::Malformed) => Response::text(400, "not an HTTP/1.1 request"),
        Err(HttpError::Closed) => return,
    };
    let _ = write_response(&mut stream, &response, false);
}

/// Serves requests arriving on `listener` with `handler`, a thread per connection and at most
/// sixteen at once, until `stop` turns true.
pub fn serve(
    listener: &TcpListener,
    handler: &(dyn Fn(&Request) -> Response + Sync),
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let active = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        while !stop() {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
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
                    serve_one(stream, handler);
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
mod tests {
    use super::*;

    #[test]
    fn a_request_head_parses_and_hostile_ones_are_refused() {
        let request = read_request(
            &mut &b"GET /metrics?x=1 HTTP/1.1\r\nHost: a\r\nAccept: text/plain\r\n\r\nignored"[..],
        )
        .unwrap();
        assert_eq!((request.method.as_str(), request.path.as_str()), ("GET", "/metrics"));
        assert_eq!(request.query.as_deref(), Some("x=1"));
        assert_eq!(request.header("accept"), Some("text/plain"));
        for (bytes, expected) in [
            (&b"GET / HTTP/1.1\r\n"[..], HttpError::Closed),
            (b"GET /\r\n\r\n", HttpError::Malformed),
            (b"GET x HTTP/1.1\r\n\r\n", HttpError::Malformed),
            (b"get / HTTP/1.1\r\n\r\n", HttpError::Malformed),
            (b"GET / HTTP/1.1\r\nbad header\r\n\r\n", HttpError::Malformed),
            (b"GET / HTTP/1.1\r\n\xff: x\r\n\r\n", HttpError::Malformed),
        ] {
            assert_eq!(read_request(&mut &bytes[..]), Err(expected), "{bytes:?}");
        }
        let posted = read_request(&mut &b"POST /x HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcdef"[..]).unwrap();
        assert_eq!(posted.body, b"abc");
        for hostile in [
            &b"POST / HTTP/1.1\r\nContent-Length: x\r\n\r\n"[..],
            b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            assert_eq!(read_request(&mut &hostile[..]), Err(HttpError::Malformed));
        }
        assert_eq!(
            read_request(&mut &b"POST / HTTP/1.1\r\nContent-Length: 99999999\r\n\r\n"[..]),
            Err(HttpError::TooLarge)
        );
        let endless = vec![b'a'; MAX_HEAD_BYTES + 10];
        assert_eq!(read_request(&mut &endless[..]), Err(HttpError::TooLarge));
        let mut many = b"GET / HTTP/1.1\r\n".to_vec();
        for index in 0..=MAX_HEADERS {
            many.extend_from_slice(format!("h{index}: v\r\n").as_bytes());
        }
        many.extend_from_slice(b"\r\n");
        assert_eq!(read_request(&mut &many[..]), Err(HttpError::TooLarge));
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
            client.write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut answer = String::new();
            client.read_to_string(&mut answer).unwrap();
            assert!(answer.ends_with("/hello"), "{answer}");
            let mut posting = TcpStream::connect(address).unwrap();
            posting
                .write_all(b"POST /echo HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello")
                .unwrap();
            let mut echoed = String::new();
            posting.read_to_string(&mut echoed).unwrap();
            assert!(echoed.ends_with("/echo hello"), "{echoed}");
            stopped.store(true, Ordering::Release);
        });
    }
}
