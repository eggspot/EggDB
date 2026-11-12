use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const MAX_STATUS_LINE: usize = 1024;
const MOST_TRUST_FILE_BYTES: u64 = 16 << 20;
const TRUST_VARIABLE: &str = "SSL_CERT_FILE";
const SYSTEM_TRUST_FILES: [&str; 4] = [
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/ssl/cert.pem",
    "/usr/local/etc/openssl/cert.pem",
];

trait Duplex: Read + Write {}

impl<T: Read + Write> Duplex for T {}

fn trust_store() -> io::Result<eggdb_tls::verify::TrustStore> {
    let candidates: Vec<String> = match std::env::var(TRUST_VARIABLE) {
        Ok(path) => vec![path],
        Err(_) => SYSTEM_TRUST_FILES.iter().map(|path| (*path).to_owned()).collect(),
    };
    for path in candidates {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut text = String::new();
        if file.take(MOST_TRUST_FILE_BYTES).read_to_string(&mut text).is_err() {
            continue;
        }
        if let Ok(trust) = eggdb_tls::verify::TrustStore::from_pem(&text) {
            return Ok(trust);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no trusted certificates to check an https:// server with: set {TRUST_VARIABLE} to a PEM file of roots"),
    ))
}

fn connected(target: &HttpTarget, timeout: Duration) -> io::Result<Box<dyn Duplex>> {
    let address = (
        target.host.trim_start_matches('[').trim_end_matches(']'),
        target.port,
    )
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the host has no address"))?;
    let stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    if !target.tls {
        return Ok(Box::new(stream));
    }
    let host = target.host.trim_start_matches('[').trim_end_matches(']');
    let server_name = eggdb_tls::verify::ServerName::parse(host)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let verification = eggdb_tls::ServerVerification::Full {
        trust: trust_store()?,
        server_name,
    };
    let config = eggdb_tls::ClientConfig::new(verification, host);
    let secured = eggdb_tls::TlsStream::connect(stream, &config)
        .map_err(|error| io::Error::new(io::ErrorKind::ConnectionAborted, format!("TLS with {host}: {error}")))?;
    Ok(Box::new(secured))
}

/// Where a plain `http://` URL points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpTarget {
    /// The host, as it goes in the `Host` header.
    pub host: String,
    /// The port, 80 unless the URL gives one.
    pub port: u16,
    /// The path and query, `/` when the URL has none.
    pub path: String,
    /// Whether the URL is `https://`: the connection runs TLS and checks the server's
    /// certificate against the roots in `SSL_CERT_FILE`, or the system's.
    pub tls: bool,
}

/// Reads an `http://host[:port][/path]` or `https://host[:port][/path]` URL; anything else is
/// refused.
pub fn parse_http_url(url: &str) -> Result<HttpTarget, String> {
    let (rest, tls) = match url.strip_prefix("https://") {
        Some(rest) => (rest, true),
        None => (
            url.strip_prefix("http://")
                .ok_or_else(|| format!("{url}: give an http:// or https:// URL"))?,
            false,
        ),
    };
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority.is_empty()
        || authority.contains('@')
        || path
            .chars()
            .any(|character| character.is_control() || character == ' ')
    {
        return Err(format!("{url} is not a valid http URL"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || authority.starts_with('[') => (
            host.to_owned(),
            port.parse::<u16>()
                .map_err(|_| format!("{url} has an invalid port"))?,
        ),
        _ => (authority.to_owned(), if tls { 443 } else { 80 }),
    };
    Ok(HttpTarget {
        host,
        port,
        path: path.to_owned(),
        tls,
    })
}

/// Sends `body` with `POST` to `target` and returns the response's status code, waiting at
/// most `timeout` for each step.
pub fn post(
    target: &HttpTarget,
    content_type: &str,
    body: &[u8],
    timeout: Duration,
) -> io::Result<u16> {
    let mut stream = connected(target, timeout)?;
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
    match (
        words.next(),
        words.next().and_then(|code| code.parse::<u16>().ok()),
    ) {
        (Some(version), Some(code)) if version.starts_with("HTTP/1.") => Ok(code),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the response has no HTTP status line",
        )),
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
        self.headers
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.as_str())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

fn dechunk(raw: &[u8], max_body: usize) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut at = 0;
    loop {
        let line_end = raw
            .get(at..)
            .unwrap_or_default()
            .windows(2)
            .position(|pair| pair == b"\r\n")
            .ok_or_else(|| invalid("a chunk size line is cut short"))?
            + at;
        let size_text = std::str::from_utf8(raw.get(at..line_end).unwrap_or_default())
            .map_err(|_| invalid("a chunk size is not text"))?;
        let size =
            usize::from_str_radix(size_text.split(';').next().unwrap_or_default().trim(), 16)
                .map_err(|_| invalid("a chunk size is not hexadecimal"))?;
        at = line_end + 2;
        if size == 0 {
            return Ok(body);
        }
        if body.len().saturating_add(size) > max_body {
            return Err(invalid("the response body is larger than allowed"));
        }
        body.extend_from_slice(
            raw.get(at..at + size)
                .ok_or_else(|| invalid("a chunk is cut short"))?,
        );
        at += size + 2;
    }
}

/// The `Host` header `target` is sent with: the host, and the port unless it is the scheme's
/// default (80, or 443 for `https://`).
pub fn host_header(target: &HttpTarget) -> String {
    if target.port == if target.tls { 443 } else { 80 } {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    }
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
    let mut stream = connected(target, timeout)?;
    let mut head = format!(
        "{method} {path_and_query} HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        head.push_str(&format!("Host: {}\r\n", host_header(target)));
    }
    for (name, value) in headers {
        if name.contains(['\r', '\n', ':']) || value.contains(['\r', '\n']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a header holds a line break",
            ));
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
            return Err(invalid(
                "the connection closed before the response head ended",
            ));
        }
        received.extend_from_slice(&chunk[..read]);
    };
    let text = std::str::from_utf8(&received[..head_end])
        .map_err(|_| invalid("the response head is not text"))?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut words = status_line.split_whitespace();
    let status = match (
        words.next(),
        words.next().and_then(|code| code.parse::<u16>().ok()),
    ) {
        (Some(version), Some(code)) if version.starts_with("HTTP/1.") => code,
        _ => return Err(invalid("the response has no HTTP status line")),
    };
    let headers: Vec<(String, String)> = lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            line.split_once(':')
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    let mut response = ClientResponse {
        status,
        headers,
        body: Vec::new(),
    };
    let mut raw = received[head_end..].to_vec();
    let length = response
        .header("content-length")
        .map(str::parse::<usize>)
        .transpose()
        .map_err(|_| invalid("the Content-Length is not a number"))?;
    let chunked = response
        .header("transfer-encoding")
        .is_some_and(|coding| coding.eq_ignore_ascii_case("chunked"));
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
            raw.get(..length)
                .ok_or_else(|| invalid("the response body is cut short"))?
                .to_vec()
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
