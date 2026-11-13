//! A small HTTP/1.1 client of the comparison's own, over one kept-alive connection: a request
//! with an `Authorization` header and a body, and the response's status and body, whether sized
//! by `Content-Length` or chunked.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const MOST_BODY: usize = 256 << 20;

/// One connection to an HTTP server.
pub(crate) struct Connection {
    host: String,
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    authorization: Option<String>,
}

/// One request: method, path with query, extra headers and body.
pub(crate) struct Request<'r> {
    pub(crate) method: &'r str,
    pub(crate) path: &'r str,
    pub(crate) headers: &'r [(&'r str, &'r str)],
    pub(crate) body: &'r [u8],
}

impl Connection {
    /// Connects to `address`, sending `authorization` (a whole header value) with every request.
    pub(crate) fn open(address: &str, authorization: Option<String>) -> Result<Self, String> {
        let stream = TcpStream::connect(address).map_err(|error| format!("cannot reach {address}: {error}"))?;
        stream.set_nodelay(true).map_err(|error| error.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(120))).map_err(|error| error.to_string())?;
        let writer = stream.try_clone().map_err(|error| error.to_string())?;
        Ok(Self { host: address.to_owned(), reader: BufReader::new(stream), writer, authorization })
    }

    /// Sends `request` and returns the status and body of its response.
    pub(crate) fn send(&mut self, request: &Request<'_>) -> Result<(u16, Vec<u8>), String> {
        let mut head = format!("{} {} HTTP/1.1\r\nhost: {}\r\ncontent-length: {}\r\n", request.method, request.path, self.host, request.body.len());
        if let Some(authorization) = &self.authorization {
            head.push_str(&format!("authorization: {authorization}\r\n"));
        }
        for (name, value) in request.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        self.writer.write_all(head.as_bytes()).map_err(|error| error.to_string())?;
        self.writer.write_all(request.body).map_err(|error| error.to_string())?;
        self.response()
    }

    fn line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        if self.reader.read_line(&mut line).map_err(|error| error.to_string())? == 0 {
            return Err("the server closed the connection".to_owned());
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }

    fn response(&mut self) -> Result<(u16, Vec<u8>), String> {
        let status_line = self.line()?;
        let status = status_line.split(' ').nth(1).and_then(|code| code.parse().ok()).ok_or_else(|| format!("a malformed status line: {status_line}"))?;
        let mut length = 0usize;
        let mut chunked = false;
        loop {
            let line = self.line()?;
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                let value = value.trim();
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.parse().map_err(|_| "a malformed content-length")?;
                } else if name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked") {
                    chunked = true;
                }
            }
        }
        let mut body = Vec::new();
        if chunked {
            loop {
                let size = usize::from_str_radix(self.line()?.split(';').next().unwrap_or_default().trim(), 16).map_err(|_| "a malformed chunk size")?;
                if body.len() + size > MOST_BODY {
                    return Err("a response larger than 256 MiB".to_owned());
                }
                let mut chunk = vec![0u8; size + 2];
                self.reader.read_exact(&mut chunk).map_err(|error| error.to_string())?;
                if size == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..size]);
            }
        } else {
            if length > MOST_BODY {
                return Err("a response larger than 256 MiB".to_owned());
            }
            body = vec![0u8; length];
            self.reader.read_exact(&mut body).map_err(|error| error.to_string())?;
        }
        Ok((status, body))
    }
}

/// `text` with the bytes a URL query may not hold percent-encoded.
pub(crate) fn query_encoded(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The value of a Basic `Authorization` header for `user` and `password`.
pub(crate) fn basic(user: &str, password: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = format!("{user}:{password}").into_bytes();
    let mut out = String::from("Basic ");
    for chunk in bytes.chunks(3) {
        let joined = chunk.iter().enumerate().fold(0u32, |joined, (at, byte)| joined | u32::from(*byte) << (16 - 8 * at));
        for at in 0..4 {
            if at <= chunk.len() {
                out.push(ALPHABET[((joined >> (18 - 6 * at)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
