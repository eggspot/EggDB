use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const MAX_STATUS_LINE: usize = 1024;

/// Where a plain `http://` URL points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpTarget {
    /// The host, as it goes in the `Host` header.
    pub host: String,
    /// The port, 80 unless the URL gives one.
    pub port: u16,
    /// The path and query, `/` when the URL has none.
    pub path: String,
}

/// Reads an `http://host[:port][/path]` URL; `https` and anything else are refused.
pub fn parse_http_url(url: &str) -> Result<HttpTarget, String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| format!("{url}: only http:// URLs are supported yet"))?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority.is_empty() || authority.contains('@') || path.chars().any(|character| character.is_control() || character == ' ') {
        return Err(format!("{url} is not a valid http URL"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || authority.starts_with('[') => {
            (host.to_owned(), port.parse::<u16>().map_err(|_| format!("{url} has an invalid port"))?)
        }
        _ => (authority.to_owned(), 80),
    };
    Ok(HttpTarget { host, port, path: path.to_owned() })
}

/// Sends `body` with `POST` to `target` and returns the response's status code, waiting at
/// most `timeout` for each step.
pub fn post(target: &HttpTarget, content_type: &str, body: &[u8], timeout: Duration) -> io::Result<u16> {
    let address = (target.host.trim_start_matches('[').trim_end_matches(']'), target.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the host has no address"))?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nUser-Agent: eggdb\r\n\r\n",
        target.path,
        target.host,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < MAX_STATUS_LINE {
        if stream.read(&mut byte)? == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&line);
    let mut words = text.split_whitespace();
    match (words.next(), words.next().and_then(|code| code.parse::<u16>().ok())) {
        (Some(version), Some(code)) if version.starts_with("HTTP/1.") => Ok(code),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "the response has no HTTP status line")),
    }
}

const MAX_RESPONSE_HEAD: usize = 64 * 1024;

/// A response read whole: its status, its header fields (names in lower case) and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientResponse {
    /// The status code.
    pub status: u16,
    /// The header fields, names in lower case, in order.
    pub headers: Vec<(String, String)>,
    /// The body, its chunked transfer coding removed.
    pub body: Vec<u8>,
}

impl ClientResponse {
    /// The first value of header `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(field, _)| field == name).map(|(_, value)| value.as_str())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

fn dechunk(raw: &[u8], max_body: usize) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut at = 0;
    loop {
        let line_end = raw[at..].windows(2).position(|pair| pair == b"\r\n").ok_or_else(|| invalid("a chunk size line is cut short"))? + at;
        let size_text = std::str::from_utf8(&raw[at..line_end]).map_err(|_| invalid("a chunk size is not text"))?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or_default().trim(), 16).map_err(|_| invalid("a chunk size is not hexadecimal"))?;
        at = line_end + 2;
        if size == 0 {
            return Ok(body);
        }
        if body.len() + size > max_body {
            return Err(invalid("the response body is larger than allowed"));
        }
        body.extend_from_slice(raw.get(at..at + size).ok_or_else(|| invalid("a chunk is cut short"))?);
        at += size + 2;
    }
}

/// The `Host` header `target` is sent with: the host, and the port unless it is 80.
pub fn host_header(target: &HttpTarget) -> String {
    if target.port == 80 { target.host.clone() } else { format!("{}:{}", target.host, target.port) }
}

/// Sends one request to `target`'s host and port (`path_and_query` replaces its path) with
/// `headers` besides `Content-Length` and `Connection` (and `Host`, unless `headers` has one),
/// and reads the whole response,
/// refusing a body over `max_body` bytes and waiting at most `timeout` for each step.
pub fn request(
    target: &HttpTarget,
    method: &str,
    path_and_query: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
    max_body: usize,
) -> io::Result<ClientResponse> {
    let address = (target.host.trim_start_matches('[').trim_end_matches(']'), target.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the host has no address"))?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut head = format!("{method} {path_and_query} HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("host")) {
        head.push_str(&format!("Host: {}\r\n", host_header(target)));
    }
    for (name, value) in headers {
        if name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "a header holds a line break"));
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    let mut received = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let head_end = loop {
        if let Some(at) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break at + 4;
        }
        if received.len() > MAX_RESPONSE_HEAD {
            return Err(invalid("the response head is too long"));
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(invalid("the connection closed before the response head ended"));
        }
        received.extend_from_slice(&chunk[..read]);
    };
    let text = std::str::from_utf8(&received[..head_end]).map_err(|_| invalid("the response head is not text"))?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut words = status_line.split_whitespace();
    let status = match (words.next(), words.next().and_then(|code| code.parse::<u16>().ok())) {
        (Some(version), Some(code)) if version.starts_with("HTTP/1.") => code,
        _ => return Err(invalid("the response has no HTTP status line")),
    };
    let headers: Vec<(String, String)> = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':').map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned())))
        .collect();
    let mut response = ClientResponse { status, headers, body: Vec::new() };
    let mut raw = received[head_end..].to_vec();
    let length = response.header("content-length").map(str::parse::<usize>).transpose().map_err(|_| invalid("the Content-Length is not a number"))?;
    let chunked = response.header("transfer-encoding").is_some_and(|coding| coding.eq_ignore_ascii_case("chunked"));
    if method == "HEAD" || status == 204 || status == 304 {
        return Ok(response);
    }
    loop {
        if length.is_some_and(|length| raw.len() >= length) {
            break;
        }
        if raw.len() > max_body + MAX_RESPONSE_HEAD {
            return Err(invalid("the response body is larger than allowed"));
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
    }
    response.body = match (chunked, length) {
        (true, _) => dechunk(&raw, max_body)?,
        (false, Some(length)) => {
            if length > max_body {
                return Err(invalid("the response body is larger than allowed"));
            }
            raw.get(..length).ok_or_else(|| invalid("the response body is cut short"))?.to_vec()
        }
        (false, None) => {
            if raw.len() > max_body {
                return Err(invalid("the response body is larger than allowed"));
            }
            raw
        }
    };
    Ok(response)
}
