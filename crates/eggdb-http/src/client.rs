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
